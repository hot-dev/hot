//! Content-addressed blob reference system for large `Val` payloads.
//!
//! Large `Val::Bytes` / `Val::Str` leaves are spilled to file storage at async
//! persistence boundaries and replaced with compact `::hot::blob/BlobRef`
//! typed maps. Values are rehydrated transparently at execution/read edges
//! (worker event handling, store gets, API run-result reads).
//!
//! Security model: a BlobRef map is untrusted data. Rehydration authorizes by
//! `blob_ref_id` plus the caller's org/env context — hashes, object ids, and
//! storage paths are identifiers, not capabilities. Callers must mask secrets
//! on the `Val` BEFORE spilling so secret bytes never reach blob storage.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use indexmap::IndexMap;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

use crate::db::DatabasePool;
use crate::db::blob::{self as blob_db, BlobDbError, BlobObjectRecord, object_status};
use crate::file_storage::{FileStorage, FileStorageContext};
use crate::val::Val;

/// Typed-map `$type` tag for blob references.
pub const BLOB_REF_TYPE: &str = "::hot::blob/BlobRef";

pub const HASH_ALG_BLAKE3: &str = "blake3";

/// Leaf encoding recorded in the BlobRef so rehydration restores the exact
/// original `Val` variant.
pub const ENCODING_BYTES: &str = "bytes";
pub const ENCODING_STRING: &str = "string";

#[derive(Error, Debug)]
pub enum BlobError {
    #[error("Blob storage error: {0}")]
    Storage(String),
    #[error("Blob database error: {0}")]
    Db(#[from] BlobDbError),
    #[error("Blob ref not found or inactive: {0}")]
    RefNotFound(Uuid),
    #[error("Blob object not available: {0}")]
    ObjectNotAvailable(Uuid),
    #[error("Blob access denied")]
    Unauthorized,
    #[error("Invalid blob ref: {0}")]
    InvalidRef(String),
    #[error("Rehydration budget exceeded: {0}")]
    BudgetExceeded(String),
}

/// Where a spilled value came from; maps to `blob_ref.source_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillSource {
    CallArgs,
    CallReturn,
    CallFlow,
    RunResult,
    RunFailure,
    EventData,
    TaskArgs,
    TaskResult,
    StoreValue,
    StreamPayload,
    Manual,
}

impl SpillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SpillSource::CallArgs => "call_args",
            SpillSource::CallReturn => "call_return",
            SpillSource::CallFlow => "call_flow",
            SpillSource::RunResult => "run_result",
            SpillSource::RunFailure => "run_failure",
            SpillSource::EventData => "event_data",
            SpillSource::TaskArgs => "task_args",
            SpillSource::TaskResult => "task_result",
            SpillSource::StoreValue => "store_value",
            SpillSource::StreamPayload => "stream_payload",
            SpillSource::Manual => "manual",
        }
    }
}

/// Operating mode for the blob subsystem, mirroring `file.mode`.
///
/// - `Disabled`: no BlobStore is constructed; spill, rehydration, downloads,
///   and GC all no-op. Default outside a project.
/// - `Service`: DB-tracked content-addressed storage (`blob_object` /
///   `blob_ref` tables over `FileStorage`) with ref-authorized reads and GC.
///   Default inside a project and forced in managed runtimes.
///
/// A future `direct` (filesystem-only, no DB) mode is intentionally not
/// implemented: BlobRefs are only produced when persisting to DB-backed
/// payloads, so there is no DB-less producer today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlobMode {
    #[default]
    Disabled,
    Service,
}

impl std::str::FromStr for BlobMode {
    type Err = String;

    /// Parse from config. Empty/missing means disabled (the CLI resolves the
    /// in-project/out-of-project default before config reaches this point).
    /// Unknown values are rejected so a typo cannot silently enable spill.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" | "disabled" => Ok(BlobMode::Disabled),
            "service" => Ok(BlobMode::Service),
            other => Err(format!(
                "Unknown blob.mode '{}'. Available options: disabled, service",
                other
            )),
        }
    }
}

/// Blob configuration, read from the `blob` section of hot config.
#[derive(Debug, Clone)]
pub struct BlobConfig {
    pub mode: BlobMode,
    pub spill_threshold_bytes: usize,
    pub preview_max_bytes: usize,
    pub spill_bytes: bool,
    pub spill_strings: bool,
    pub spill_calls: bool,
    pub spill_runs: bool,
    pub spill_events: bool,
    pub storage_prefix: String,
    pub rehydrate_max_refs: usize,
    pub rehydrate_max_total_bytes: usize,
}

impl Default for BlobConfig {
    fn default() -> Self {
        Self {
            mode: BlobMode::Disabled,
            spill_threshold_bytes: 65536,
            preview_max_bytes: 256,
            spill_bytes: true,
            spill_strings: true,
            spill_calls: true,
            spill_runs: false,
            spill_events: false,
            storage_prefix: "__hot/blobs".to_string(),
            rehydrate_max_refs: 1000,
            rehydrate_max_total_bytes: 512 * 1024 * 1024,
        }
    }
}

impl BlobConfig {
    /// Parse from the hot config root (the same Val that `file.*` config is
    /// read from). Missing keys fall back to conservative defaults.
    pub fn from_conf(conf: &Val) -> Self {
        let d = Self::default();
        let get_bool = |path: &str, default: bool| match conf.get(path) {
            Some(Val::Bool(b)) => b,
            _ => default,
        };
        let get_usize = |path: &str, default: usize| match conf.get(path) {
            Some(Val::Int(i)) if i >= 0 => i as usize,
            _ => default,
        };
        let storage_prefix = match conf.get("blob.storage.prefix") {
            Some(Val::Str(s)) if !s.is_empty() => (*s).to_string(),
            _ => d.storage_prefix.clone(),
        };
        // Unknown modes are rejected loudly and treated as disabled: a typo in
        // blob.mode must not silently enable spill, and the error makes the
        // misconfiguration visible.
        let mode_str = conf.get_str_or_default("blob.mode", "");
        let mode = match mode_str.parse::<BlobMode>() {
            Ok(mode) => mode,
            Err(e) => {
                tracing::error!("{}; blob storage disabled", e);
                BlobMode::Disabled
            }
        };
        Self {
            mode,
            spill_threshold_bytes: get_usize("blob.spill.threshold-bytes", d.spill_threshold_bytes),
            preview_max_bytes: get_usize("blob.preview.max-bytes", d.preview_max_bytes),
            spill_bytes: get_bool("blob.spill.bytes", d.spill_bytes),
            spill_strings: get_bool("blob.spill.strings", d.spill_strings),
            spill_calls: get_bool("blob.spill.calls", d.spill_calls),
            spill_runs: get_bool("blob.spill.runs", d.spill_runs),
            spill_events: get_bool("blob.spill.events", d.spill_events),
            storage_prefix,
            rehydrate_max_refs: get_usize("blob.rehydrate.max-refs", d.rehydrate_max_refs),
            rehydrate_max_total_bytes: get_usize(
                "blob.rehydrate.max-total-bytes",
                d.rehydrate_max_total_bytes,
            ),
        }
    }

    /// Whether the blob subsystem is active at all.
    pub fn enabled(&self) -> bool {
        self.mode == BlobMode::Service
    }

    /// Whether spill is enabled for a given source kind.
    pub fn spill_enabled_for(&self, source: SpillSource) -> bool {
        if !self.enabled() {
            return false;
        }
        match source {
            SpillSource::CallArgs | SpillSource::CallReturn | SpillSource::CallFlow => {
                self.spill_calls
            }
            SpillSource::RunResult | SpillSource::RunFailure => self.spill_runs,
            SpillSource::EventData => self.spill_events,
            _ => true,
        }
    }
}

/// Tenant scope for a spill or rehydrate operation.
#[derive(Debug, Clone, Copy)]
pub struct BlobScope {
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
}

/// Parsed `::hot::blob/BlobRef` typed map.
#[derive(Debug, Clone)]
pub struct BlobRefVal {
    pub id: Uuid,
    pub object_id: Uuid,
    pub hash_alg: String,
    pub hash: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub preview: Option<String>,
    pub encoding: String,
}

impl BlobRefVal {
    /// Build the `::hot::blob/BlobRef` typed map with kebab-case keys.
    pub fn to_val(&self) -> Val {
        let mut inner = IndexMap::new();
        inner.insert(Val::from("id"), Val::from(self.id.to_string()));
        inner.insert(
            Val::from("object-id"),
            Val::from(self.object_id.to_string()),
        );
        inner.insert(Val::from("hash-alg"), Val::from(self.hash_alg.clone()));
        inner.insert(Val::from("hash"), Val::from(self.hash.clone()));
        inner.insert(Val::from("size"), Val::Int(self.size));
        if let Some(ct) = &self.content_type {
            inner.insert(Val::from("content-type"), Val::from(ct.clone()));
        }
        if let Some(p) = &self.preview {
            inner.insert(Val::from("preview"), Val::from(p.clone()));
        }
        inner.insert(Val::from("encoding"), Val::from(self.encoding.clone()));

        let mut map = IndexMap::new();
        map.insert(Val::from("$type"), Val::from(BLOB_REF_TYPE));
        map.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        Val::Map(Box::new(map))
    }

    /// Parse a Val that may be a BlobRef typed map. Returns None if the value
    /// is not a BlobRef at all; Err if it claims to be one but is malformed.
    pub fn from_val(val: &Val) -> Result<Option<Self>, BlobError> {
        let Val::Map(m) = val else { return Ok(None) };
        match m.get(&Val::from("$type")) {
            Some(Val::Str(t)) if &**t == BLOB_REF_TYPE => {}
            _ => return Ok(None),
        }
        let inner = match m.get(&Val::from("$val")) {
            Some(Val::Map(inner)) => inner,
            _ => return Err(BlobError::InvalidRef("missing $val map".to_string())),
        };
        let get_str = |key: &str| -> Option<String> {
            match inner.get(&Val::from(key)) {
                Some(Val::Str(s)) => Some((**s).to_string()),
                _ => None,
            }
        };
        let id = get_str("id")
            .and_then(|s| Uuid::parse_str(&s).ok())
            .ok_or_else(|| BlobError::InvalidRef("missing or invalid id".to_string()))?;
        let object_id = get_str("object-id")
            .and_then(|s| Uuid::parse_str(&s).ok())
            .ok_or_else(|| BlobError::InvalidRef("missing or invalid object-id".to_string()))?;
        let size = match inner.get(&Val::from("size")) {
            Some(Val::Int(i)) => *i,
            _ => return Err(BlobError::InvalidRef("missing size".to_string())),
        };
        let encoding = get_str("encoding")
            .ok_or_else(|| BlobError::InvalidRef("missing encoding".to_string()))?;
        Ok(Some(Self {
            id,
            object_id,
            hash_alg: get_str("hash-alg").unwrap_or_else(|| HASH_ALG_BLAKE3.to_string()),
            hash: get_str("hash").unwrap_or_default(),
            size,
            content_type: get_str("content-type"),
            preview: get_str("preview"),
            encoding,
        }))
    }
}

/// True if the value is a `::hot::blob/BlobRef` typed map.
pub fn is_blob_ref_val(val: &Val) -> bool {
    let Val::Map(m) = val else { return false };
    matches!(m.get(&Val::from("$type")), Some(Val::Str(t)) if &**t == BLOB_REF_TYPE)
}

/// True if the value contains any BlobRef anywhere in its tree.
pub fn contains_blob_ref(val: &Val) -> bool {
    match val {
        Val::Map(m) => {
            if is_blob_ref_val(val) {
                return true;
            }
            m.iter()
                .any(|(k, v)| contains_blob_ref(k) || contains_blob_ref(v))
        }
        Val::Vec(v) => v.iter().any(contains_blob_ref),
        _ => false,
    }
}

/// True if a raw JSON payload contains a BlobRef typed map anywhere. Cheap
/// pre-check for read boundaries that hold `serde_json::Value` and only pay
/// for Val conversion + rehydration when refs are actually present.
pub fn json_contains_blob_ref(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if matches!(map.get("$type"), Some(serde_json::Value::String(t)) if t == BLOB_REF_TYPE)
            {
                return true;
            }
            map.values().any(json_contains_blob_ref)
        }
        serde_json::Value::Array(items) => items.iter().any(json_contains_blob_ref),
        _ => false,
    }
}

/// Rough in-memory payload size of a Val tree; used for spill decisions and
/// metrics, not exact accounting.
pub fn estimate_val_size(val: &Val) -> usize {
    match val {
        Val::Bytes(b) => b.len(),
        Val::Str(s) => s.len(),
        Val::Vec(v) => 8 + v.iter().map(estimate_val_size).sum::<usize>(),
        Val::Map(m) => {
            8 + m
                .iter()
                .map(|(k, v)| estimate_val_size(k) + estimate_val_size(v))
                .sum::<usize>()
        }
        _ => 8,
    }
}

/// Rehydration budget limits, enforced per top-level rehydrate call.
#[derive(Debug, Clone, Copy)]
pub struct RehydrateBudget {
    pub max_refs: usize,
    pub max_total_bytes: usize,
}

impl RehydrateBudget {
    pub fn from_config(config: &BlobConfig) -> Self {
        Self {
            max_refs: config.rehydrate_max_refs,
            max_total_bytes: config.rehydrate_max_total_bytes,
        }
    }
}

/// Shared blob store: content-addressed writes/reads over `FileStorage` plus
/// `blob_object`/`blob_ref` tracking.
pub struct BlobStore {
    db: Arc<DatabasePool>,
    storage: Arc<dyn FileStorage>,
    config: BlobConfig,
}

struct SpillStats {
    spilled_leaves: usize,
    spilled_bytes: usize,
    json_paths: Vec<String>,
}

impl BlobStore {
    pub fn new(db: Arc<DatabasePool>, storage: Arc<dyn FileStorage>, config: BlobConfig) -> Self {
        Self {
            db,
            storage,
            config,
        }
    }

    pub fn config(&self) -> &BlobConfig {
        &self.config
    }

    pub fn db(&self) -> &Arc<DatabasePool> {
        &self.db
    }

    /// FileStorageContext for internal blob byte IO. user_id is nil because
    /// blob bytes never create user-facing file rows.
    fn storage_ctx(&self, scope: &BlobScope) -> FileStorageContext {
        FileStorageContext {
            db: self.db.clone(),
            org_id: scope.org_id,
            env_id: scope.env_id,
            user_id: Uuid::nil(),
            run_id: scope.run_id,
            file_max_bytes_conf: None,
        }
    }

    /// Deterministic org/env-relative storage key for a content hash.
    fn storage_key(&self, hash: &str) -> String {
        let prefix = &self.config.storage_prefix;
        let shard = &hash[..2.min(hash.len())];
        format!("{prefix}/{HASH_ALG_BLAKE3}/{shard}/{hash}")
    }

    /// Spill all large leaves in `val`, replacing them with BlobRef maps.
    /// The caller MUST have already masked secrets in `val`.
    /// Returns the (possibly modified) value.
    pub async fn spill_large_val(
        &self,
        mut val: Val,
        scope: BlobScope,
        source: SpillSource,
        source_id: Option<&str>,
    ) -> Result<Val, BlobError> {
        if !self.config.spill_enabled_for(source) {
            return Ok(val);
        }
        let mut stats = SpillStats {
            spilled_leaves: 0,
            spilled_bytes: 0,
            json_paths: Vec::new(),
        };
        let mut path = String::from("$");
        self.spill_in_place(&mut val, &scope, source, source_id, &mut path, &mut stats)
            .await?;
        if stats.spilled_leaves > 0 {
            tracing::debug!(
                source = source.as_str(),
                leaves = stats.spilled_leaves,
                bytes = stats.spilled_bytes,
                "spilled large val leaves to blob storage"
            );
        }
        Ok(val)
    }

    fn spill_in_place<'a>(
        &'a self,
        val: &'a mut Val,
        scope: &'a BlobScope,
        source: SpillSource,
        source_id: Option<&'a str>,
        path: &'a mut String,
        stats: &'a mut SpillStats,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), BlobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let threshold = self.config.spill_threshold_bytes;
            match val {
                Val::Bytes(bytes) if self.config.spill_bytes && bytes.len() >= threshold => {
                    let bytes = std::mem::take(bytes);
                    let blob_ref = self
                        .spill_leaf(
                            &bytes,
                            ENCODING_BYTES,
                            scope,
                            source,
                            source_id,
                            path,
                            stats,
                        )
                        .await?;
                    *val = blob_ref;
                }
                Val::Str(s) if self.config.spill_strings && s.len() >= threshold => {
                    let text: Arc<str> = std::mem::replace(s, Arc::from(""));
                    let blob_ref = self
                        .spill_leaf(
                            text.as_bytes(),
                            ENCODING_STRING,
                            scope,
                            source,
                            source_id,
                            path,
                            stats,
                        )
                        .await?;
                    *val = blob_ref;
                }
                Val::Vec(items) => {
                    for (i, item) in items.iter_mut().enumerate() {
                        let saved_len = path.len();
                        path.push_str(&format!("[{i}]"));
                        self.spill_in_place(item, scope, source, source_id, path, stats)
                            .await?;
                        path.truncate(saved_len);
                    }
                }
                Val::Map(m) => {
                    // Never descend into an existing BlobRef.
                    let is_ref = matches!(
                        m.get(&Val::from("$type")),
                        Some(Val::Str(t)) if &**t == BLOB_REF_TYPE
                    );
                    if is_ref {
                        return Ok(());
                    }
                    // Spill values only; map keys keep their semantics.
                    for (k, v) in m.iter_mut() {
                        let saved_len = path.len();
                        match k {
                            Val::Str(key) => path.push_str(&format!(".{key}")),
                            _ => path.push_str(".?"),
                        }
                        self.spill_in_place(v, scope, source, source_id, path, stats)
                            .await?;
                        path.truncate(saved_len);
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }

    /// Content-addressed write of one leaf; returns the BlobRef typed map.
    #[allow(clippy::too_many_arguments)]
    async fn spill_leaf(
        &self,
        bytes: &[u8],
        encoding: &str,
        scope: &BlobScope,
        source: SpillSource,
        source_id: Option<&str>,
        path: &str,
        stats: &mut SpillStats,
    ) -> Result<Val, BlobError> {
        let content_type = match encoding {
            ENCODING_STRING => "text/plain",
            _ => "application/octet-stream",
        };
        let object = self.put_object(bytes, Some(content_type), scope).await?;

        // Cap diagnostics paths.
        if stats.json_paths.len() < 20 {
            stats.json_paths.push(path.to_string());
        }
        let json_paths = JsonValue::Array(
            stats
                .json_paths
                .iter()
                .map(|p| JsonValue::String(p.clone()))
                .collect(),
        );

        let blob_ref = blob_db::insert_ref(
            &self.db,
            object.blob_object_id,
            scope.org_id,
            scope.env_id,
            source.as_str(),
            source_id,
            Some(&json_paths),
            scope.run_id,
        )
        .await?;

        stats.spilled_leaves += 1;
        stats.spilled_bytes += bytes.len();

        let preview = self.make_preview(bytes, encoding);
        Ok(BlobRefVal {
            id: blob_ref.blob_ref_id,
            object_id: object.blob_object_id,
            hash_alg: object.hash_alg,
            hash: object.hash,
            size: object.size,
            content_type: object.content_type,
            preview,
            encoding: encoding.to_string(),
        }
        .to_val())
    }

    fn make_preview(&self, bytes: &[u8], encoding: &str) -> Option<String> {
        let n = self.config.preview_max_bytes;
        if n == 0 {
            return None;
        }
        match encoding {
            ENCODING_STRING => {
                let text = std::str::from_utf8(bytes).ok()?;
                let mut end = n.min(text.len());
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                Some(text[..end].to_string())
            }
            _ => Some(BASE64.encode(&bytes[..n.min(bytes.len())])),
        }
    }

    /// Content-addressed object write: dedupe by hash, write bytes through
    /// file storage, and mark the object available.
    ///
    /// Write order is crash-safe: pending row -> storage write -> available.
    /// Orphans (pending rows without bytes, or bytes without refs) are cleaned
    /// up by GC after the grace window.
    pub async fn put_object(
        &self,
        bytes: &[u8],
        content_type: Option<&str>,
        scope: &BlobScope,
    ) -> Result<BlobObjectRecord, BlobError> {
        let hash = blake3::hash(bytes).to_hex().to_string();

        // Dedupe hit: touch last_referenced_at BEFORE the caller inserts a ref
        // so GC's grace window cannot race a concurrent delete.
        if let Some(existing) = blob_db::get_object_by_hash(
            &self.db,
            scope.org_id,
            scope.env_id,
            HASH_ALG_BLAKE3,
            &hash,
        )
        .await?
            && existing.status == object_status::AVAILABLE
        {
            blob_db::touch_object(&self.db, existing.blob_object_id).await?;
            return Ok(existing);
        }

        let storage_key = self.storage_key(&hash);
        let ctx = self.storage_ctx(scope);

        // A stale pending row from a crashed writer may exist; reuse it and
        // retry the storage write rather than failing on the unique index.
        let pending = match blob_db::get_object_by_hash(
            &self.db,
            scope.org_id,
            scope.env_id,
            HASH_ALG_BLAKE3,
            &hash,
        )
        .await?
        {
            Some(existing) => existing,
            None => {
                blob_db::insert_pending_object(
                    &self.db,
                    scope.org_id,
                    scope.env_id,
                    HASH_ALG_BLAKE3,
                    &hash,
                    bytes.len() as i64,
                    content_type,
                    self.storage.storage_type(),
                    &storage_key,
                )
                .await?
            }
        };

        self.storage
            .write_blob_bytes(&storage_key, bytes, content_type, &ctx)
            .await
            .map_err(BlobError::Storage)?;

        blob_db::set_object_status(&self.db, pending.blob_object_id, object_status::AVAILABLE)
            .await?;
        blob_db::touch_object(&self.db, pending.blob_object_id).await?;

        Ok(BlobObjectRecord {
            status: object_status::AVAILABLE.to_string(),
            ..pending
        })
    }

    /// Read blob bytes for an authorized ref. Authorization is by ref id and
    /// tenant scope, never by hash or storage path.
    pub async fn read_ref_bytes(
        &self,
        blob_ref_id: Uuid,
        scope: &BlobScope,
    ) -> Result<(Vec<u8>, BlobObjectRecord), BlobError> {
        let blob_ref = blob_db::get_ref_by_id(&self.db, blob_ref_id)
            .await?
            .ok_or(BlobError::RefNotFound(blob_ref_id))?;
        if !blob_ref.active {
            return Err(BlobError::RefNotFound(blob_ref_id));
        }
        if blob_ref.org_id != scope.org_id {
            return Err(BlobError::Unauthorized);
        }
        // Env check: an env-scoped caller may read org-wide refs (env_id NULL)
        // and refs in its own env, never another env's refs.
        if let Some(ref_env) = blob_ref.env_id
            && scope.env_id != Some(ref_env)
        {
            return Err(BlobError::Unauthorized);
        }

        let object = blob_db::get_object_by_id(&self.db, blob_ref.blob_object_id)
            .await?
            .ok_or(BlobError::ObjectNotAvailable(blob_ref.blob_object_id))?;
        if object.status != object_status::AVAILABLE {
            return Err(BlobError::ObjectNotAvailable(object.blob_object_id));
        }

        // Read using the object's own scope: the object may be org-wide while
        // the caller is env-scoped.
        let object_scope = BlobScope {
            org_id: object.org_id,
            env_id: object.env_id,
            run_id: None,
        };
        let ctx = self.storage_ctx(&object_scope);
        let key = self.storage_key(&object.hash);
        let bytes = self
            .storage
            .read_blob_bytes(&key, &ctx)
            .await
            .map_err(BlobError::Storage)?;

        Ok((bytes, object))
    }

    /// Replace every BlobRef map in `val` with its original leaf value.
    /// Fails closed: unknown refs, cross-tenant refs, unavailable objects,
    /// and budget overruns are errors.
    pub async fn rehydrate_blob_refs(
        &self,
        mut val: Val,
        scope: BlobScope,
        budget: RehydrateBudget,
    ) -> Result<Val, BlobError> {
        let mut used = RehydrateUsage::default();
        self.rehydrate_in_place(&mut val, &scope, &budget, &mut used)
            .await?;
        Ok(val)
    }

    fn rehydrate_in_place<'a>(
        &'a self,
        val: &'a mut Val,
        scope: &'a BlobScope,
        budget: &'a RehydrateBudget,
        used: &'a mut RehydrateUsage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), BlobError>> + Send + 'a>>
    {
        Box::pin(async move {
            if is_blob_ref_val(val) {
                let parsed = BlobRefVal::from_val(val)?
                    .ok_or_else(|| BlobError::InvalidRef("not a blob ref".to_string()))?;

                used.refs += 1;
                if used.refs > budget.max_refs {
                    return Err(BlobError::BudgetExceeded(format!(
                        "more than {} blob refs",
                        budget.max_refs
                    )));
                }

                let (bytes, _object) = self.read_ref_bytes(parsed.id, scope).await?;
                used.bytes += bytes.len();
                if used.bytes > budget.max_total_bytes {
                    return Err(BlobError::BudgetExceeded(format!(
                        "more than {} rehydrated bytes",
                        budget.max_total_bytes
                    )));
                }

                *val = match parsed.encoding.as_str() {
                    ENCODING_STRING => {
                        let text = String::from_utf8(bytes).map_err(|_| {
                            BlobError::InvalidRef("string blob is not valid UTF-8".to_string())
                        })?;
                        Val::from(text)
                    }
                    _ => Val::Bytes(bytes),
                };
                return Ok(());
            }
            match val {
                Val::Vec(items) => {
                    for item in items.iter_mut() {
                        self.rehydrate_in_place(item, scope, budget, used).await?;
                    }
                }
                Val::Map(m) => {
                    for (_k, v) in m.iter_mut() {
                        self.rehydrate_in_place(v, scope, budget, used).await?;
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }

    /// JSON-level rehydration for read boundaries that hold persisted
    /// `serde_json::Value` payloads (API responses, UI reads). Round-trips
    /// through `Val` so spilled leaves are restored with the same JSON shape
    /// they would have had inline.
    pub async fn rehydrate_json(
        &self,
        value: serde_json::Value,
        scope: BlobScope,
        budget: RehydrateBudget,
    ) -> Result<serde_json::Value, BlobError> {
        let val: Val = serde_json::from_value(value)
            .map_err(|e| BlobError::InvalidRef(format!("payload is not a valid Val: {e}")))?;
        let rehydrated = self.rehydrate_blob_refs(val, scope, budget).await?;
        serde_json::to_value(&rehydrated)
            .map_err(|e| BlobError::InvalidRef(format!("rehydrated Val is not serializable: {e}")))
    }

    /// Physically delete GC candidate objects: no active refs, past the grace
    /// window. Deletes storage bytes first, then marks the row deleted.
    /// Returns the number of objects deleted.
    pub async fn gc_objects(
        &self,
        grace_cutoff: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<usize, BlobError> {
        let candidates = blob_db::gc_candidate_objects(&self.db, grace_cutoff, limit).await?;
        let mut deleted = 0;
        for object in candidates {
            // Re-check active refs right before delete: a dedupe hit may have
            // touched + ref'd the object after candidate selection.
            if blob_db::count_active_refs(&self.db, object.blob_object_id).await? > 0 {
                continue;
            }
            let refreshed = blob_db::get_object_by_id(&self.db, object.blob_object_id).await?;
            if let Some(refreshed) = &refreshed
                && refreshed.last_referenced_at >= grace_cutoff
            {
                continue;
            }

            blob_db::set_object_status(
                &self.db,
                object.blob_object_id,
                object_status::DELETE_PENDING,
            )
            .await?;

            let object_scope = BlobScope {
                org_id: object.org_id,
                env_id: object.env_id,
                run_id: None,
            };
            let ctx = self.storage_ctx(&object_scope);
            let key = self.storage_key(&object.hash);
            if let Err(e) = self.storage.delete_blob_bytes(&key, &ctx).await {
                tracing::warn!(
                    blob_object_id = %object.blob_object_id,
                    error = %e,
                    "failed to delete blob bytes; leaving object delete_pending for retry"
                );
                continue;
            }

            blob_db::set_object_status(&self.db, object.blob_object_id, object_status::DELETED)
                .await?;
            deleted += 1;
        }
        Ok(deleted)
    }
}

#[derive(Default)]
struct RehydrateUsage {
    refs: usize,
    bytes: usize,
}

/// Build a shared BlobStore from hot config if blob storage is active.
/// Returns None when `blob.mode` is not `service` or storage cannot be built,
/// so callers can plumb `Option<Arc<BlobStore>>` and no-op when disabled.
pub async fn blob_store_from_conf(db: Arc<DatabasePool>, conf: &Val) -> Option<Arc<BlobStore>> {
    let config = BlobConfig::from_conf(conf);
    if !config.enabled() {
        return None;
    }
    match crate::file_storage::file_storage_from_config(conf).await {
        Ok(storage) => Some(Arc::new(BlobStore::new(db, Arc::from(storage), config))),
        Err(e) => {
            tracing::warn!(error = %e, "blob spill enabled but file storage init failed; disabling blob spill");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_storage::LocalFileStorage;
    use crate::val;

    fn test_config() -> BlobConfig {
        BlobConfig {
            mode: BlobMode::Service,
            spill_threshold_bytes: 1024,
            spill_runs: true,
            spill_events: true,
            ..BlobConfig::default()
        }
    }

    async fn test_store() -> (BlobStore, BlobScope, tempfile::TempDir) {
        let db = Arc::new(crate::db::test_db().await);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(LocalFileStorage::new(temp_dir.path().to_path_buf()));
        let store = BlobStore::new(db, storage, test_config());
        let scope = BlobScope {
            org_id: Uuid::now_v7(),
            env_id: Some(Uuid::now_v7()),
            run_id: None,
        };
        (store, scope, temp_dir)
    }

    fn large_bytes(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn test_blob_ref_val_round_trip() {
        let original = BlobRefVal {
            id: Uuid::now_v7(),
            object_id: Uuid::now_v7(),
            hash_alg: HASH_ALG_BLAKE3.to_string(),
            hash: "abc123".to_string(),
            size: 5242880,
            content_type: Some("application/octet-stream".to_string()),
            preview: Some("cHJldmlldw==".to_string()),
            encoding: ENCODING_BYTES.to_string(),
        };
        let val = original.to_val();
        assert!(is_blob_ref_val(&val));
        assert!(contains_blob_ref(&val));

        let parsed = BlobRefVal::from_val(&val).unwrap().unwrap();
        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.object_id, original.object_id);
        assert_eq!(parsed.hash, original.hash);
        assert_eq!(parsed.size, original.size);
        assert_eq!(parsed.content_type, original.content_type);
        assert_eq!(parsed.preview, original.preview);
        assert_eq!(parsed.encoding, original.encoding);
    }

    #[test]
    fn test_blob_ref_json_wire_format() {
        let blob_ref = BlobRefVal {
            id: Uuid::nil(),
            object_id: Uuid::nil(),
            hash_alg: HASH_ALG_BLAKE3.to_string(),
            hash: "deadbeef".to_string(),
            size: 42,
            content_type: None,
            preview: None,
            encoding: ENCODING_BYTES.to_string(),
        };
        let json = serde_json::to_value(blob_ref.to_val()).unwrap();
        assert_eq!(json["$type"], BLOB_REF_TYPE);
        assert_eq!(json["$val"]["hash-alg"], "blake3");
        assert_eq!(json["$val"]["hash"], "deadbeef");
        assert_eq!(json["$val"]["size"], 42);
        assert_eq!(json["$val"]["encoding"], "bytes");
    }

    #[test]
    fn test_non_blob_ref_vals() {
        assert!(!is_blob_ref_val(&val!({"a": 1})));
        assert!(!is_blob_ref_val(&Val::Int(5)));
        assert!(!contains_blob_ref(&val!({"nested": {"x": [1, 2]}})));
        assert!(BlobRefVal::from_val(&val!({"a": 1})).unwrap().is_none());
    }

    #[test]
    fn test_forged_blob_ref_map_is_malformed() {
        // Claims the type but has no valid fields: must be an error, not None.
        let mut m = IndexMap::new();
        m.insert(Val::from("$type"), Val::from(BLOB_REF_TYPE));
        m.insert(Val::from("$val"), val!({"id": "not-a-uuid"}));
        let forged = Val::Map(Box::new(m));
        assert!(BlobRefVal::from_val(&forged).is_err());
    }

    #[test]
    fn test_json_contains_blob_ref() {
        let plain = serde_json::json!({"a": [1, {"b": "x"}]});
        assert!(!json_contains_blob_ref(&plain));

        let with_ref = serde_json::json!({
            "outer": [{
                "$type": BLOB_REF_TYPE,
                "$val": {"id": "x"},
            }],
        });
        assert!(json_contains_blob_ref(&with_ref));
    }

    #[tokio::test]
    async fn test_rehydrate_json_round_trip() {
        let (store, scope, _tmp) = test_store().await;
        let text = "j".repeat(4096);
        let original = val!({"body": Val::from(text.clone()), "n": 1});

        let spilled = store
            .spill_large_val(original.clone(), scope, SpillSource::RunResult, Some("r1"))
            .await
            .unwrap();
        let spilled_json = serde_json::to_value(&spilled).unwrap();
        assert!(json_contains_blob_ref(&spilled_json));

        let rehydrated_json = store
            .rehydrate_json(
                spilled_json,
                scope,
                RehydrateBudget::from_config(store.config()),
            )
            .await
            .unwrap();
        assert!(!json_contains_blob_ref(&rehydrated_json));
        assert_eq!(rehydrated_json, serde_json::to_value(&original).unwrap());
        assert_eq!(rehydrated_json["body"], serde_json::json!(text));
    }

    #[test]
    fn test_estimate_val_size() {
        assert_eq!(estimate_val_size(&Val::Bytes(vec![0u8; 100])), 100);
        assert_eq!(estimate_val_size(&Val::from("hello")), 5);
        assert!(estimate_val_size(&val!({"k": [1, 2, 3]})) > 8);
    }

    #[tokio::test]
    async fn test_spill_below_threshold_stays_inline() {
        let (store, scope, _tmp) = test_store().await;
        let small = val!({
            "data": Val::Bytes(vec![1u8; 100]),
            "text": "short",
        });
        let result = store
            .spill_large_val(small.clone(), scope, SpillSource::CallArgs, Some("c1"))
            .await
            .unwrap();
        assert_eq!(result, small);
        assert!(!contains_blob_ref(&result));
    }

    #[tokio::test]
    async fn test_spill_and_rehydrate_bytes_round_trip() {
        let (store, scope, _tmp) = test_store().await;
        let payload = large_bytes(7, 4096);
        let original = val!({
            "meta": {"name": "x"},
            "data": Val::Bytes(payload.clone()),
        });

        let spilled = store
            .spill_large_val(original.clone(), scope, SpillSource::CallArgs, Some("c1"))
            .await
            .unwrap();
        assert!(contains_blob_ref(&spilled));
        // Small siblings unchanged.
        assert_eq!(spilled.get("meta.name"), Some(Val::from("x")));
        // The large leaf is now a compact ref.
        assert!(estimate_val_size(&spilled) < 2048);

        let rehydrated = store
            .rehydrate_blob_refs(spilled, scope, RehydrateBudget::from_config(store.config()))
            .await
            .unwrap();
        assert_eq!(rehydrated, original);
    }

    #[tokio::test]
    async fn test_spill_and_rehydrate_string_round_trip() {
        let (store, scope, _tmp) = test_store().await;
        let text = "héllo wörld ".repeat(500);
        let original = val!({"body": Val::from(text.clone())});

        let spilled = store
            .spill_large_val(original.clone(), scope, SpillSource::EventData, Some("e1"))
            .await
            .unwrap();
        assert!(contains_blob_ref(&spilled));

        // String previews must be valid UTF-8 prefixes.
        let ref_val = spilled.get("body").unwrap();
        let parsed = BlobRefVal::from_val(&ref_val).unwrap().unwrap();
        assert_eq!(parsed.encoding, ENCODING_STRING);
        assert!(text.starts_with(parsed.preview.as_deref().unwrap()));

        let rehydrated = store
            .rehydrate_blob_refs(spilled, scope, RehydrateBudget::from_config(store.config()))
            .await
            .unwrap();
        assert_eq!(rehydrated, original);
    }

    #[tokio::test]
    async fn test_spill_nested_and_vec() {
        let (store, scope, _tmp) = test_store().await;
        let original = val!({
            "list": [
                {"blob": Val::Bytes(large_bytes(1, 2048))},
                {"blob": Val::Bytes(large_bytes(2, 2048))},
            ],
        });
        let spilled = store
            .spill_large_val(original.clone(), scope, SpillSource::RunResult, Some("r1"))
            .await
            .unwrap();
        let rehydrated = store
            .rehydrate_blob_refs(spilled, scope, RehydrateBudget::from_config(store.config()))
            .await
            .unwrap();
        assert_eq!(rehydrated, original);
    }

    #[tokio::test]
    async fn test_dedupe_same_content_one_object() {
        let (store, scope, _tmp) = test_store().await;
        let payload = large_bytes(9, 3000);
        let original = val!({
            "a": Val::Bytes(payload.clone()),
            "b": Val::Bytes(payload.clone()),
        });
        let spilled = store
            .spill_large_val(original, scope, SpillSource::CallArgs, Some("c2"))
            .await
            .unwrap();

        let ref_a = BlobRefVal::from_val(&spilled.get("a").unwrap())
            .unwrap()
            .unwrap();
        let ref_b = BlobRefVal::from_val(&spilled.get("b").unwrap())
            .unwrap()
            .unwrap();
        // Same content: one object, and one ref row per (source, object).
        assert_eq!(ref_a.object_id, ref_b.object_id);
        assert_eq!(ref_a.id, ref_b.id);
        assert_eq!(
            blob_db::count_active_refs(store.db(), ref_a.object_id)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn test_rehydrate_cross_org_rejected() {
        let (store, scope, _tmp) = test_store().await;
        let original = val!({"data": Val::Bytes(large_bytes(3, 2048))});
        let spilled = store
            .spill_large_val(original, scope, SpillSource::CallArgs, Some("c3"))
            .await
            .unwrap();

        let other_scope = BlobScope {
            org_id: Uuid::now_v7(),
            env_id: scope.env_id,
            run_id: None,
        };
        let err = store
            .rehydrate_blob_refs(
                spilled,
                other_scope,
                RehydrateBudget::from_config(store.config()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::Unauthorized));
    }

    #[tokio::test]
    async fn test_rehydrate_cross_env_rejected() {
        let (store, scope, _tmp) = test_store().await;
        let original = val!({"data": Val::Bytes(large_bytes(4, 2048))});
        let spilled = store
            .spill_large_val(original, scope, SpillSource::CallArgs, Some("c4"))
            .await
            .unwrap();

        let other_scope = BlobScope {
            org_id: scope.org_id,
            env_id: Some(Uuid::now_v7()),
            run_id: None,
        };
        let err = store
            .rehydrate_blob_refs(
                spilled,
                other_scope,
                RehydrateBudget::from_config(store.config()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::Unauthorized));
    }

    #[tokio::test]
    async fn test_rehydrate_forged_ref_fails_closed() {
        let (store, scope, _tmp) = test_store().await;
        let forged = BlobRefVal {
            id: Uuid::now_v7(),
            object_id: Uuid::now_v7(),
            hash_alg: HASH_ALG_BLAKE3.to_string(),
            hash: "0".repeat(64),
            size: 10,
            content_type: None,
            preview: None,
            encoding: ENCODING_BYTES.to_string(),
        }
        .to_val();
        let err = store
            .rehydrate_blob_refs(forged, scope, RehydrateBudget::from_config(store.config()))
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::RefNotFound(_)));
    }

    #[tokio::test]
    async fn test_rehydrate_ref_budget_enforced() {
        let (store, scope, _tmp) = test_store().await;
        let original = val!({
            "a": Val::Bytes(large_bytes(5, 2048)),
            "b": Val::Bytes(large_bytes(6, 2048)),
        });
        let spilled = store
            .spill_large_val(original, scope, SpillSource::CallArgs, Some("c5"))
            .await
            .unwrap();

        let budget = RehydrateBudget {
            max_refs: 1,
            max_total_bytes: usize::MAX,
        };
        let err = store
            .rehydrate_blob_refs(spilled, scope, budget)
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::BudgetExceeded(_)));
    }

    #[tokio::test]
    async fn test_rehydrate_byte_budget_enforced() {
        let (store, scope, _tmp) = test_store().await;
        let original = val!({"a": Val::Bytes(large_bytes(5, 4096))});
        let spilled = store
            .spill_large_val(original, scope, SpillSource::CallArgs, Some("c6"))
            .await
            .unwrap();

        let budget = RehydrateBudget {
            max_refs: 100,
            max_total_bytes: 1024,
        };
        let err = store
            .rehydrate_blob_refs(spilled, scope, budget)
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::BudgetExceeded(_)));
    }

    #[tokio::test]
    async fn test_spill_disabled_config_is_noop() {
        let db = Arc::new(crate::db::test_db().await);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(LocalFileStorage::new(temp_dir.path().to_path_buf()));
        let config = BlobConfig {
            mode: BlobMode::Disabled,
            spill_threshold_bytes: 1024,
            ..BlobConfig::default()
        };
        let store = BlobStore::new(db, storage, config);
        let scope = BlobScope {
            org_id: Uuid::now_v7(),
            env_id: None,
            run_id: None,
        };

        let original = val!({"data": Val::Bytes(large_bytes(1, 8192))});
        let result = store
            .spill_large_val(original.clone(), scope, SpillSource::CallArgs, None)
            .await
            .unwrap();
        assert_eq!(result, original);
    }

    #[tokio::test]
    async fn test_spill_runs_gated_separately() {
        let (mut_store, scope, _tmp) = {
            let db = Arc::new(crate::db::test_db().await);
            let temp_dir = tempfile::TempDir::new().unwrap();
            let storage = Arc::new(LocalFileStorage::new(temp_dir.path().to_path_buf()));
            let config = BlobConfig {
                mode: BlobMode::Service,
                spill_threshold_bytes: 1024,
                spill_runs: false,
                ..BlobConfig::default()
            };
            (
                BlobStore::new(db, storage, config),
                BlobScope {
                    org_id: Uuid::now_v7(),
                    env_id: None,
                    run_id: None,
                },
                temp_dir,
            )
        };
        let original = val!({"result": Val::Bytes(large_bytes(1, 8192))});
        let run_result = mut_store
            .spill_large_val(original.clone(), scope, SpillSource::RunResult, Some("r9"))
            .await
            .unwrap();
        assert_eq!(run_result, original);

        let call_args = mut_store
            .spill_large_val(original.clone(), scope, SpillSource::CallArgs, Some("c9"))
            .await
            .unwrap();
        assert!(contains_blob_ref(&call_args));
    }

    #[tokio::test]
    async fn test_gc_object_lifecycle() {
        let (store, scope, _tmp) = test_store().await;
        let original = val!({"data": Val::Bytes(large_bytes(8, 2048))});
        let spilled = store
            .spill_large_val(original, scope, SpillSource::CallArgs, Some("c7"))
            .await
            .unwrap();
        let parsed = BlobRefVal::from_val(&spilled.get("data").unwrap())
            .unwrap()
            .unwrap();

        let future_cutoff = chrono::Utc::now() + chrono::Duration::hours(1);

        // Active ref: object must survive GC even past the grace window.
        assert_eq!(store.gc_objects(future_cutoff, 100).await.unwrap(), 0);

        // Deactivate the source refs (as call retention would).
        let deactivated = blob_db::deactivate_refs_by_source(
            store.db(),
            scope.org_id,
            scope.env_id,
            SpillSource::CallArgs.as_str(),
            "c7",
        )
        .await
        .unwrap();
        assert_eq!(deactivated, 1);

        // Within the grace window (cutoff in the past): still not deleted.
        let past_cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
        assert_eq!(store.gc_objects(past_cutoff, 100).await.unwrap(), 0);

        // Past the grace window: deleted.
        assert_eq!(store.gc_objects(future_cutoff, 100).await.unwrap(), 1);
        let object = blob_db::get_object_by_id(store.db(), parsed.object_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(object.status, object_status::DELETED);

        // Rehydration now fails closed.
        let err = store
            .rehydrate_blob_refs(spilled, scope, RehydrateBudget::from_config(store.config()))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            BlobError::RefNotFound(_) | BlobError::ObjectNotAvailable(_)
        ));
    }

    #[tokio::test]
    async fn test_retention_deactivates_call_refs_but_not_manual() {
        let (store, scope, _tmp) = test_store().await;

        let call_val = store
            .spill_large_val(
                val!({"d": Val::Bytes(large_bytes(21, 2048))}),
                scope,
                SpillSource::CallArgs,
                Some("c-ret"),
            )
            .await
            .unwrap();
        let call_ref = BlobRefVal::from_val(&call_val.get("d").unwrap())
            .unwrap()
            .unwrap();
        let manual_val = store
            .spill_large_val(
                val!({"d": Val::Bytes(large_bytes(22, 2048))}),
                scope,
                SpillSource::Manual,
                Some("m-ret"),
            )
            .await
            .unwrap();
        let manual_ref = BlobRefVal::from_val(&manual_val.get("d").unwrap())
            .unwrap()
            .unwrap();

        // Retention cutoff in the future covers both refs, but only call_*
        // kinds are passed, so the manual ref must stay active.
        let cutoff = chrono::Utc::now() + chrono::Duration::hours(1);
        let deactivated = blob_db::deactivate_refs_older_than(
            store.db(),
            Some(scope.org_id),
            &["call_args", "call_return", "call_flow"],
            cutoff,
        )
        .await
        .unwrap();
        assert_eq!(deactivated, 1);
        assert_eq!(
            blob_db::count_active_refs(store.db(), call_ref.object_id)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            blob_db::count_active_refs(store.db(), manual_ref.object_id)
                .await
                .unwrap(),
            1
        );

        // Other orgs' refs are untouched when scoped by org.
        let other_org_deactivated = blob_db::deactivate_refs_older_than(
            store.db(),
            Some(Uuid::now_v7()),
            &["manual"],
            cutoff,
        )
        .await
        .unwrap();
        assert_eq!(other_org_deactivated, 0);
    }

    #[tokio::test]
    async fn test_dedupe_reactivates_after_source_deactivation() {
        let (store, scope, _tmp) = test_store().await;
        let payload = large_bytes(11, 2048);

        let first = store
            .spill_large_val(
                val!({"d": Val::Bytes(payload.clone())}),
                scope,
                SpillSource::CallArgs,
                Some("c8"),
            )
            .await
            .unwrap();
        let parsed = BlobRefVal::from_val(&first.get("d").unwrap())
            .unwrap()
            .unwrap();

        blob_db::deactivate_refs_by_source(
            store.db(),
            scope.org_id,
            scope.env_id,
            SpillSource::CallArgs.as_str(),
            "c8",
        )
        .await
        .unwrap();
        assert_eq!(
            blob_db::count_active_refs(store.db(), parsed.object_id)
                .await
                .unwrap(),
            0
        );

        // Same source spills the same content again: the ref row reactivates.
        store
            .spill_large_val(
                val!({"d": Val::Bytes(payload)}),
                scope,
                SpillSource::CallArgs,
                Some("c8"),
            )
            .await
            .unwrap();
        assert_eq!(
            blob_db::count_active_refs(store.db(), parsed.object_id)
                .await
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_config_from_conf() {
        let conf = val!({
            "blob": {
                "mode": "service",
                "spill": {
                    "threshold-bytes": 1000,
                    "runs": false,
                },
                "preview": {"max-bytes": 64},
            },
        });
        let config = BlobConfig::from_conf(&conf);
        assert_eq!(config.mode, BlobMode::Service);
        assert!(config.enabled());
        assert_eq!(config.spill_threshold_bytes, 1000);
        assert_eq!(config.preview_max_bytes, 64);
        assert!(!config.spill_runs);
        // Defaults preserved.
        assert!(config.spill_bytes);
        assert_eq!(config.storage_prefix, "__hot/blobs");
    }

    #[test]
    fn test_config_mode_parsing() {
        // Missing/empty and "disabled" resolve to Disabled.
        assert_eq!(BlobConfig::from_conf(&val!({})).mode, BlobMode::Disabled);
        assert_eq!(
            BlobConfig::from_conf(&val!({"blob": {"mode": "disabled"}})).mode,
            BlobMode::Disabled
        );
        // Unknown values fail closed to Disabled.
        assert_eq!(
            BlobConfig::from_conf(&val!({"blob": {"mode": "direct"}})).mode,
            BlobMode::Disabled
        );
        assert!("direct".parse::<BlobMode>().is_err());
    }
}
