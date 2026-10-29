//! Moving builds, files, and internal blobs to another storage backend
//! (`hot db move-storage`), while Hot is stopped.
//!
//! Records name their objects by paths that include the backend's root, so
//! copying bytes and changing configuration is not enough: each record is
//! rewritten to its object's new location. Objects keep their keys under the
//! organization and environment (including the unique suffixes of file
//! versions), and every copy is read back and checked before its record
//! changes, one object at a time, so an interrupted move can run again.
//!
//! The source keeps every object. A file record's single update retires the
//! source object under the source backend, which a worker configured for the
//! target never collects, so the old bytes stay for recovery.

use crate::db::{DatabasePool, DatabaseScope, MaintenanceOperation};
use crate::file_storage::{FileStorage, FileStorageContext, FileStorageType};
use crate::storage::BuildStorage;
use crate::val::Val;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// Records read per database round trip.
const PAGE: i64 = 200;

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
    /// The configuration that describes the target, on top of `conf`.
    pub fn configure(&self, conf: &Val, role: StorageRole) -> Result<Val, String> {
        let role = role.as_str();
        let set = |conf: Val, key: &str, value: &str| {
            conf.set(&format!("{role}.{key}"), Val::from(value))
        };
        Ok(match self.kind {
            FileStorageType::Local => {
                if self.path.trim().is_empty() {
                    return Err("A local target needs --path".into());
                }
                set(
                    set(conf.clone(), "storage.type", "local"),
                    "storage.path",
                    &self.path,
                )
            }
            FileStorageType::S3 => {
                if self.bucket.is_empty() {
                    return Err("An S3 target needs --bucket".into());
                }
                let conf = set(
                    set(conf.clone(), "storage.type", "s3"),
                    "aws.s3.bucket",
                    &self.bucket,
                );
                set(
                    set(conf, "aws.s3.prefix", &self.prefix),
                    "aws.s3.region",
                    &self.region,
                )
            }
            FileStorageType::Gcs => {
                if self.bucket.is_empty() {
                    return Err("A GCS target needs --bucket".into());
                }
                let conf = set(
                    set(conf.clone(), "storage.type", "gcs"),
                    "gcs.bucket",
                    &self.bucket,
                );
                set(conf, "gcs.prefix", &self.prefix)
            }
        })
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
            let config = crate::gcs::GcsConfig::resolve(conf, "file", |_| None)?;
            let client = crate::gcs::GcsClient::connect(&config).await?;
            Ok(Box::new(crate::file_storage::gcs::GcsFileStorage::new(
                client,
            )))
        }
        #[allow(unreachable_patterns)]
        other => Err(format!("This binary cannot store files in {other:?}")),
    }
}

fn local_path(conf: &Val, role: &str) -> Result<String, String> {
    let path = conf.get_str_or_default(&format!("{role}.storage.path"), "");
    if path.trim().is_empty() || path == "null" {
        return Err(format!("{role}.storage.path is not set"));
    }
    Ok(path)
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
    pub bytes: usize,
    /// SHA-256 of the bytes, or the BLAKE3 hash for internal blobs.
    pub digest: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MoveSummary {
    pub moved: usize,
    pub planned: usize,
    pub failed: usize,
    pub bytes: usize,
    /// Retired objects left on the source backend, never collected there
    /// once the target is configured.
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
            _ => self.failed += 1,
        }
    }
}

/// Move one role's objects from `source_conf`'s configured backend to
/// `target`, calling `report` for each object. With `dry_run`, nothing is
/// read or written; each object is reported as planned.
pub async fn move_storage(
    db: &DatabasePool,
    source_conf: &Val,
    role: StorageRole,
    target: &MoveTarget,
    dry_run: bool,
    report: &mut dyn FnMut(&MovedObject),
) -> Result<MoveSummary, String> {
    let target_conf = target.configure(source_conf, role)?;
    let mut summary = MoveSummary::default();
    let mut note = |object: MovedObject, summary: &mut MoveSummary| {
        summary.record(&object);
        report(&object);
    };
    match role {
        StorageRole::Build => {
            let source = crate::storage::build_storage_from_config(source_conf).await?;
            let target_storage = build_storage_for(&target_conf, target.kind).await?;
            refuse_same_backend(source.storage_type(), target_storage.storage_type(), role)?;
            for object in move_builds(db, source.as_ref(), target_storage.as_ref(), dry_run).await?
            {
                note(object, &mut summary);
            }
        }
        StorageRole::File => {
            let source = crate::file_storage::file_storage_from_config(source_conf).await?;
            let target_storage = file_storage_for(&target_conf, target.kind).await?;
            refuse_same_backend(source.storage_type(), target_storage.storage_type(), role)?;
            let objects = move_files(db, source.as_ref(), target_storage.as_ref(), dry_run).await?;
            let blobs = move_blobs(db, source.as_ref(), target_storage.as_ref(), dry_run).await?;
            for object in objects.into_iter().chain(blobs) {
                note(object, &mut summary);
            }
            summary.retired_left = retired_objects(db, source.storage_type()).await?;
        }
    }
    Ok(summary)
}

/// A move between two places of the same backend would leave records
/// pointing at either; only moves between backends are supported.
fn refuse_same_backend(source: &str, target: &str, role: StorageRole) -> Result<(), String> {
    if source == target {
        return Err(format!(
            "{} storage already uses {source}; a move changes the backend (local, s3, or gcs)",
            role.as_str()
        ));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Builds
// ---------------------------------------------------------------------------

struct BuildRow {
    build_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    storage_path: String,
}

async fn move_builds(
    db: &DatabasePool,
    source: &dyn BuildStorage,
    target: &dyn BuildStorage,
    dry_run: bool,
) -> Result<Vec<MovedObject>, String> {
    let db = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Retention,
        org_id: None,
    });
    let mut moved = Vec::new();
    let mut after = Uuid::nil();
    loop {
        let rows = build_page(&db, source.storage_type(), after).await?;
        let Some(last) = rows.last() else { break };
        after = last.build_id;
        for row in rows {
            let mut object = MovedObject {
                kind: "build",
                id: row.build_id,
                org_id: row.org_id,
                env_id: Some(row.env_id),
                from: row.storage_path.clone(),
                to: String::new(),
                bytes: 0,
                digest: String::new(),
                status: "planned",
                error: None,
            };
            if !dry_run {
                match copy_build(&db, source, target, &row).await {
                    Ok((to, bytes, digest)) => {
                        object.to = to;
                        object.bytes = bytes;
                        object.digest = digest;
                        object.status = "moved";
                    }
                    Err(error) => {
                        object.status = "failed";
                        object.error = Some(error);
                    }
                }
            }
            moved.push(object);
        }
    }
    Ok(moved)
}

async fn copy_build(
    db: &DatabasePool,
    source: &dyn BuildStorage,
    target: &dyn BuildStorage,
    row: &BuildRow,
) -> Result<(String, usize, String), String> {
    let (id, org, env) = (&row.build_id, &row.org_id, &row.env_id);
    let bytes = source.retrieve_build(id, org, env).await?;
    let digest = sha256(&bytes);
    let size = bytes.len();
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
    let rows: Vec<(Uuid, Uuid, Uuid, String)> = match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query_as(
                "SELECT b.build_id, e.org_id, p.env_id, b.storage_path FROM build b \
             JOIN project p ON p.project_id = b.project_id JOIN env e ON e.env_id = p.env_id \
             WHERE b.storage_path IS NOT NULL AND b.storage_path <> '' \
             AND COALESCE(b.storage_backend, 'local') = $1 AND b.build_id > $2 \
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
             WHERE b.storage_path IS NOT NULL AND b.storage_path <> '' \
             AND COALESCE(b.storage_backend, 'local') = ? AND b.build_id > ? \
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
    dry_run: bool,
) -> Result<Vec<MovedObject>, String> {
    let listing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Retention,
        org_id: None,
    });
    let mut moved = Vec::new();
    let mut after = Uuid::nil();
    loop {
        let rows = file_page(&listing, source.storage_type(), after).await?;
        let Some(last) = rows.last() else { break };
        after = last.file_id;
        for row in rows {
            let mut object = MovedObject {
                kind: "file",
                id: row.file_id,
                org_id: row.org_id,
                env_id: row.env_id,
                from: row.storage_path.clone().unwrap_or_else(|| row.path.clone()),
                to: String::new(),
                bytes: 0,
                digest: String::new(),
                status: "planned",
                error: None,
            };
            if !dry_run {
                match copy_file(db, source, target, &row).await {
                    Ok((to, bytes, digest)) => {
                        object.to = to;
                        object.bytes = bytes;
                        object.digest = digest;
                        object.status = "moved";
                    }
                    Err(error) => {
                        object.status = "failed";
                        object.error = Some(error);
                    }
                }
            }
            moved.push(object);
        }
    }
    Ok(moved)
}

async fn copy_file(
    db: &DatabasePool,
    source: &dyn FileStorage,
    target: &dyn FileStorage,
    row: &FileRow,
) -> Result<(String, usize, String), String> {
    let key = object_key(
        row.storage_path.as_deref(),
        &row.org_id,
        row.env_id.as_ref(),
        &row.path,
    )?;
    let scoped = record_scope(db, row.org_id, row.env_id);
    let ctx = storage_context(&scoped, row.org_id, row.env_id);
    let bytes = source.read_blob_bytes(&key, &ctx).await?;
    let digest = sha256(&bytes);
    let to = target
        .write_blob_bytes(&key, &bytes, row.content_type.as_deref(), &ctx)
        .await?;
    if sha256(&target.read_blob_bytes(&key, &ctx).await?) != digest {
        return Err("the copy does not match the source".into());
    }
    // One statement, so the retire trigger records the source object under
    // the source backend, where nothing configured for the target collects it.
    let updated = match &scoped {
        DatabasePool::Postgres(pool) => sqlx::query(
            "UPDATE file SET storage_backend = $1, storage_path = $2 \
             WHERE file_id = $3 AND storage_backend = $4 AND storage_path IS NOT DISTINCT FROM $5",
        )
        .bind(target.storage_type())
        .bind(&to)
        .bind(row.file_id)
        .bind(source.storage_type())
        .bind(&row.storage_path)
        .execute(pool)
        .await
        .map(|r| r.rows_affected()),
        DatabasePool::Sqlite(pool) => sqlx::query(
            "UPDATE file SET storage_backend = ?, storage_path = ? \
             WHERE file_id = ? AND storage_backend = ? AND storage_path IS ?",
        )
        .bind(target.storage_type())
        .bind(&to)
        .bind(row.file_id)
        .bind(source.storage_type())
        .bind(&row.storage_path)
        .execute(pool)
        .await
        .map(|r| r.rows_affected()),
    }
    .map_err(|e| format!("Cannot update the file record: {e}"))?;
    if updated != 1 {
        return Err("the file changed while it was copied".into());
    }
    Ok((to, bytes.len(), digest))
}

async fn file_page(db: &DatabasePool, backend: &str, after: Uuid) -> Result<Vec<FileRow>, String> {
    type Row = (
        Uuid,
        Uuid,
        Option<Uuid>,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = match db {
        DatabasePool::Postgres(pool) => sqlx::query_as(
            "SELECT file_id, org_id, env_id, path, content_type, storage_path FROM file \
             WHERE active = true AND storage_backend = $1 AND file_id > $2 ORDER BY file_id LIMIT $3",
        )
        .bind(backend)
        .bind(after)
        .bind(PAGE)
        .fetch_all(pool)
        .await,
        DatabasePool::Sqlite(pool) => sqlx::query_as(
            "SELECT file_id, org_id, env_id, path, content_type, storage_path FROM file \
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
            |(file_id, org_id, env_id, path, content_type, storage_path)| FileRow {
                file_id,
                org_id,
                env_id,
                path,
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
    dry_run: bool,
) -> Result<Vec<MovedObject>, String> {
    let db = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::BlobGc,
        org_id: None,
    });
    type Row = (Uuid, Uuid, Option<Uuid>, String, String, Option<String>);
    let mut moved = Vec::new();
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
            let mut object = MovedObject {
                kind: "blob",
                id,
                org_id,
                env_id,
                from: key.clone(),
                to: key.clone(),
                bytes: 0,
                digest: hash.clone(),
                status: "planned",
                error: None,
            };
            if !dry_run {
                let ctx = storage_context(&db, org_id, env_id);
                let copied = async {
                    let bytes = source.read_blob_bytes(&key, &ctx).await?;
                    if blake3::hash(&bytes).to_hex().as_str() != hash {
                        return Err("the source does not match its hash".to_string());
                    }
                    target
                        .write_blob_bytes(&key, &bytes, content_type.as_deref(), &ctx)
                        .await?;
                    if blake3::hash(&target.read_blob_bytes(&key, &ctx).await?)
                        .to_hex()
                        .as_str()
                        != hash
                    {
                        return Err("the copy does not match its hash".to_string());
                    }
                    crate::db::blob::set_object_backend(&db, id, target.storage_type())
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(bytes.len())
                }
                .await;
                match copied {
                    Ok(bytes) => {
                        object.bytes = bytes;
                        object.status = "moved";
                    }
                    Err(error) => {
                        object.status = "failed";
                        object.error = Some(error);
                    }
                }
            }
            moved.push(object);
        }
    }
    Ok(moved)
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
        assert!(
            MoveTarget {
                bucket: String::new(),
                ..target(FileStorageType::S3)
            }
            .configure(&Val::map_empty(), StorageRole::File)
            .is_err()
        );
        assert!(refuse_same_backend("s3", "s3", StorageRole::File).is_err());
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

        let planned = move_files(&f.db, &f.source, &target, true).await.unwrap();
        assert!(planned.iter().all(|o| o.status == "planned"));
        assert_eq!(f.file_records().await, before, "a dry run changes nothing");

        let moved = move_files(&f.db, &f.source, &target, false).await.unwrap();
        assert_eq!(moved.len(), 2);
        assert!(moved.iter().all(|o| o.status == "moved"), "{moved:?}");
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
            move_files(&f.db, &f.source, &target, false)
                .await
                .unwrap()
                .is_empty(),
            "nothing is left to move"
        );
        let retired = retired_objects(&f.db, "local").await.unwrap();
        assert!(
            retired >= 2,
            "the source objects are retired under the source backend ({retired})"
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
        let moved = move_files(&f.db, &f.source, &target, false).await.unwrap();
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
        let moved = move_blobs(&f.db, &f.source, &target, false).await.unwrap();
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
        let moved = move_builds(&f.db, &source, &target, false).await.unwrap();
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
}
