pub mod embedding;
pub mod postgres;
pub mod sqlite;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::sync::Arc;

use crate::val::Val;

enum EmbeddingInput<'a> {
    Off,
    Default,
    Custom(&'a Val),
    Invalid(String),
}

fn classify_embedding_input(val: Option<&Val>) -> EmbeddingInput<'_> {
    let Some(val) = val else {
        return EmbeddingInput::Off;
    };

    match val {
        Val::Null => EmbeddingInput::Off,
        Val::Bool(_) => EmbeddingInput::Invalid(
            "::hot::store/Map.embedding expects EmbeddingOptions?".to_string(),
        ),
        Val::Map(m) => {
            let type_name = m.get(&Val::from("$type")).and_then(|v| match v {
                Val::Str(s) => Some(s.as_ref()),
                _ => None,
            });

            match type_name {
                Some(t) if t.ends_with("/EmbeddingOptions.Off") => EmbeddingInput::Off,
                Some(t) if t.ends_with("/EmbeddingOptions.Default") => EmbeddingInput::Default,
                Some(t) if t.ends_with("/EmbeddingOptions.Embedding") => m
                    .get(&Val::from("$val"))
                    .map_or(EmbeddingInput::Off, EmbeddingInput::Custom),
                Some(t) => EmbeddingInput::Invalid(format!(
                    "::hot::store/Map.embedding expects EmbeddingOptions?, got {t}"
                )),
                None => EmbeddingInput::Invalid(
                    "::hot::store/Map.embedding expects a typed EmbeddingOptions value".to_string(),
                ),
            }
        }
        _ => EmbeddingInput::Invalid(
            "::hot::store/Map.embedding expects EmbeddingOptions?".to_string(),
        ),
    }
}

fn embedding_fields_map(val: &Val) -> Option<&indexmap::IndexMap<Val, Val>> {
    let Val::Map(m) = val else {
        return None;
    };

    if m.contains_key(&Val::from("$type"))
        && let Some(Val::Map(inner)) = m.get(&Val::from("$val"))
    {
        if inner.contains_key(&Val::from("$type"))
            && let Some(Val::Map(deeper)) = inner.get(&Val::from("$val"))
        {
            return Some(deeper);
        }
        return Some(inner);
    }

    Some(m)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreMapConfig {
    pub name: String,
    pub embedding_provider: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_conf: Option<JsonValue>,
    pub embedding_field: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub text_search: bool,
    pub embedding_on_error: Option<String>,
    #[serde(skip)]
    pub embed_fn: Option<Val>,
    #[serde(skip)]
    pub embed_batch_fn: Option<Val>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntry {
    pub key: serde_json::Value,
    pub value: serde_json::Value,
    pub seq: i64,
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl StoreEntry {
    pub fn to_info(&self) -> StoreEntryInfo {
        StoreEntryInfo {
            key: self.key.clone(),
            seq: self.seq,
            embedding_dimensions: self.embedding.as_ref().map(|e| e.len()),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntryInfo {
    pub key: serde_json::Value,
    pub seq: i64,
    pub embedding_dimensions: Option<usize>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Summary of a named store map within the current `(org_id, env_id)` scope.
/// Returned by [`Store::list_maps`] for store browsing and admin UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreMapInfo {
    pub name: String,
    pub embedding_provider: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_conf: Option<JsonValue>,
    pub embedding_dimensions: Option<u32>,
    pub embedding_field: Option<String>,
    pub text_search: bool,
    pub entry_count: i64,
    pub storage_bytes: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub entry: StoreEntry,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchInfoPage {
    pub entries: Vec<StoreEntryInfo>,
    pub total_entries: usize,
}

#[derive(Debug, Clone, Default)]
pub enum SearchMode {
    #[default]
    Semantic,
    Keyword,
    Hybrid,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: Option<usize>,
    pub min_score: Option<f64>,
    pub mode: SearchMode,
}

#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[async_trait]
pub trait Store: Send + Sync {
    /// Ensure the named store exists, creating it if necessary.
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String>;

    /// Write a key-value pair. If the key already exists, update in place (preserve seq).
    async fn put(
        &self,
        store_name: &str,
        key: serde_json::Value,
        value: serde_json::Value,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<StoreEntry, String>;

    /// Get a value by key. Returns None if not found.
    async fn get(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntry>, String>;

    /// Get metadata for an entry without selecting its value.
    async fn get_info(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntryInfo>, String>;

    /// Delete by key. Returns true if the key existed.
    async fn delete(&self, store_name: &str, key: &serde_json::Value) -> Result<bool, String>;

    /// All keys in insertion order.
    async fn keys(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String>;

    /// All values in insertion order.
    async fn vals(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String>;

    /// Number of entries.
    async fn length(&self, store_name: &str) -> Result<usize, String>;

    /// First entry by insertion order.
    async fn first(&self, store_name: &str) -> Result<Option<StoreEntry>, String>;

    /// Last entry by insertion order.
    async fn last(&self, store_name: &str) -> Result<Option<StoreEntry>, String>;

    /// Paginated listing in insertion order.
    async fn list(&self, store_name: &str, options: ListOptions)
    -> Result<Vec<StoreEntry>, String>;

    /// Paginated metadata listing in insertion order, without selecting values.
    async fn list_info(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntryInfo>, String>;

    /// Remove all entries from a store (but keep the store itself).
    async fn clear(&self, store_name: &str) -> Result<(), String>;

    /// Delete the store entirely (schema + data).
    async fn destroy(&self, store_name: &str) -> Result<(), String>;

    /// Batch write from a set of key-value pairs.
    async fn put_many(
        &self,
        store_name: &str,
        entries: Vec<(
            serde_json::Value,
            serde_json::Value,
            Option<Vec<f32>>,
            Option<String>,
        )>,
    ) -> Result<usize, String>;

    /// Semantic / keyword / hybrid search. `query_embedding` is pre-computed by the caller.
    async fn search(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>, String>;

    /// Metadata-only search for browser UIs. Implementations should search keys
    /// and values by keyword, and may additionally include semantic matches when
    /// `query_embedding` is present.
    async fn search_info(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        search_options: SearchOptions,
        list_options: ListOptions,
    ) -> Result<SearchInfoPage, String>;

    /// Total bytes used by all entries across all stores for this backend instance.
    async fn storage_bytes(&self) -> Result<i64, String>;

    fn storage_type(&self) -> &str;

    /// Enumerate all named stores in the current `(org_id, env_id)` scope.
    /// Returned entries include cheap aggregates (entry count, total bytes) so
    /// admin UIs can render summary tables without an extra round-trip.
    async fn list_maps(&self) -> Result<Vec<StoreMapInfo>, String>;
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub enum StoreBackendType {
    #[default]
    Sqlite,
    Postgres,
}

impl StoreBackendType {
    fn parse(raw: &str) -> Self {
        match raw.to_lowercase().as_str() {
            "postgres" | "pg" => StoreBackendType::Postgres,
            _ => StoreBackendType::Sqlite,
        }
    }

    pub fn from_config(conf: &Val) -> Self {
        let env_store_type = std::env::var("HOT_STORE_TYPE").ok();
        Self::from_config_with_env(conf, env_store_type.as_deref())
    }

    fn from_config_with_env(conf: &Val, env_store_type: Option<&str>) -> Self {
        if let Some(store_type) = env_store_type {
            return Self::parse(store_type);
        }
        conf.get("store.type")
            .and_then(|v| match v {
                Val::Str(s) => Some(Self::parse(s.as_ref())),
                _ => None,
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub async fn store_from_config_with_db(
    conf: &Val,
    db_pool: Option<Arc<crate::db::DatabasePool>>,
    org_id: Option<uuid::Uuid>,
    env_id: Option<uuid::Uuid>,
) -> Result<Box<dyn Store>, String> {
    let store_type = StoreBackendType::from_config(conf);
    let eid = env_id.ok_or_else(|| {
        "::hot::store requires an env_id because stores are environment-scoped.".to_string()
    })?;

    match store_type {
        StoreBackendType::Sqlite => {
            let pool = db_pool.ok_or_else(|| {
                "SQLite store requires a DatabasePool. Use store_from_config_with_db() with a pool."
                    .to_string()
            })?;
            let oid = org_id.ok_or_else(|| {
                "SQLite store requires an org_id. Use store_from_config_with_db() with the active org."
                    .to_string()
            })?;

            // One-shot legacy import: copy rows from any pre-2.0 single-tenant
            // .hot/store/store.db into the main hot DB on first run. No-op if the
            // legacy file is absent or already imported.
            if let Err(e) = sqlite::maybe_import_legacy_store(&pool, ".hot/store", oid, eid).await {
                tracing::warn!("Legacy ::hot::store import skipped: {e}");
            }

            let store = sqlite::SqliteStore::new(pool, oid, eid);
            Ok(Box::new(store))
        }
        StoreBackendType::Postgres => {
            let pool = db_pool.ok_or_else(|| {
                "Postgres store requires a DatabasePool. Use store_from_config_with_db() with a pool.".to_string()
            })?;
            let oid = org_id.ok_or_else(|| "Postgres store requires an org_id".to_string())?;
            let store = postgres::PgStore::new(pool, oid, eid);
            Ok(Box::new(store))
        }
    }
}

/// Extract a StoreMapConfig from a Hot `::hot::store/Map` Val.
pub fn store_map_config_from_val(val: &Val) -> Result<StoreMapConfig, String> {
    let inner = match val {
        Val::Map(m) => {
            if let Some(Val::Map(inner_val)) = m.get(&Val::from("$val")) {
                inner_val.clone()
            } else {
                return Err("Expected a ::hot::store/Map typed value".to_string());
            }
        }
        _ => return Err("Expected a ::hot::store/Map typed value".to_string()),
    };

    let name = match inner.get(&Val::from("name")) {
        Some(Val::Str(s)) => s.to_string(),
        _ => return Err("::hot::store/Map requires a 'name' field (Str)".to_string()),
    };

    let embedding_field_val = inner.get(&Val::from("embedding"));

    let (
        embedding_provider,
        embedding_model,
        embedding_field,
        embedding_dimensions,
        text_search,
        embedding_on_error,
        embed_fn,
        embed_batch_fn,
    ) = match classify_embedding_input(embedding_field_val) {
        // embedding: EmbeddingOptions.Default → use system defaults (resolved later)
        EmbeddingInput::Default => (
            Some("__system_default__".to_string()),
            Some("__system_default__".to_string()),
            None,
            None,
            false,
            None,
            None,
            None,
        ),
        // embedding: null / missing / EmbeddingOptions.Off → off
        EmbeddingInput::Off => (None, None, None, None, false, None, None, None),
        // embedding: EmbeddingOptions.Embedding(Embedding({...})) → extract fields
        EmbeddingInput::Custom(em) => {
            let Some(em_inner) = embedding_fields_map(em) else {
                return Err("::hot::store/Embedding requires a map/object value".to_string());
            };
            let provider = em_inner.get(&Val::from("provider")).and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            });
            let embed_fn = em_inner.get(&Val::from("embed-fn")).and_then(|v| {
                if matches!(v, Val::Null) {
                    None
                } else {
                    Some(v.clone())
                }
            });
            let embed_batch_fn = em_inner.get(&Val::from("embed-batch-fn")).and_then(|v| {
                if matches!(v, Val::Null) {
                    None
                } else {
                    Some(v.clone())
                }
            });
            let model = em_inner
                .get(&Val::from("model"))
                .and_then(|v| match v {
                    Val::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .or_else(|| {
                    if embed_fn.is_some() {
                        Some("__hot_fn__".to_string())
                    } else {
                        Some("__system_default__".to_string())
                    }
                });
            let field = em_inner.get(&Val::from("field")).and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            });
            let dimensions = em_inner
                .get(&Val::from("dimensions"))
                .and_then(|v| match v {
                    Val::Int(i) if *i > 0 => Some(*i as u32),
                    _ => None,
                });
            let ts = em_inner
                .get(&Val::from("text-search"))
                .and_then(|v| match v {
                    Val::Bool(b) => Some(*b),
                    _ => None,
                })
                .unwrap_or(false);
            let on_error = em_inner.get(&Val::from("on-error")).and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            });
            (
                provider,
                model,
                field,
                dimensions,
                ts,
                on_error,
                embed_fn,
                embed_batch_fn,
            )
        }
        EmbeddingInput::Invalid(msg) => return Err(msg),
    };

    Ok(StoreMapConfig {
        name,
        embedding_provider,
        embedding_model,
        embedding_conf: None,
        embedding_field,
        embedding_dimensions,
        text_search,
        embedding_on_error,
        embed_fn,
        embed_batch_fn,
    })
}

/// Resolve `__system_default__` model placeholder using hot.hot config.
pub fn resolve_embedding_model(config: &mut StoreMapConfig, conf: Option<&Val>) {
    if config.embedding_provider.as_deref() == Some("__system_default__") {
        let resolved = conf
            .and_then(|c| c.get("store.embedding.provider"))
            .and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "local".to_string());
        config.embedding_provider = Some(resolved);
    } else if config.embedding_provider.is_none() && config.embed_fn.is_some() {
        config.embedding_provider = Some("hot".to_string());
    }

    if config.embedding_model.as_deref() == Some("__system_default__") {
        let resolved = conf
            .and_then(|c| c.get("store.embedding.model"))
            .and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "bge-base-en-v1.5".to_string());
        config.embedding_model = Some(resolved);
    }

    if config.embedding_field.is_none() && config.embedding_model.is_some() {
        let resolved = conf
            .and_then(|c| c.get("store.embedding.field"))
            .and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "content".to_string());
        config.embedding_field = Some(resolved);
    }

    refresh_embedding_conf(config);
}

pub fn refresh_embedding_conf(config: &mut StoreMapConfig) {
    config.embedding_conf = build_embedding_conf(config);
}

pub fn effective_embedding_conf(config: &StoreMapConfig) -> Option<JsonValue> {
    if config.embedding_conf.is_some() {
        return config.embedding_conf.clone();
    }

    build_embedding_conf(config)
}

fn build_embedding_conf(config: &StoreMapConfig) -> Option<JsonValue> {
    let model = config.embedding_model.as_deref()?;

    let provider = config
        .embedding_provider
        .as_deref()
        .unwrap_or(if config.embed_fn.is_some() {
            "hot"
        } else {
            "local"
        });
    let field = config.embedding_field.as_deref().unwrap_or("content");

    Some(json!({
        "provider": provider,
        "model": model,
        "dimensions": config.embedding_dimensions,
        "field": field,
        "version": 1,
    }))
}

/// Outcome of comparing an `ensure_store` request against the stored
/// metadata. Callers should write `UpdateConf(c)` back to the row so
/// `embedding_conf` reflects the newly-known identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatAction {
    NoChange,
    UpdateConf(JsonValue),
}

pub fn validate_store_map_compatibility(
    config: &StoreMapConfig,
    existing_conf: Option<&JsonValue>,
    existing_text_search: bool,
) -> Result<CompatAction, String> {
    if existing_text_search != config.text_search {
        return Err(format!(
            "::hot::store/Map '{}' already exists with text_search {}, requested {}. Recreate the store before changing search mode.",
            config.name, existing_text_search, config.text_search
        ));
    }

    let requested_conf = effective_embedding_conf(config);

    match (existing_conf, requested_conf.as_ref()) {
        (None, None) => Ok(CompatAction::NoChange),
        (None, Some(requested)) => Ok(CompatAction::UpdateConf(requested.clone())),
        (Some(_), None) => Err(format!(
            "::hot::store/Map '{}' already exists with embeddings enabled, requested without. Recreate the store before disabling embeddings.",
            config.name
        )),
        (Some(existing), Some(requested)) => match merge_compatible_confs(existing, requested) {
            Some(merged) if &merged == existing => Ok(CompatAction::NoChange),
            Some(merged) => Ok(CompatAction::UpdateConf(merged)),
            None => Err(format!(
                "::hot::store/Map '{}' already exists with embedding_conf {existing}, requested {requested}. Reindex or recreate the store before changing embeddings.",
                config.name
            )),
        },
    }
}

/// Combine the stored embedding identity with the requested one,
/// returning `None` when they describe incompatible vector spaces.
///
/// Provider/model/field/version must match exactly. `dimensions` is the
/// only field that may widen: a stored `null` is replaced by the
/// requested value (and vice versa) so the first successful embed can
/// lock in the real vector length.
fn merge_compatible_confs(existing: &JsonValue, requested: &JsonValue) -> Option<JsonValue> {
    for key in ["provider", "model", "field", "version"] {
        if existing.get(key) != requested.get(key) {
            return None;
        }
    }

    let e_dims = existing.get("dimensions").and_then(|v| v.as_u64());
    let r_dims = requested.get("dimensions").and_then(|v| v.as_u64());

    let dims = match (e_dims, r_dims) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(_), Some(_)) => return None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let mut merged = existing.clone();
    if let Some(obj) = merged.as_object_mut() {
        obj.insert(
            "dimensions".to_string(),
            dims.map(JsonValue::from).unwrap_or(JsonValue::Null),
        );
    }
    Some(merged)
}

/// Reject configurations whose `provider` we can't actually run.
///
/// At config time we only accept `"local"` (the built-in Rust provider)
/// or `"hot"` / any user-chosen label when an `embed-fn` is supplied.
/// Anything else used to fail silently at `put` time with a warning.
pub fn validate_provider_config(config: &StoreMapConfig) -> Result<(), String> {
    if config.embedding_model.is_none() {
        return Ok(());
    }
    if config.embed_fn.is_some() || config.embed_batch_fn.is_some() {
        return Ok(());
    }

    match config.embedding_provider.as_deref() {
        None | Some("local") => Ok(()),
        Some(other) => Err(format!(
            "::hot::store/Map '{}' embedding provider '{other}' is not built-in. Provide an `embed-fn` (and optional `embed-batch-fn`) on the Embedding, or set provider to \"local\".",
            config.name
        )),
    }
}

pub fn embedding_conf_provider(conf: Option<&JsonValue>) -> Option<String> {
    conf.and_then(|c| c.get("provider"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub fn embedding_conf_model(conf: Option<&JsonValue>) -> Option<String> {
    conf.and_then(|c| c.get("model"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub fn embedding_conf_dimensions(conf: Option<&JsonValue>) -> Option<u32> {
    conf.and_then(|c| c.get("dimensions"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
}

pub fn embedding_conf_field(conf: Option<&JsonValue>) -> Option<String> {
    conf.and_then(|c| c.get("field"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_val(name: &str) -> Val {
        let mut inner = indexmap::IndexMap::new();
        inner.insert(Val::from("name"), Val::from(name));
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from("::hot::store/Map"));
        outer.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        Val::Map(Box::new(outer))
    }

    fn map_val_with_embedding(name: &str, embedding: Val) -> Val {
        let mut inner = indexmap::IndexMap::new();
        inner.insert(Val::from("name"), Val::from(name));
        inner.insert(Val::from("embedding"), embedding);
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from("::hot::store/Map"));
        outer.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        Val::Map(Box::new(outer))
    }

    fn typed_val(type_name: &str, payload: Option<Val>) -> Val {
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from(type_name));
        if let Some(payload) = payload {
            outer.insert(Val::from("$val"), payload);
        }
        Val::Map(Box::new(outer))
    }

    #[test]
    fn test_store_backend_type_from_config() {
        let conf = crate::val!({
            "store": {
                "type": "postgres",
            },
        });
        assert_eq!(
            StoreBackendType::from_config_with_env(&conf, None),
            StoreBackendType::Postgres
        );
    }

    #[test]
    fn test_store_backend_type_defaults_to_sqlite() {
        assert_eq!(
            StoreBackendType::from_config_with_env(&Val::map_empty(), None),
            StoreBackendType::Sqlite
        );
    }

    #[test]
    fn test_store_backend_type_env_overrides_config() {
        let conf = crate::val!({
            "store": {
                "type": "sqlite",
            },
        });
        assert_eq!(
            StoreBackendType::from_config_with_env(&conf, Some("pg")),
            StoreBackendType::Postgres
        );
    }

    #[test]
    fn test_config_from_val_plain() {
        let val = map_val("settings");
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(config.name, "settings");
        assert!(config.embedding_model.is_none());
        assert!(config.embedding_field.is_none());
        assert!(!config.text_search);
    }

    #[test]
    fn test_config_from_val_rejects_legacy_bool_embedding() {
        let val = map_val_with_embedding("kb", Val::Bool(true));
        assert!(store_map_config_from_val(&val).is_err());
    }

    #[test]
    fn test_config_from_val_embedding_conf_custom_map() {
        let mut em = indexmap::IndexMap::new();
        em.insert(Val::from("model"), Val::from("text-embedding-3-small"));
        em.insert(Val::from("provider"), Val::from("openai"));
        em.insert(Val::from("field"), Val::from("body"));
        em.insert(Val::from("dimensions"), Val::Int(1536));
        em.insert(Val::from("text-search"), Val::Bool(true));
        em.insert(Val::from("on-error"), Val::from("fail"));
        let embedding = typed_val("::hot::store/Embedding", Some(Val::Map(Box::new(em))));
        let val = map_val_with_embedding(
            "docs",
            typed_val("::hot::store/EmbeddingOptions.Embedding", Some(embedding)),
        );
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(config.embedding_provider.as_deref(), Some("openai"));
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.embedding_field.as_deref(), Some("body"));
        assert_eq!(config.embedding_dimensions, Some(1536));
        assert_eq!(config.embedding_on_error.as_deref(), Some("fail"));
        assert!(config.text_search);
    }

    #[test]
    fn test_config_from_val_embedding_conf_custom_hot_fn() {
        let mut em = indexmap::IndexMap::new();
        em.insert(Val::from("provider"), Val::from("test"));
        em.insert(Val::from("dimensions"), Val::Int(2));
        em.insert(Val::from("embed-fn"), Val::from("test-embed"));
        em.insert(Val::from("embed-batch-fn"), Val::from("test-embed-batch"));
        let embedding = typed_val("::hot::store/Embedding", Some(Val::Map(Box::new(em))));
        let val = map_val_with_embedding(
            "docs",
            typed_val("::hot::store/EmbeddingOptions.Embedding", Some(embedding)),
        );
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(config.embedding_provider.as_deref(), Some("test"));
        assert_eq!(config.embedding_model.as_deref(), Some("__hot_fn__"));
        assert_eq!(config.embedding_dimensions, Some(2));
        assert!(config.embed_fn.is_some());
        assert!(config.embed_batch_fn.is_some());
    }

    #[test]
    fn test_config_from_val_embedding_conf_default() {
        let val = map_val_with_embedding(
            "kb",
            typed_val("::hot::store/EmbeddingOptions.Default", None),
        );
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(
            config.embedding_provider.as_deref(),
            Some("__system_default__")
        );
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("__system_default__")
        );
    }

    #[test]
    fn test_config_from_val_embedding_conf_off() {
        let val =
            map_val_with_embedding("kb", typed_val("::hot::store/EmbeddingOptions.Off", None));
        let config = store_map_config_from_val(&val).unwrap();
        assert!(config.embedding_model.is_none());
    }

    #[test]
    fn test_config_from_val_embedding_conf_custom() {
        let mut em = indexmap::IndexMap::new();
        em.insert(Val::from("model"), Val::from("text-embedding-3-small"));
        em.insert(Val::from("provider"), Val::from("openai"));
        em.insert(Val::from("field"), Val::from("body"));
        em.insert(Val::from("dimensions"), Val::Int(1536));

        let embedding = typed_val("::hot::store/Embedding", Some(Val::Map(Box::new(em))));
        let val = map_val_with_embedding(
            "docs",
            typed_val("::hot::store/EmbeddingOptions.Embedding", Some(embedding)),
        );
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(config.embedding_provider.as_deref(), Some("openai"));
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.embedding_field.as_deref(), Some("body"));
        assert_eq!(config.embedding_dimensions, Some(1536));
    }

    #[test]
    fn test_config_from_val_missing_name() {
        let mut inner = indexmap::IndexMap::new();
        inner.insert(Val::from("not-name"), Val::from("oops"));
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from("::hot::store/Map"));
        outer.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        let val = Val::Map(Box::new(outer));
        assert!(store_map_config_from_val(&val).is_err());
    }

    #[test]
    fn test_config_from_val_wrong_type() {
        assert!(store_map_config_from_val(&Val::from("not a map")).is_err());
    }

    #[test]
    fn test_resolve_embedding_model_system_default() {
        let mut config = StoreMapConfig {
            name: "t".into(),
            embedding_provider: Some("__system_default__".into()),
            embedding_model: Some("__system_default__".into()),
            embedding_conf: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        };
        resolve_embedding_model(&mut config, None);
        assert_eq!(config.embedding_provider.as_deref(), Some("local"));
        assert_eq!(config.embedding_model.as_deref(), Some("bge-base-en-v1.5"));
        assert_eq!(config.embedding_field.as_deref(), Some("content"));
        assert_eq!(
            config
                .embedding_conf
                .as_ref()
                .and_then(|c| c.get("provider"))
                .and_then(|v| v.as_str()),
            Some("local")
        );
    }

    #[test]
    fn test_resolve_embedding_model_with_conf() {
        let conf = crate::val!({
            "store": {
                "embedding": {
                    "provider": "openai",
                    "model": "text-embedding-3-small",
                    "field": "body"
                }
            }
        });
        let mut config = StoreMapConfig {
            name: "t".into(),
            embedding_provider: Some("__system_default__".into()),
            embedding_model: Some("__system_default__".into()),
            embedding_conf: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        };
        resolve_embedding_model(&mut config, Some(&conf));
        assert_eq!(config.embedding_provider.as_deref(), Some("openai"));
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.embedding_field.as_deref(), Some("body"));
        assert_eq!(
            config
                .embedding_conf
                .as_ref()
                .and_then(|c| c.get("provider"))
                .and_then(|v| v.as_str()),
            Some("openai")
        );
    }

    #[test]
    fn test_resolve_embedding_model_explicit_not_replaced() {
        let mut config = StoreMapConfig {
            name: "t".into(),
            embedding_provider: Some("custom".into()),
            embedding_model: Some("my-custom-model".into()),
            embedding_conf: None,
            embedding_field: Some("text".into()),
            embedding_dimensions: None,
            text_search: false,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        };
        resolve_embedding_model(&mut config, None);
        assert_eq!(config.embedding_provider.as_deref(), Some("custom"));
        assert_eq!(config.embedding_model.as_deref(), Some("my-custom-model"));
        assert_eq!(config.embedding_field.as_deref(), Some("text"));
    }

    fn local_conf(name: &str, dims: Option<u32>) -> StoreMapConfig {
        let mut c = StoreMapConfig {
            name: name.into(),
            embedding_provider: Some("local".into()),
            embedding_model: Some("bge-base-en-v1.5".into()),
            embedding_conf: None,
            embedding_field: Some("content".into()),
            embedding_dimensions: dims,
            text_search: false,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        };
        refresh_embedding_conf(&mut c);
        c
    }

    fn plain_config(name: &str) -> StoreMapConfig {
        StoreMapConfig {
            name: name.into(),
            embedding_provider: None,
            embedding_model: None,
            embedding_conf: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        }
    }

    #[test]
    fn test_compat_no_change_when_both_empty() {
        let cfg = plain_config("s");
        let action = validate_store_map_compatibility(&cfg, None, false).unwrap();
        assert_eq!(action, CompatAction::NoChange);
    }

    #[test]
    fn test_compat_upgrade_from_none_to_some() {
        let cfg = local_conf("s", Some(768));
        let action = validate_store_map_compatibility(&cfg, None, false).unwrap();
        match action {
            CompatAction::UpdateConf(c) => {
                assert_eq!(c.get("provider").and_then(|v| v.as_str()), Some("local"));
                assert_eq!(c.get("dimensions").and_then(|v| v.as_u64()), Some(768));
            }
            _ => panic!("expected UpdateConf"),
        }
    }

    #[test]
    fn test_compat_widen_dimensions() {
        let existing = local_conf("s", None).embedding_conf.unwrap();
        let cfg = local_conf("s", Some(768));
        let action = validate_store_map_compatibility(&cfg, Some(&existing), false).unwrap();
        match action {
            CompatAction::UpdateConf(c) => {
                assert_eq!(c.get("dimensions").and_then(|v| v.as_u64()), Some(768));
            }
            _ => panic!("expected UpdateConf, got {action:?}"),
        }
    }

    #[test]
    fn test_compat_idempotent() {
        let cfg = local_conf("s", Some(768));
        let existing = cfg.embedding_conf.clone().unwrap();
        let action = validate_store_map_compatibility(&cfg, Some(&existing), false).unwrap();
        assert_eq!(action, CompatAction::NoChange);
    }

    #[test]
    fn test_compat_rejects_model_change() {
        let existing = local_conf("s", Some(768)).embedding_conf.unwrap();
        let mut cfg = local_conf("s", Some(768));
        cfg.embedding_model = Some("other-model".into());
        refresh_embedding_conf(&mut cfg);
        let err = validate_store_map_compatibility(&cfg, Some(&existing), false).unwrap_err();
        assert!(err.contains("embedding_conf"), "got: {err}");
    }

    #[test]
    fn test_compat_rejects_dimension_change() {
        let existing = local_conf("s", Some(768)).embedding_conf.unwrap();
        let cfg = local_conf("s", Some(1536));
        let err = validate_store_map_compatibility(&cfg, Some(&existing), false).unwrap_err();
        assert!(err.contains("embedding_conf"), "got: {err}");
    }

    #[test]
    fn test_compat_rejects_disabling_embeddings() {
        let existing = local_conf("s", Some(768)).embedding_conf.unwrap();
        let cfg = plain_config("s");
        let err = validate_store_map_compatibility(&cfg, Some(&existing), false).unwrap_err();
        assert!(err.contains("disabling embeddings"), "got: {err}");
    }

    #[test]
    fn test_compat_rejects_text_search_change() {
        let mut cfg = plain_config("s");
        cfg.text_search = true;
        let err = validate_store_map_compatibility(&cfg, None, false).unwrap_err();
        assert!(err.contains("text_search"), "got: {err}");
    }

    #[test]
    fn test_validate_provider_config_allows_local() {
        let cfg = local_conf("s", Some(768));
        assert!(validate_provider_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_provider_config_rejects_unknown_without_embed_fn() {
        let mut cfg = local_conf("s", Some(1536));
        cfg.embedding_provider = Some("openai".into());
        cfg.embedding_model = Some("text-embedding-3-small".into());
        refresh_embedding_conf(&mut cfg);
        let err = validate_provider_config(&cfg).unwrap_err();
        assert!(err.contains("not built-in"), "got: {err}");
    }

    #[test]
    fn test_validate_provider_config_allows_unknown_with_embed_fn() {
        let mut cfg = local_conf("s", Some(2));
        cfg.embedding_provider = Some("hot".into());
        cfg.embedding_model = Some("__hot_fn__".into());
        cfg.embed_fn = Some(Val::Null);
        refresh_embedding_conf(&mut cfg);
        assert!(validate_provider_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_provider_config_skips_when_embedding_off() {
        let cfg = plain_config("s");
        assert!(validate_provider_config(&cfg).is_ok());
    }
}
