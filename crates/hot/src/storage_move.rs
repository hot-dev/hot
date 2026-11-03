//! Moving builds, files, and internal blobs to another storage backend
//! (`hot db move-storage`), while Hot is stopped.
//!
//! Records name their objects by paths that include the backend's root, so
//! copying bytes and changing configuration is not enough: each record is
//! rewritten to its object's new location. Objects keep their keys under the
//! organization and environment (including the unique suffixes of file
//! versions), and every copy is read back and checked before its record
//! changes, one object at a time, so an interrupted move can run again. Files
//! and blobs stream through, one part at a time, so their size is not bound by
//! memory. Each object is reported as soon as it is done.
//!
//! The source keeps every object. A file record's update also takes its source
//! object out of collection, so no worker deletes the old bytes, on either
//! configuration, and they stay for recovery.

use crate::db::{DatabasePool, DatabaseScope, MaintenanceOperation};
use crate::file_storage::{FileStorage, FileStorageContext, FileStorageType};
use crate::storage::BuildStorage;
use crate::val::Val;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Component, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use uuid::Uuid;

/// Records read per database round trip.
const PAGE: i64 = 200;

/// How long one GCS request of a move may take. A request's limit covers a
/// whole read, usually five minutes, and a move reads whole objects.
#[cfg(feature = "gcs-storage")]
const MOVE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Which storage role moves: builds, or files with the internal blobs that
/// share their backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageRole {
    Build,
    File,
}

impl StorageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::File => "file",
        }
    }
}

/// `path` from the working directory, with `.` and `..` resolved. A `..` after
/// a symlink is the parent of the link's target, as it is to the filesystem;
/// nothing else is resolved, so the path stays the one the operator named.
fn resolve_local_path(path: &str) -> std::io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for part in std::path::absolute(path)?.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.is_symlink() {
                    resolved = std::fs::canonicalize(&resolved)?;
                }
                resolved.pop();
            }
            part => resolved.push(part),
        }
    }
    Ok(resolved)
}

/// Where a role's objects go. `bucket`, `prefix`, and `region` apply to S3
/// and GCS; `path` to local storage.
#[derive(Debug, Clone)]
pub struct MoveTarget {
    pub kind: FileStorageType,
    pub path: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

impl MoveTarget {
    /// The settings that describe the target. Once the move is done the role
    /// must use exactly these: Hot finds internal blobs and builds through
    /// them, not through their records. A local path is made absolute, with no
    /// `.` or `..` left in it, because records keep it and Hot may run from
    /// another directory.
    pub fn settings(&self, role: StorageRole) -> Result<Vec<(String, String)>, String> {
        let role = role.as_str();
        let setting = |key: &str, value: &str| (format!("{role}.{key}"), value.to_string());
        Ok(match self.kind {
            FileStorageType::Local => {
                if self.path.trim().is_empty() {
                    return Err("A local target needs --path".into());
                }
                let path = resolve_local_path(&self.path)
                    .map_err(|e| format!("Cannot resolve --path {}: {e}", self.path))?;
                vec![
                    setting("storage.type", "local"),
                    setting("storage.path", &path.to_string_lossy()),
                ]
            }
            FileStorageType::S3 => {
                if self.bucket.is_empty() {
                    return Err("An S3 target needs --bucket".into());
                }
                vec![
                    setting("storage.type", "s3"),
                    setting("aws.s3.bucket", &self.bucket),
                    setting("aws.s3.prefix", &self.prefix),
                    setting("aws.s3.region", &self.region),
                ]
            }
            FileStorageType::Gcs => {
                if self.bucket.is_empty() {
                    return Err("A GCS target needs --bucket".into());
                }
                vec![
                    setting("storage.type", "gcs"),
                    setting("gcs.bucket", &self.bucket),
                    setting("gcs.prefix", &self.prefix),
                ]
            }
        })
    }

    /// The configuration that describes the target, on top of `conf`.
    pub fn configure(&self, conf: &Val, role: StorageRole) -> Result<Val, String> {
        Ok(self
            .settings(role)?
            .into_iter()
            .fold(conf.clone(), |conf, (key, value)| {
                conf.set(&key, Val::from(value.as_str()))
            }))
    }
}

/// Build storage of an explicit backend. Unlike the configured factory it
/// ignores `HOT_BUILD_STORAGE_TYPE` and `HOT_BUILD_GCS_*`, which name the
/// source while a move runs.
pub async fn build_storage_for(
    conf: &Val,
    kind: FileStorageType,
) -> Result<Box<dyn BuildStorage>, String> {
    match kind {
        FileStorageType::Local => Ok(Box::new(crate::storage::LocalBuildStorage::new(
            PathBuf::from(local_path(conf, "build")?),
        ))),
        #[cfg(feature = "s3-storage")]
        FileStorageType::S3 => Ok(Box::new(
            crate::storage::s3::S3BuildStorage::from_config(conf).await?,
        )),
        #[cfg(feature = "gcs-storage")]
        FileStorageType::Gcs => {
            let config = crate::gcs::GcsConfig::resolve(conf, "build", |_| None)?;
            let client = crate::gcs::GcsClient::connect(&config).await?;
            Ok(Box::new(crate::storage::gcs::GcsBuildStorage::new(client)))
        }
        #[allow(unreachable_patterns)]
        other => Err(format!("This binary cannot store builds in {other:?}")),
    }
}

/// File storage of an explicit backend; see [`build_storage_for`].
pub async fn file_storage_for(
    conf: &Val,
    kind: FileStorageType,
) -> Result<Box<dyn FileStorage>, String> {
    match kind {
        FileStorageType::Local => Ok(Box::new(crate::file_storage::LocalFileStorage::new(
            PathBuf::from(local_path(conf, "file")?),
        ))),
        #[cfg(feature = "s3-storage")]
        FileStorageType::S3 => Ok(Box::new(
            crate::file_storage::s3::S3FileStorage::from_config(conf).await?,
        )),
        #[cfg(feature = "gcs-storage")]
        FileStorageType::Gcs => {
            gcs_file_storage(crate::gcs::GcsConfig::resolve(conf, "file", |_| None)?).await
        }
        #[allow(unreachable_patterns)]
        other => Err(format!("This binary cannot store files in {other:?}")),
    }
}

/// The configured file storage, which names the source of a move.
async fn source_file_storage(conf: &Val) -> Result<Box<dyn FileStorage>, String> {
    #[cfg(feature = "gcs-storage")]
    if FileStorageType::resolve(conf)? == FileStorageType::Gcs {
        let config = crate::gcs::GcsConfig::from_conf(conf, "file")?;
        return gcs_file_storage(config)
            .await
            .map_err(|e| crate::file_storage::backend_init_error(e, "file", "gcs"));
    }
    crate::file_storage::file_storage_from_config(conf).await
}

#[cfg(feature = "gcs-storage")]
async fn gcs_file_storage(
    mut config: crate::gcs::GcsConfig,
) -> Result<Box<dyn FileStorage>, String> {
    config.request_timeout = config.request_timeout.max(MOVE_REQUEST_TIMEOUT);
    let client = crate::gcs::GcsClient::connect(&config).await?;
    Ok(Box::new(crate::file_storage::gcs::GcsFileStorage::new(
        client,
    )))
}

fn local_path(conf: &Val, role: &str) -> Result<String, String> {
    let path = conf.get_str_or_default(&format!("{role}.storage.path"), "");
    if path.trim().is_empty() || path == "null" {
        return Err(format!("{role}.storage.path is not set"));
    }
    Ok(path)
}

/// How a move runs.
#[derive(Debug, Clone, Default)]
pub struct MoveOptions {
    /// Report each object as planned, without copying anything.
    pub dry_run: bool,
    /// Records to leave on the source, by their file, blob object, or build
    /// id: objects the operator has accepted can never move.
    pub skip: HashSet<Uuid>,
}

/// What happened to one object, as written to the manifest.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MovedObject {
    pub kind: &'static str,
    pub id: Uuid,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub from: String,
    pub to: String,
    pub bytes: u64,
    /// SHA-256 of the bytes, or the BLAKE3 hash for internal blobs.
    pub digest: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The object moved, but something about it looks wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl MovedObject {
    fn new(
        kind: &'static str,
        id: Uuid,
        org_id: Uuid,
        env_id: Option<Uuid>,
        from: String,
        options: &MoveOptions,
    ) -> Self {
        Self {
            kind,
            id,
            org_id,
            env_id,
            from,
            to: String::new(),
            bytes: 0,
            digest: String::new(),
            status: if options.skip.contains(&id) {
                "skipped"
            } else {
                "planned"
            },
            error: None,
            warning: None,
        }
    }

    /// Whether this run copies the object.
    fn copies(&self, options: &MoveOptions) -> bool {
        self.status == "planned" && !options.dry_run
    }

    fn failed(&mut self, error: String) {
        self.status = "failed";
        self.error = Some(error);
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MoveSummary {
    pub moved: usize,
    pub planned: usize,
    pub skipped: usize,
    pub failed: usize,
    pub warnings: usize,
    pub bytes: u64,
    /// Objects retired before the move and left on the source backend, never
    /// collected there once the target is configured.
    pub retired_left: i64,
}

impl MoveSummary {
    fn record(&mut self, object: &MovedObject) {
        match object.status {
            "moved" => {
                self.moved += 1;
                self.bytes += object.bytes;
            }
            "planned" => self.planned += 1,
            "skipped" => self.skipped += 1,
            _ => self.failed += 1,
        }
        if object.warning.is_some() {
            self.warnings += 1;
        }
    }
}

/// Reports one finished object; an error stops the move.
type Note<'a> = dyn FnMut(MovedObject) -> Result<(), String> + 'a;

/// Move one role's objects from `source_conf`'s configured backend to
/// `target`, calling `report` as each object finishes, so what `report` keeps
/// is complete up to the moment a run stops. An error from `report` stops the
/// move. `summary` counts what was done, also when this returns an error. In a
/// dry run no object is copied; each is reported as planned.
pub async fn move_storage(
    db: &DatabasePool,
    source_conf: &Val,
    role: StorageRole,
    target: &MoveTarget,
    options: &MoveOptions,
    summary: &mut MoveSummary,
    report: &mut dyn FnMut(&MovedObject) -> Result<(), String>,
) -> Result<(), String> {
    let target_conf = target.configure(source_conf, role)?;
    let mut note = |object: MovedObject| {
        summary.record(&object);
        report(&object)
    };
    match role {
        StorageRole::Build => {
            let source = crate::storage::build_storage_from_config(source_conf).await?;
            let target_storage = build_storage_for(&target_conf, target.kind).await?;
            refuse_same_backend(
                source.storage_type(),
                target_storage.storage_type(),
                source_conf,
                role,
                target,
            )?;
            move_builds(
                db,
                source.as_ref(),
                target_storage.as_ref(),
                options,
                &mut note,
            )
            .await
        }
        StorageRole::File => {
            let source = source_file_storage(source_conf).await?;
            let target_storage = file_storage_for(&target_conf, target.kind).await?;
            refuse_same_backend(
                source.storage_type(),
                target_storage.storage_type(),
                source_conf,
                role,
                target,
            )?;
            move_files(
                db,
                source.as_ref(),
                target_storage.as_ref(),
                options,
                &mut note,
            )
            .await?;
            move_blobs(
                db,
                source.as_ref(),
                target_storage.as_ref(),
                options,
                &mut note,
            )
            .await?;
            summary.retired_left = retired_objects(db, source.storage_type()).await?;
            Ok(())
        }
    }
}

/// A setting's value as Hot would resolve it: `HOT_FILE_GCS_PREFIX` over
/// `file.gcs.prefix`.
fn configured_value(conf: &Val, key: &str) -> String {
    let env = format!("HOT_{}", key.to_ascii_uppercase().replace(['.', '-'], "_"));
    let value = std::env::var(env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| conf.get_str_or_default(key, ""));
    if value == "null" {
        String::new()
    } else {
        value
    }
}

/// A move between two places of the same backend would leave records
/// pointing at either; only moves between backends are supported. Run again
/// after a switch, this is also the check that the configuration matches where
/// the move put things.
fn refuse_same_backend(
    source: &str,
    target_backend: &str,
    conf: &Val,
    role: StorageRole,
    target: &MoveTarget,
) -> Result<(), String> {
    if source != target_backend {
        return Ok(());
    }
    let role_name = role.as_str();
    let differing: Vec<String> = target
        .settings(role)?
        .into_iter()
        .filter(|(key, _)| !key.ends_with(".storage.type") && !key.ends_with(".region"))
        .filter_map(|(key, wanted)| {
            let current = configured_value(conf, &key);
            (current.trim_matches('/') != wanted.trim_matches('/'))
                .then(|| format!("{key} is '{current}', not '{wanted}'"))
        })
        .collect();
    if differing.is_empty() {
        return Err(format!(
            "{role_name} storage already uses {source} with these settings; there is nothing to move"
        ));
    }
    Err(format!(
        "{role_name} storage already uses {source}, and a move changes the backend (local, s3, or \
         gcs). The configuration also differs from this target: {}. Hot finds internal blobs and \
         builds through these settings, so any moved to this target stay unreadable until they match",
        differing.join(", ")
    ))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Passes everything read through `observe`, so one copy can hash what it
/// streams.
struct Observed<R, F> {
    inner: R,
    observe: F,
}

impl<R: AsyncRead + Unpin, F: FnMut(&[u8]) + Unpin> AsyncRead for Observed<R, F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            (this.observe)(&buf.filled()[before..]);
        }
        polled
    }
}

/// Stream a stored object through `observe`, one buffer at a time.
async fn read_object(
    storage: &dyn FileStorage,
    storage_path: Option<&str>,
    key: &str,
    ctx: &FileStorageContext,
    observe: &mut (dyn FnMut(&[u8]) + Send),
) -> Result<(), String> {
    let mut reader = storage.open_object(storage_path, key, ctx).await?;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|e| format!("Cannot read the object: {e}"))?;
        if read == 0 {
            return Ok(());
        }
        observe(&buffer[..read]);
    }
}

// ---------------------------------------------------------------------------
// Builds
// ---------------------------------------------------------------------------

struct BuildRow {
    build_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    storage_path: Option<String>,
}

async fn move_builds(
    db: &DatabasePool,
    source: &dyn BuildStorage,
    target: &dyn BuildStorage,
    options: &MoveOptions,
    note: &mut Note<'_>,
) -> Result<(), String> {
    let db = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Retention,
        org_id: None,
    });
    let mut after = Uuid::nil();
    loop {
        let rows = build_page(&db, source.storage_type(), after).await?;
        let Some(last) = rows.last() else { break };
        after = last.build_id;
        for row in rows {
            let (id, org, env) = (&row.build_id, &row.org_id, &row.env_id);
            let recorded = row.storage_path.clone().filter(|path| !path.is_empty());
            let mut object = MovedObject::new(
                "build",
                row.build_id,
                row.org_id,
                Some(row.env_id),
                recorded
                    .clone()
                    .unwrap_or_else(|| source.build_path(id, org, env)),
                options,
            );
            // Hot reads builds by a key it computes, so a record with no path
            // can still have an object, as builds ported from Hot 1.x do.
            // Without one it is a live build, which has nothing to move. A
            // listed build stays behind, so this check can't fail it.
            if recorded.is_none() && object.status == "planned" {
                match source.exists(id, org, env).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => object.failed(error),
                }
            }
            if object.copies(options) {
                match copy_build(&db, source, target, &row).await {
                    Ok((to, bytes, digest)) => {
                        object.to = to;
                        object.bytes = bytes;
                        object.digest = digest;
                        object.status = "moved";
                    }
                    Err(error) => object.failed(error),
                }
            }
            note(object)?;
        }
    }
    Ok(())
}

async fn copy_build(
    db: &DatabasePool,
    source: &dyn BuildStorage,
    target: &dyn BuildStorage,
    row: &BuildRow,
) -> Result<(String, u64, String), String> {
    let (id, org, env) = (&row.build_id, &row.org_id, &row.env_id);
    let bytes = source.retrieve_build(id, org, env).await?;
    let digest = sha256(&bytes);
    let size = bytes.len() as u64;
    let to = target.store_build(id, org, env, bytes).await?;
    if sha256(&target.retrieve_build(id, org, env).await?) != digest {
        return Err("the copy does not match the source".into());
    }
    crate::db::Build::update_build_storage(db, id, &to, target.storage_type())
        .await
        .map_err(|e| e.to_string())?;
    Ok((to, size, digest))
}

async fn build_page(
    db: &DatabasePool,
    backend: &str,
    after: Uuid,
) -> Result<Vec<BuildRow>, String> {
    // Builds recorded without a backend predate it and are local.
    let rows: Vec<(Uuid, Uuid, Uuid, Option<String>)> = match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query_as(
                "SELECT b.build_id, e.org_id, p.env_id, b.storage_path FROM build b \
             JOIN project p ON p.project_id = b.project_id JOIN env e ON e.env_id = p.env_id \
             WHERE COALESCE(b.storage_backend, 'local') = $1 AND b.build_id > $2 \
             ORDER BY b.build_id LIMIT $3",
            )
            .bind(backend)
            .bind(after)
            .bind(PAGE)
            .fetch_all(pool)
            .await
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query_as(
                "SELECT b.build_id, e.org_id, p.env_id, b.storage_path FROM build b \
             JOIN project p ON p.project_id = b.project_id JOIN env e ON e.env_id = p.env_id \
             WHERE COALESCE(b.storage_backend, 'local') = ? AND b.build_id > ? \
             ORDER BY b.build_id LIMIT ?",
            )
            .bind(backend)
            .bind(after)
            .bind(PAGE)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|e| format!("Cannot list builds: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|(build_id, org_id, env_id, storage_path)| BuildRow {
            build_id,
            org_id,
            env_id,
            storage_path,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

struct FileRow {
    file_id: Uuid,
    org_id: Uuid,
    env_id: Option<Uuid>,
    path: String,
    size: i64,
    etag: Option<String>,
    content_type: Option<String>,
    storage_path: Option<String>,
}

/// The object's key under its organization and environment, from the
/// record's backend-specific path: everything after `{org}/{env}/`, which
/// keeps a version's unique suffix. Records written before storage paths
/// were kept name the unsuffixed path.
pub fn object_key(
    storage_path: Option<&str>,
    org_id: &Uuid,
    env_id: Option<&Uuid>,
    path: &str,
) -> Result<String, String> {
    let stored = storage_path.unwrap_or("").trim().replace('\\', "/");
    if stored.is_empty() {
        return Ok(path.trim_start_matches('/').to_string());
    }
    let env = env_id.map_or_else(|| "default".to_string(), |id| id.simple().to_string());
    let marker = format!("{}/{}/", org_id.simple(), env);
    stored
        .find(&marker)
        .map(|at| stored[at + marker.len()..].to_string())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("{stored} is not under {marker}"))
}

fn storage_context(db: &DatabasePool, org_id: Uuid, env_id: Option<Uuid>) -> FileStorageContext {
    FileStorageContext {
        db: Arc::new(db.clone()),
        org_id,
        env_id,
        user_id: Uuid::nil(),
        run_id: None,
        run_provenance: None,
        file_max_bytes_conf: None,
    }
}

/// The scope a file record is written in: its environment, or its
/// organization for organization-level files.
fn record_scope(db: &DatabasePool, org_id: Uuid, env_id: Option<Uuid>) -> DatabasePool {
    match env_id {
        Some(env_id) => db.with_scope(DatabaseScope::Environment { org_id, env_id }),
        None => db.with_scope(DatabaseScope::Organization { org_id }),
    }
}

async fn move_files(
    db: &DatabasePool,
    source: &dyn FileStorage,
    target: &dyn FileStorage,
    options: &MoveOptions,
    note: &mut Note<'_>,
) -> Result<(), String> {
    let listing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Retention,
        org_id: None,
    });
    let mut after = Uuid::nil();
    loop {
        let rows = file_page(&listing, source.storage_type(), after).await?;
        let Some(last) = rows.last() else { break };
        after = last.file_id;
        for row in rows {
            let mut object = MovedObject::new(
                "file",
                row.file_id,
                row.org_id,
                row.env_id,
                row.storage_path.clone().unwrap_or_else(|| row.path.clone()),
                options,
            );
            if object.copies(options) {
                match copy_file(db, source, target, &row).await {
                    Ok(copy) => {
                        object.to = copy.to;
                        object.bytes = copy.bytes;
                        object.digest = copy.digest;
                        object.warning = copy.warning;
                        object.status = "moved";
                    }
                    Err(error) => object.failed(error),
                }
            }
            note(object)?;
        }
    }
    Ok(())
}

struct FileCopy {
    to: String,
    bytes: u64,
    digest: String,
    warning: Option<String>,
}

async fn copy_file(
    db: &DatabasePool,
    source: &dyn FileStorage,
    target: &dyn FileStorage,
    row: &FileRow,
) -> Result<FileCopy, String> {
    let key = object_key(
        row.storage_path.as_deref(),
        &row.org_id,
        row.env_id.as_ref(),
        &row.path,
    )?;
    let scoped = record_scope(db, row.org_id, row.env_id);
    let ctx = storage_context(&scoped, row.org_id, row.env_id);
    let reader = source
        .open_object(row.storage_path.as_deref(), &key, &ctx)
        .await?;
    let (mut sha, mut md5, mut bytes) = (Sha256::new(), md5::Context::new(), 0u64);
    let mut hashed = Observed {
        inner: reader,
        observe: |chunk: &[u8]| {
            sha.update(chunk);
            md5.consume(chunk);
            bytes += chunk.len() as u64;
        },
    };
    let to = target
        .write_object(&key, &mut hashed, row.content_type.as_deref(), &ctx)
        .await?;
    drop(hashed);
    let digest = hex::encode(sha.finalize());
    if i64::try_from(bytes) != Ok(row.size) {
        return Err(format!(
            "the source holds {bytes} bytes, but its record says {}",
            row.size
        ));
    }
    let mut check = Sha256::new();
    read_object(target, Some(&to), &key, &ctx, &mut |chunk: &[u8]| {
        check.update(chunk)
    })
    .await?;
    if hex::encode(check.finalize()) != digest {
        return Err("the copy does not match the source".into());
    }
    let warning = etag_warning(source.storage_type(), row.etag.as_deref(), md5.finalize());
    let updated = rewrite_file_record(&scoped, row, source.storage_type(), target, &to)
        .await
        .map_err(|e| format!("Cannot update the file record: {e}"))?;
    if updated != 1 {
        return Err("the file changed while it was copied".into());
    }
    Ok(FileCopy {
        to,
        bytes,
        digest,
        warning,
    })
}

/// A warning when bytes don't match a record's MD5 etag. An etag is only an
/// MD5 for some writes (not after S3 encrypts with KMS, or for multipart
/// uploads), so a mismatch does not prove damage: the object moves as it is,
/// and the source keeps it too.
fn etag_warning(backend: &str, etag: Option<&str>, md5: md5::Digest) -> Option<String> {
    let etag = etag.filter(|etag| {
        backend != "s3" && etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit())
    })?;
    let md5 = format!("{md5:x}");
    (!etag.eq_ignore_ascii_case(&md5))
        .then(|| format!("the bytes' MD5 is {md5}, but the record's etag is {etag}"))
}

/// Point a file record at its new object. The update fires the retire trigger,
/// which would have a worker on the old configuration delete the source
/// object; the same transaction takes it back out, because the source keeps
/// every object for recovery. Returns how many records changed.
async fn rewrite_file_record(
    db: &DatabasePool,
    row: &FileRow,
    source_backend: &str,
    target: &dyn FileStorage,
    to: &str,
) -> Result<u64, sqlx::Error> {
    match db {
        DatabasePool::Postgres(pool) => {
            let mut tx = pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE file SET storage_backend = $1, storage_path = $2 \
                 WHERE file_id = $3 AND storage_backend = $4 AND storage_path IS NOT DISTINCT FROM $5",
            )
            .bind(target.storage_type())
            .bind(to)
            .bind(row.file_id)
            .bind(source_backend)
            .bind(&row.storage_path)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if updated == 1 {
                sqlx::query(
                    "DELETE FROM retired_file_object WHERE storage_backend = $1 AND storage_path = $2",
                )
                .bind(source_backend)
                .bind(&row.storage_path)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
            Ok(updated)
        }
        DatabasePool::Sqlite(pool) => {
            let mut tx = pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE file SET storage_backend = ?, storage_path = ? \
                 WHERE file_id = ? AND storage_backend = ? AND storage_path IS ?",
            )
            .bind(target.storage_type())
            .bind(to)
            .bind(row.file_id)
            .bind(source_backend)
            .bind(&row.storage_path)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if updated == 1 {
                sqlx::query(
                    "DELETE FROM retired_file_object WHERE storage_backend = ? AND storage_path = ?",
                )
                .bind(source_backend)
                .bind(&row.storage_path)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
            Ok(updated)
        }
    }
}

async fn file_page(db: &DatabasePool, backend: &str, after: Uuid) -> Result<Vec<FileRow>, String> {
    type Row = (
        Uuid,
        Uuid,
        Option<Uuid>,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = match db {
        DatabasePool::Postgres(pool) => sqlx::query_as(
            "SELECT file_id, org_id, env_id, path, size, etag, content_type, storage_path FROM file \
             WHERE active = true AND storage_backend = $1 AND file_id > $2 ORDER BY file_id LIMIT $3",
        )
        .bind(backend)
        .bind(after)
        .bind(PAGE)
        .fetch_all(pool)
        .await,
        DatabasePool::Sqlite(pool) => sqlx::query_as(
            "SELECT file_id, org_id, env_id, path, size, etag, content_type, storage_path FROM file \
             WHERE active = 1 AND storage_backend = ? AND file_id > ? ORDER BY file_id LIMIT ?",
        )
        .bind(backend)
        .bind(after)
        .bind(PAGE)
        .fetch_all(pool)
        .await,
    }
    .map_err(|e| format!("Cannot list files: {e}"))?;
    Ok(rows
        .into_iter()
        .map(
            |(file_id, org_id, env_id, path, size, etag, content_type, storage_path)| FileRow {
                file_id,
                org_id,
                env_id,
                path,
                size,
                etag,
                content_type,
                storage_path,
            },
        )
        .collect())
}

async fn retired_objects(db: &DatabasePool, backend: &str) -> Result<i64, String> {
    let db = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Retention,
        org_id: None,
    });
    match &db {
        DatabasePool::Postgres(pool) => {
            sqlx::query_scalar(
                "SELECT count(*) FROM retired_file_object WHERE storage_backend = $1",
            )
            .bind(backend)
            .fetch_one(pool)
            .await
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query_scalar("SELECT count(*) FROM retired_file_object WHERE storage_backend = ?")
                .bind(backend)
                .fetch_one(pool)
                .await
        }
    }
    .map_err(|e| format!("Cannot count retired objects: {e}"))
}

// ---------------------------------------------------------------------------
// Internal blobs
// ---------------------------------------------------------------------------

async fn move_blobs(
    db: &DatabasePool,
    source: &dyn FileStorage,
    target: &dyn FileStorage,
    options: &MoveOptions,
    note: &mut Note<'_>,
) -> Result<(), String> {
    let db = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::BlobGc,
        org_id: None,
    });
    type Row = (Uuid, Uuid, Option<Uuid>, String, String, Option<String>);
    let mut after = Uuid::nil();
    loop {
        let rows: Vec<Row> = match &db {
            DatabasePool::Postgres(pool) => sqlx::query_as(
                "SELECT blob_object_id, org_id, env_id, hash, storage_path, content_type FROM blob_object \
                 WHERE status = 'available' AND storage_backend = $1 AND blob_object_id > $2 \
                 ORDER BY blob_object_id LIMIT $3",
            )
            .bind(source.storage_type())
            .bind(after)
            .bind(PAGE)
            .fetch_all(pool)
            .await,
            DatabasePool::Sqlite(pool) => sqlx::query_as(
                "SELECT blob_object_id, org_id, env_id, hash, storage_path, content_type FROM blob_object \
                 WHERE status = 'available' AND storage_backend = ? AND blob_object_id > ? \
                 ORDER BY blob_object_id LIMIT ?",
            )
            .bind(source.storage_type())
            .bind(after)
            .bind(PAGE)
            .fetch_all(pool)
            .await,
        }
        .map_err(|e| format!("Cannot list blobs: {e}"))?;
        let Some(last) = rows.last() else { break };
        after = last.0;
        for (id, org_id, env_id, hash, key, content_type) in rows {
            let mut object = MovedObject::new("blob", id, org_id, env_id, key.clone(), options);
            object.digest = hash.clone();
            if object.copies(options) {
                let ctx = storage_context(&db, org_id, env_id);
                match copy_blob(source, target, &key, &hash, content_type.as_deref(), &ctx).await {
                    Ok((to, bytes)) => {
                        match crate::db::blob::set_object_backend(&db, id, target.storage_type())
                            .await
                        {
                            Ok(()) => {
                                object.to = to;
                                object.bytes = bytes;
                                object.status = "moved";
                            }
                            Err(error) => object.failed(error.to_string()),
                        }
                    }
                    Err(error) => object.failed(error),
                }
            }
            note(object)?;
        }
    }
    Ok(())
}

/// The BLAKE3 hash and size of a stored blob.
async fn blob_digest(
    storage: &dyn FileStorage,
    storage_path: Option<&str>,
    key: &str,
    ctx: &FileStorageContext,
) -> Result<(String, u64), String> {
    let (mut hasher, mut bytes) = (blake3::Hasher::new(), 0u64);
    read_object(storage, storage_path, key, ctx, &mut |chunk: &[u8]| {
        hasher.update(chunk);
        bytes += chunk.len() as u64;
    })
    .await?;
    Ok((hasher.finalize().to_hex().to_string(), bytes))
}

/// Copy a blob and check both ends against its hash. The source is checked
/// before anything is written: the key is a content address, so bytes that
/// don't match must never land there.
async fn copy_blob(
    source: &dyn FileStorage,
    target: &dyn FileStorage,
    key: &str,
    hash: &str,
    content_type: Option<&str>,
    ctx: &FileStorageContext,
) -> Result<(String, u64), String> {
    let (digest, bytes) = blob_digest(source, None, key, ctx).await?;
    if digest != hash {
        return Err("the source does not match its hash".to_string());
    }
    let mut reader = source.open_object(None, key, ctx).await?;
    let to = target
        .write_object(key, &mut reader, content_type, ctx)
        .await?;
    if blob_digest(target, Some(&to), key, ctx).await?.0 != hash {
        return Err("the copy does not match its hash".to_string());
    }
    Ok((to, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A second backend for tests: objects in memory, reported as S3.
    #[derive(Default)]
    struct MemoryStorage {
        objects: Mutex<HashMap<String, Vec<u8>>>,
        corrupt: bool,
        /// Answers every existence check with an error, as a bucket this role
        /// can no longer read does.
        exists_fails: bool,
    }

    impl MemoryStorage {
        fn key(ctx: &FileStorageContext, key: &str) -> String {
            let env = ctx
                .env_id
                .map_or_else(|| "default".into(), |id| id.simple().to_string());
            format!("s3://bucket/files/{}/{env}/{key}", ctx.org_id.simple())
        }
        fn read(&self, key: &str) -> Result<Vec<u8>, String> {
            let mut bytes = self
                .objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or("missing")?;
            if self.corrupt {
                bytes.push(0);
            }
            Ok(bytes)
        }
    }

    #[async_trait]
    impl BuildStorage for MemoryStorage {
        async fn store_build(
            &self,
            id: &Uuid,
            org: &Uuid,
            env: &Uuid,
            data: Vec<u8>,
        ) -> Result<String, String> {
            let key = self.build_path(id, org, env);
            self.objects.lock().unwrap().insert(key.clone(), data);
            Ok(key)
        }
        async fn retrieve_build(
            &self,
            id: &Uuid,
            org: &Uuid,
            env: &Uuid,
        ) -> Result<Vec<u8>, String> {
            self.read(&self.build_path(id, org, env))
        }
        async fn exists(&self, id: &Uuid, org: &Uuid, env: &Uuid) -> Result<bool, String> {
            if self.exists_fails {
                return Err("403 Forbidden".into());
            }
            Ok(self
                .objects
                .lock()
                .unwrap()
                .contains_key(&self.build_path(id, org, env)))
        }
        async fn delete_build(&self, _: &Uuid, _: &Uuid, _: &Uuid) -> Result<(), String> {
            Ok(())
        }
        fn build_path(&self, id: &Uuid, org: &Uuid, env: &Uuid) -> String {
            format!(
                "s3://bucket/builds/{}/{}/{}",
                org.simple(),
                env.simple(),
                crate::storage::build_zip_filename(id)
            )
        }
        fn storage_type(&self) -> &str {
            "s3"
        }
    }

    #[async_trait]
    impl FileStorage for MemoryStorage {
        async fn write_file(
            &self,
            _: &str,
            _: &[u8],
            _: Option<&str>,
            _: &FileStorageContext,
        ) -> Result<crate::file_storage::FileMetadata, String> {
            unimplemented!()
        }
        async fn write_file_if(
            &self,
            _: &str,
            _: &[u8],
            _: Option<&str>,
            _: Option<&str>,
            _: &FileStorageContext,
        ) -> Result<Option<crate::file_storage::FileMetadata>, String> {
            unimplemented!()
        }
        async fn read_file(&self, _: &str, _: &FileStorageContext) -> Result<Vec<u8>, String> {
            unimplemented!()
        }
        async fn delete_file(&self, _: &str, _: &FileStorageContext) -> Result<bool, String> {
            unimplemented!()
        }
        async fn file_exists(&self, _: &str, _: &FileStorageContext) -> Result<bool, String> {
            unimplemented!()
        }
        async fn get_file_metadata(
            &self,
            _: &str,
            _: &FileStorageContext,
        ) -> Result<crate::file_storage::FileMetadata, String> {
            unimplemented!()
        }
        async fn list_files(
            &self,
            _: &str,
            _: &FileStorageContext,
        ) -> Result<Vec<crate::file_storage::FileMetadata>, String> {
            unimplemented!()
        }
        fn storage_type(&self) -> &str {
            "s3"
        }
        async fn write_blob_bytes(
            &self,
            key: &str,
            content: &[u8],
            _: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            let key = Self::key(ctx, key);
            self.objects
                .lock()
                .unwrap()
                .insert(key.clone(), content.to_vec());
            Ok(key)
        }
        async fn read_blob_bytes(
            &self,
            key: &str,
            ctx: &FileStorageContext,
        ) -> Result<Vec<u8>, String> {
            self.read(&Self::key(ctx, key))
        }
    }

    struct Fixture {
        db: DatabasePool,
        _dir: tempfile::TempDir,
        source: crate::file_storage::LocalFileStorage,
        root: PathBuf,
        org: Uuid,
        env: Uuid,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let db = crate::db::test_db().await;
        let org = Uuid::now_v7();
        // File writes check the organization's storage quota.
        crate::db::file::seed_storage_quota_test_org(&Arc::new(db.clone()), org).await;
        Fixture {
            db,
            source: crate::file_storage::LocalFileStorage::new(root.clone()),
            root,
            _dir: dir,
            org,
            env: Uuid::now_v7(),
        }
    }

    fn run() -> MoveOptions {
        MoveOptions::default()
    }

    fn dry_run() -> MoveOptions {
        MoveOptions {
            dry_run: true,
            ..Default::default()
        }
    }

    /// Every object one role's move reports.
    async fn moved_files(
        db: &DatabasePool,
        source: &dyn FileStorage,
        target: &dyn FileStorage,
        options: &MoveOptions,
    ) -> Vec<MovedObject> {
        let mut moved = Vec::new();
        move_files(db, source, target, options, &mut |object| {
            moved.push(object);
            Ok(())
        })
        .await
        .unwrap();
        moved
    }

    async fn moved_blobs(
        db: &DatabasePool,
        source: &dyn FileStorage,
        target: &dyn FileStorage,
    ) -> Vec<MovedObject> {
        let mut moved = Vec::new();
        move_blobs(db, source, target, &run(), &mut |object| {
            moved.push(object);
            Ok(())
        })
        .await
        .unwrap();
        moved
    }

    async fn moved_builds(
        db: &DatabasePool,
        source: &dyn BuildStorage,
        target: &dyn BuildStorage,
        options: &MoveOptions,
    ) -> Vec<MovedObject> {
        let mut moved = Vec::new();
        move_builds(db, source, target, options, &mut |object| {
            moved.push(object);
            Ok(())
        })
        .await
        .unwrap();
        moved
    }

    /// Whatever a worker configured for `storage` would collect, as if every
    /// grace period had passed.
    async fn collect_everything(db: &DatabasePool, storage: &dyn FileStorage) {
        let after_grace = chrono::Utc::now() + chrono::Duration::days(1);
        crate::file_storage::collect_retired_objects(db, storage, after_grace, 1_000)
            .await
            .unwrap();
    }

    impl Fixture {
        fn ctx(&self) -> FileStorageContext {
            storage_context(&self.db, self.org, Some(self.env))
        }
        async fn file_records(&self) -> Vec<(String, String, Option<String>)> {
            let DatabasePool::Sqlite(pool) = &self.db else {
                unreachable!()
            };
            sqlx::query_as("SELECT path, storage_backend, storage_path FROM file WHERE active = 1 ORDER BY path")
                .fetch_all(pool)
                .await
                .unwrap()
        }
    }

    #[test]
    fn object_keys_keep_the_part_under_the_org_and_env() {
        let org = Uuid::parse_str("0190f1aa-0000-7000-8000-000000000001").unwrap();
        let env = Uuid::parse_str("0190f1aa-0000-7000-8000-000000000002").unwrap();
        let (o, e) = (org.simple(), env.simple());
        for stored in [
            format!("/var/lib/hot/file/{o}/{e}/docs/a.txt.v-123"),
            format!("s3://bucket/files/{o}/{e}/docs/a.txt.v-123"),
            format!("gs://bucket/files/{o}/{e}/docs/a.txt.v-123"),
        ] {
            assert_eq!(
                object_key(Some(&stored), &org, Some(&env), "docs/a.txt").unwrap(),
                "docs/a.txt.v-123"
            );
        }
        let org_level = format!("s3://bucket/files/{o}/default/notes.md.cas-9");
        assert_eq!(
            object_key(Some(&org_level), &org, None, "notes.md").unwrap(),
            "notes.md.cas-9"
        );
        assert_eq!(
            object_key(None, &org, Some(&env), "/legacy.txt").unwrap(),
            "legacy.txt"
        );
        assert!(object_key(Some("s3://bucket/elsewhere/x"), &org, Some(&env), "x").is_err());
    }

    #[test]
    fn targets_describe_each_backend_in_configuration() {
        let target = |kind| MoveTarget {
            kind,
            path: "/data/files".into(),
            bucket: "acme".into(),
            prefix: "files".into(),
            region: "us-east-1".into(),
        };
        let s3 = target(FileStorageType::S3)
            .configure(&Val::map_empty(), StorageRole::File)
            .unwrap();
        assert_eq!(s3.get_str("file.storage.type"), "s3");
        assert_eq!(s3.get_str("file.aws.s3.bucket"), "acme");
        assert_eq!(s3.get_str("file.aws.s3.region"), "us-east-1");
        let gcs = target(FileStorageType::Gcs)
            .configure(&Val::map_empty(), StorageRole::Build)
            .unwrap();
        assert_eq!(gcs.get_str("build.gcs.prefix"), "files");
        let local = target(FileStorageType::Local)
            .configure(&Val::map_empty(), StorageRole::File)
            .unwrap();
        assert_eq!(local.get_str("file.storage.path"), "/data/files");
        // Records keep the path, and Hot may run from another directory.
        let relative = MoveTarget {
            path: "data/files".into(),
            ..target(FileStorageType::Local)
        }
        .configure(&Val::map_empty(), StorageRole::File)
        .unwrap();
        let path = relative.get_str("file.storage.path");
        assert!(std::path::Path::new(&path).is_absolute(), "{path}");
        assert!(path.ends_with("data/files"), "{path}");
        // A record with a `..` in it would depend on that directory too, and
        // collection would not delete its object.
        let resolved = |path: &str| {
            MoveTarget {
                path: path.into(),
                ..target(FileStorageType::Local)
            }
            .configure(&Val::map_empty(), StorageRole::File)
            .unwrap()
            .get_str("file.storage.path")
        };
        let parent = resolved("../x");
        let parent = std::path::Path::new(&parent);
        assert!(parent.is_absolute(), "{parent:?}");
        assert!(parent.ends_with("x"), "{parent:?}");
        assert!(
            parent
                .components()
                .all(|part| !matches!(part, Component::ParentDir | Component::CurDir)),
            "{parent:?}"
        );
        for (given, wanted) in [
            ("/nonexistent/../x", "/x"),
            ("/nonexistent/./sub/../files/", "/nonexistent/files"),
            ("/nonexistent/sub/../../../x", "/x"),
            ("/..", "/"),
        ] {
            assert_eq!(resolved(given), wanted, "{given}");
        }
        assert!(
            MoveTarget {
                bucket: String::new(),
                ..target(FileStorageType::S3)
            }
            .configure(&Val::map_empty(), StorageRole::File)
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_after_a_symlink_is_the_parent_of_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("volume/files");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling")).unwrap();
        let named = |rest: &str| dir.path().join(rest).to_string_lossy().into_owned();

        assert_eq!(
            resolve_local_path(&named("link/../x")).unwrap(),
            std::fs::canonicalize(dir.path().join("volume"))
                .unwrap()
                .join("x"),
        );
        // A link only resolves where a `..` needs it to.
        assert_eq!(
            resolve_local_path(&named("link/sub/../x")).unwrap(),
            dir.path().join("link/x"),
        );
        assert_eq!(
            resolve_local_path(&named("link")).unwrap(),
            dir.path().join("link"),
        );
        // Where a `..` leads can't be known, so nothing moves there.
        let error = MoveTarget {
            kind: FileStorageType::Local,
            path: named("dangling/../x"),
            bucket: String::new(),
            prefix: String::new(),
            region: String::new(),
        }
        .settings(StorageRole::File)
        .unwrap_err();
        assert!(error.contains("Cannot resolve --path"), "{error}");
    }

    #[test]
    fn running_again_after_a_switch_checks_the_settings() {
        let target = MoveTarget {
            kind: FileStorageType::S3,
            path: String::new(),
            bucket: "acme".into(),
            prefix: "files".into(),
            region: "us-east-1".into(),
        };
        let switched = target
            .configure(&Val::map_empty(), StorageRole::File)
            .unwrap();
        let same = refuse_same_backend("s3", "s3", &switched, StorageRole::File, &target);
        assert!(same.unwrap_err().contains("nothing to move"));

        let forgot_the_prefix = switched.set("file.aws.s3.prefix", Val::from(""));
        let error = refuse_same_backend("s3", "s3", &forgot_the_prefix, StorageRole::File, &target)
            .unwrap_err();
        assert!(
            error.contains("file.aws.s3.prefix is '', not 'files'"),
            "{error}"
        );
        assert!(!error.contains("file.aws.s3.bucket"), "{error}");
        assert!(refuse_same_backend("local", "s3", &switched, StorageRole::File, &target).is_ok());
    }

    #[tokio::test]
    async fn files_move_with_their_version_keys_and_the_source_is_kept() {
        let f = fixture().await;
        f.source
            .write_file("docs/a.txt", b"first", Some("text/plain"), &f.ctx())
            .await
            .unwrap();
        f.source
            .write_file("docs/a.txt", b"second", Some("text/plain"), &f.ctx())
            .await
            .unwrap();
        f.source
            .write_file("b.bin", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        let before = f.file_records().await;
        let target = MemoryStorage::default();

        let planned = moved_files(&f.db, &f.source, &target, &dry_run()).await;
        assert!(planned.iter().all(|o| o.status == "planned"));
        assert_eq!(f.file_records().await, before, "a dry run changes nothing");

        let moved = moved_files(&f.db, &f.source, &target, &run()).await;
        assert_eq!(moved.len(), 2);
        assert!(moved.iter().all(|o| o.status == "moved"), "{moved:?}");
        // The only object retired on the source is the version the second
        // write replaced. A worker still on the old configuration collects
        // that one, and leaves every moved object as a recovery copy.
        assert_eq!(retired_objects(&f.db, "local").await.unwrap(), 1);
        collect_everything(&f.db, &f.source).await;
        for ((path, backend, stored), (_, _, old)) in f.file_records().await.into_iter().zip(before)
        {
            assert_eq!(backend, "s3");
            let old = old.unwrap();
            let suffix = old.rsplit('/').next().unwrap();
            assert!(
                stored.as_deref().unwrap().ends_with(suffix),
                "{path}: {stored:?} keeps {suffix}"
            );
            assert!(
                std::path::Path::new(&old).exists(),
                "the source object stays for recovery"
            );
            let bytes = target.read(stored.as_deref().unwrap()).unwrap();
            assert_eq!(
                bytes,
                if path == "b.bin" {
                    b"bytes".to_vec()
                } else {
                    b"second".to_vec()
                }
            );
        }
        assert!(
            moved_files(&f.db, &f.source, &target, &run())
                .await
                .is_empty(),
            "nothing is left to move"
        );
        assert!(f.root.exists());
    }

    #[tokio::test]
    async fn a_copy_that_does_not_match_leaves_the_record() {
        let f = fixture().await;
        f.source
            .write_file("a.txt", b"content", None, &f.ctx())
            .await
            .unwrap();
        let before = f.file_records().await;
        let target = MemoryStorage {
            corrupt: true,
            ..Default::default()
        };
        let moved = moved_files(&f.db, &f.source, &target, &run()).await;
        assert_eq!(moved[0].status, "failed");
        assert_eq!(
            moved[0].error.as_deref(),
            Some("the copy does not match the source")
        );
        assert_eq!(f.file_records().await, before);
    }

    #[tokio::test]
    async fn blobs_move_when_their_hash_checks() {
        let f = fixture().await;
        let DatabasePool::Sqlite(pool) = &f.db else {
            unreachable!()
        };
        let mut ids = Vec::new();
        for (content, tamper) in [
            (b"blob one".as_slice(), false),
            (b"blob two".as_slice(), true),
        ] {
            let hash = blake3::hash(content).to_hex().to_string();
            let key = format!("__hot/blobs/blake3/{}/{hash}", &hash[..2]);
            let stored = if tamper {
                b"tampered".as_slice()
            } else {
                content
            };
            f.source
                .write_blob_bytes(&key, stored, None, &f.ctx())
                .await
                .unwrap();
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO blob_object (blob_object_id, org_id, env_id, hash, size, storage_backend, storage_path, status) VALUES (?, ?, ?, ?, ?, 'local', ?, 'available')")
                .bind(id).bind(f.org).bind(f.env).bind(&hash).bind(content.len() as i64).bind(&key)
                .execute(pool).await.unwrap();
            ids.push(id);
        }
        let target = MemoryStorage::default();
        let moved = moved_blobs(&f.db, &f.source, &target).await;
        let status: HashMap<Uuid, &str> = moved.iter().map(|o| (o.id, o.status)).collect();
        assert_eq!((status[&ids[0]], status[&ids[1]]), ("moved", "failed"));
        let backends: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT blob_object_id, storage_backend FROM blob_object ORDER BY blob_object_id",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(
            backends,
            vec![(ids[0], "s3".to_string()), (ids[1], "local".to_string())]
        );
        // A blob's key is its content's address: bad bytes never land there.
        assert_eq!(target.objects.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn builds_move_and_their_records_follow() {
        let f = fixture().await;
        let DatabasePool::Sqlite(pool) = &f.db else {
            unreachable!()
        };
        let source = crate::storage::LocalBuildStorage::new(f.root.join("builds"));
        let (project, build) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?, ?, 'production', ?)")
            .bind(f.env).bind(f.org).bind(Uuid::nil()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO project (project_id, env_id, name, created_by_user_id) VALUES (?, ?, 'demo', ?)")
            .bind(project).bind(f.env).bind(Uuid::nil()).execute(pool).await.unwrap();
        let path = source
            .store_build(&build, &f.org, &f.env, b"zip bytes".to_vec())
            .await
            .unwrap();
        sqlx::query("INSERT INTO build (build_id, project_id, hash, size, created_by_user_id, storage_path, storage_backend) VALUES (?, ?, 'h', 9, ?, ?, 'local')")
            .bind(build).bind(project).bind(Uuid::nil()).bind(&path).execute(pool).await.unwrap();
        let target = MemoryStorage::default();
        let moved = moved_builds(&f.db, &source, &target, &run()).await;
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].status, "moved", "{moved:?}");
        let (stored, backend): (String, String) =
            sqlx::query_as("SELECT storage_path, storage_backend FROM build")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(backend, "s3");
        assert_eq!(target.read(&stored).unwrap(), b"zip bytes");
        assert!(std::path::Path::new(&path).exists());
    }

    #[tokio::test]
    async fn a_move_within_one_backend_touches_nothing() {
        let f = fixture().await;
        f.source
            .write_file("a.txt", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        let before = f.file_records().await;
        let conf =
            crate::val!({"file": {"storage": {"type": "local", "path": f.root.to_str().unwrap()}}});
        let target = MoveTarget {
            kind: FileStorageType::Local,
            path: f.root.join("other").to_str().unwrap().to_string(),
            bucket: String::new(),
            prefix: String::new(),
            region: String::new(),
        };
        let mut summary = MoveSummary::default();
        let refused = move_storage(
            &f.db,
            &conf,
            StorageRole::File,
            &target,
            &run(),
            &mut summary,
            &mut |_| panic!("no object is reported"),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("already uses local"), "{refused}");
        assert_eq!(summary.moved, 0);
        assert_eq!(f.file_records().await, before);
    }

    #[tokio::test]
    async fn builds_without_a_recorded_path_move_when_they_have_an_object() {
        let f = fixture().await;
        let DatabasePool::Sqlite(pool) = &f.db else {
            unreachable!()
        };
        let source = crate::storage::LocalBuildStorage::new(f.root.join("builds"));
        let project = Uuid::now_v7();
        sqlx::query("INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?, ?, 'production', ?)")
            .bind(f.env).bind(f.org).bind(Uuid::nil()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO project (project_id, env_id, name, created_by_user_id) VALUES (?, ?, 'demo', ?)")
            .bind(project).bind(f.env).bind(Uuid::nil()).execute(pool).await.unwrap();
        // Ported from Hot 1.x: a zip, and no storage path or backend.
        let (ported, live) = (Uuid::now_v7(), Uuid::now_v7());
        source
            .store_build(&ported, &f.org, &f.env, b"old zip".to_vec())
            .await
            .unwrap();
        for build in [ported, live] {
            sqlx::query("INSERT INTO build (build_id, project_id, hash, size, created_by_user_id) VALUES (?, ?, 'h', 7, ?)")
                .bind(build).bind(project).bind(Uuid::nil()).execute(pool).await.unwrap();
        }
        let target = MemoryStorage::default();
        let moved = moved_builds(&f.db, &source, &target, &run()).await;
        assert_eq!(
            moved.len(),
            1,
            "a live build has nothing to move: {moved:?}"
        );
        assert_eq!((moved[0].id, moved[0].status), (ported, "moved"));
        assert_eq!(
            target
                .retrieve_build(&ported, &f.org, &f.env)
                .await
                .unwrap(),
            b"old zip"
        );
        let (stored, backend): (String, String) =
            sqlx::query_as("SELECT storage_path, storage_backend FROM build WHERE build_id = ?")
                .bind(ported)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(stored, moved[0].to);
        assert_eq!(backend, "s3");
    }

    #[tokio::test]
    async fn a_listed_build_stays_behind_when_its_existence_check_fails() {
        let f = fixture().await;
        let DatabasePool::Sqlite(pool) = &f.db else {
            unreachable!()
        };
        let source = MemoryStorage {
            exists_fails: true,
            ..Default::default()
        };
        let project = Uuid::now_v7();
        sqlx::query("INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?, ?, 'production', ?)")
            .bind(f.env).bind(f.org).bind(Uuid::nil()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO project (project_id, env_id, name, created_by_user_id) VALUES (?, ?, 'demo', ?)")
            .bind(project).bind(f.env).bind(Uuid::nil()).execute(pool).await.unwrap();
        let build = Uuid::now_v7();
        sqlx::query("INSERT INTO build (build_id, project_id, hash, size, created_by_user_id, storage_backend) VALUES (?, ?, 'h', 7, ?, 's3')")
            .bind(build).bind(project).bind(Uuid::nil()).execute(pool).await.unwrap();
        let target = MemoryStorage::default();

        // The build whose check errors fails on every run...
        let moved = moved_builds(&f.db, &source, &target, &run()).await;
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!((moved[0].id, moved[0].status), (build, "failed"));
        assert_eq!(moved[0].error.as_deref(), Some("403 Forbidden"));

        // ...until the operator chooses to leave it behind.
        for dry_run in [false, true] {
            let options = MoveOptions {
                dry_run,
                skip: HashSet::from([build]),
            };
            let moved = moved_builds(&f.db, &source, &target, &options).await;
            assert_eq!(moved.len(), 1, "{moved:?}");
            assert_eq!((moved[0].id, moved[0].status), (build, "skipped"));
            assert_eq!(moved[0].error, None);
            let mut summary = MoveSummary::default();
            summary.record(&moved[0]);
            assert_eq!((summary.skipped, summary.failed), (1, 0));
        }
        let (stored, backend): (Option<String>, String) =
            sqlx::query_as("SELECT storage_path, storage_backend FROM build")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!((stored, backend.as_str()), (None, "s3"));
    }

    #[tokio::test]
    async fn each_object_is_reported_when_it_finishes_and_a_failed_report_stops_the_move() {
        let f = fixture().await;
        for path in ["a.txt", "b.txt"] {
            f.source
                .write_file(path, b"bytes", None, &f.ctx())
                .await
                .unwrap();
        }
        let target = MemoryStorage::default();
        let mut reported = Vec::new();
        let stopped = move_files(&f.db, &f.source, &target, &run(), &mut |object| {
            reported.push(object);
            Err("the disk is full".to_string())
        })
        .await;
        assert_eq!(stopped.unwrap_err(), "the disk is full");
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].status, "moved");
        let backends: Vec<String> = f
            .file_records()
            .await
            .into_iter()
            .map(|(_, backend, _)| backend)
            .collect();
        assert_eq!(
            backends.iter().filter(|backend| *backend == "s3").count(),
            1,
            "the object after the failed report was not moved: {backends:?}"
        );
    }

    #[tokio::test]
    async fn skipped_objects_stay_on_the_source() {
        let f = fixture().await;
        f.source
            .write_file("gone.txt", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        f.source
            .write_file("kept.txt", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        let before = f.file_records().await;
        std::fs::remove_file(before[0].2.as_deref().unwrap()).unwrap();
        let target = MemoryStorage::default();

        // The object whose bytes are gone fails on every run...
        let moved = moved_files(&f.db, &f.source, &target, &dry_run()).await;
        let gone = moved[0].id;
        let moved = moved_files(&f.db, &f.source, &target, &run()).await;
        let failed: Vec<_> = moved.iter().filter(|o| o.status == "failed").collect();
        assert_eq!(failed.len(), 1, "{moved:?}");
        assert_eq!(failed[0].id, gone);
        assert!(failed[0].error.as_deref().unwrap().contains("not found"));

        // ...until the operator chooses to leave it behind.
        let options = MoveOptions {
            skip: HashSet::from([gone]),
            ..Default::default()
        };
        let moved = moved_files(&f.db, &f.source, &target, &options).await;
        assert_eq!(moved.len(), 1);
        assert_eq!((moved[0].id, moved[0].status), (gone, "skipped"));
        let mut summary = MoveSummary::default();
        summary.record(&moved[0]);
        assert_eq!((summary.skipped, summary.failed), (1, 0));
        let records = f.file_records().await;
        assert_eq!(records[0], before[0], "its record is unchanged");
        assert_eq!(records[1].1, "s3");
    }

    #[tokio::test]
    async fn sources_are_found_through_their_records() {
        let f = fixture().await;
        f.source
            .write_file("a.txt", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        // The base directory has changed since the file was written. Hot
        // still reads it from the path its record holds, so the move does too.
        let now_configured = crate::file_storage::LocalFileStorage::new(f.root.join("elsewhere"));
        let target = MemoryStorage::default();
        let moved = moved_files(&f.db, &now_configured, &target, &run()).await;
        assert_eq!(moved[0].status, "moved", "{moved:?}");
        assert_eq!(target.read(&moved[0].to).unwrap(), b"bytes");
    }

    #[tokio::test]
    async fn sources_are_checked_against_their_records() {
        let f = fixture().await;
        f.source
            .write_file("short.txt", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        f.source
            .write_file("edited.txt", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        let before = f.file_records().await;
        let path_of = |name: &str| {
            before
                .iter()
                .find(|(path, _, _)| path == name)
                .and_then(|(_, _, stored)| stored.clone())
                .unwrap()
        };
        std::fs::write(path_of("short.txt"), b"by").unwrap();
        std::fs::write(path_of("edited.txt"), b"BYTES").unwrap();
        let target = MemoryStorage::default();
        let moved = moved_files(&f.db, &f.source, &target, &run()).await;
        let outcome = |name: &str| {
            moved
                .iter()
                .find(|o| o.from == path_of(name))
                .unwrap()
                .clone()
        };
        let short = outcome("short.txt");
        assert_eq!(short.status, "failed");
        assert_eq!(
            short.error.as_deref(),
            Some("the source holds 2 bytes, but its record says 5")
        );
        // An etag is not always an MD5, so this moves and says so.
        let edited = outcome("edited.txt");
        assert_eq!(edited.status, "moved");
        assert!(edited.warning.as_deref().unwrap().contains("etag"));
        let mut summary = MoveSummary::default();
        summary.record(&edited);
        assert_eq!((summary.moved, summary.warnings), (1, 1));
        assert!(etag_warning("s3", Some(&"0".repeat(32)), md5::compute(b"x")).is_none());
        assert!(etag_warning("gcs", Some("abc-3"), md5::compute(b"x")).is_none());
        assert!(
            etag_warning(
                "local",
                Some(&format!("{:x}", md5::compute(b"x"))),
                md5::compute(b"x")
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn names_ending_in_a_dot_can_move() {
        let f = fixture().await;
        f.source
            .write_file("exports/report.", b"bytes", None, &f.ctx())
            .await
            .unwrap();
        let (_, _, stored) = f.file_records().await.remove(0);
        assert!(stored.unwrap().contains("report..v-"));
        let target = MemoryStorage::default();
        let moved = moved_files(&f.db, &f.source, &target, &run()).await;
        assert_eq!(moved[0].status, "moved", "{moved:?}");
        assert!(moved[0].to.contains("exports/report..v-"));
    }

    #[tokio::test]
    async fn names_too_long_for_a_local_target_are_shortened() {
        let f = fixture().await;
        let DatabasePool::Sqlite(pool) = &f.db else {
            unreachable!()
        };
        let source = MemoryStorage::default();
        let name = format!("reports/{}", "n".repeat(230));
        let key = format!("{name}.v-{}", Uuid::new_v4().simple());
        let stored = source
            .write_blob_bytes(&key, b"bytes", None, &f.ctx())
            .await
            .unwrap();
        sqlx::query("INSERT INTO file (file_id, path, size, storage_backend, storage_path, org_id, env_id, active, created_by_user_id) VALUES (?, ?, 5, 's3', ?, ?, ?, 1, ?)")
            .bind(Uuid::now_v7()).bind(&name).bind(&stored).bind(f.org).bind(f.env).bind(Uuid::nil())
            .execute(pool).await.unwrap();

        let moved = moved_files(&f.db, &source, &f.source, &run()).await;
        assert_eq!(moved[0].status, "moved", "{moved:?}");
        let written = std::path::Path::new(&moved[0].to);
        assert!(written.exists());
        assert!(written.file_name().unwrap().len() <= 255);
        assert_eq!(
            f.source.read_file(&name, &f.ctx()).await.unwrap(),
            b"bytes",
            "Hot reads it through its record"
        );
    }

    /// Against the real schema and SQL. Only this test's own record is moved:
    /// other tests share the database.
    #[tokio::test]
    async fn postgres_moved_objects_stay_out_of_collection() {
        let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
            eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
            return;
        };
        let conf = crate::val!({"uri": uri, "schema": "hot", "rls": {"enforce": false}});
        crate::db::run_migrations(&conf).await.unwrap();
        let pool = crate::db::create_db_pool(&conf).await.unwrap();
        let (org, env, user) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let onboarding = pool.with_scope(DatabaseScope::Bootstrap(
            crate::db::BootstrapOperation::Onboarding,
        ));
        crate::db::User::insert_user(
            &onboarding,
            &user,
            &format!("move-{user}@example.test"),
            None,
            Some(&user),
        )
        .await
        .unwrap();
        crate::db::Org::insert_org(
            &onboarding,
            &org,
            "Move fixture",
            &format!("move-{org}"),
            "individual",
            &user,
        )
        .await
        .unwrap();
        crate::db::Env::insert_env(&onboarding, &env, &org, "development", &user)
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let source = crate::file_storage::LocalFileStorage::new(dir.path().to_path_buf());
        let scoped = record_scope(&pool, org, Some(env));
        let ctx = FileStorageContext::minimal(Arc::new(scoped), org, Some(env), user);
        let written = source
            .write_file("a.txt", b"bytes", None, &ctx)
            .await
            .unwrap();
        let maintenance = pool.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Retention,
            org_id: None,
        });
        // The listings' SQL; no other test's rows are touched.
        assert!(
            file_page(&maintenance, "no-such-backend", Uuid::nil())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            build_page(&maintenance, "no-such-backend", Uuid::nil())
                .await
                .unwrap()
                .is_empty()
        );

        let row = FileRow {
            file_id: written.file_id,
            org_id: org,
            env_id: Some(env),
            path: written.path.clone(),
            size: written.size,
            etag: written.etag.clone(),
            content_type: written.content_type.clone(),
            storage_path: Some(written.storage_path.clone()),
        };
        let target = MemoryStorage::default();
        let copy = copy_file(&pool, &source, &target, &row).await.unwrap();

        let DatabasePool::Postgres(executor) = &maintenance else {
            unreachable!()
        };
        let retired: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM retired_file_object WHERE storage_backend = 'local' AND storage_path = $1",
        )
        .bind(&written.storage_path)
        .fetch_one(executor)
        .await
        .unwrap();
        assert_eq!(retired, 0, "the source object is not up for collection");
        let (backend, stored): (String, String) =
            sqlx::query_as("SELECT storage_backend, storage_path FROM file WHERE file_id = $1")
                .bind(written.file_id)
                .fetch_one(executor)
                .await
                .unwrap();
        assert_eq!(
            (backend.as_str(), stored.as_str()),
            ("s3", copy.to.as_str())
        );
        assert!(std::path::Path::new(&written.storage_path).exists());

        // A second copy of the same version finds the record already changed.
        let again = copy_file(&pool, &source, &target, &row).await;
        assert_eq!(
            again.err().as_deref(),
            Some("the file changed while it was copied")
        );
    }
}
