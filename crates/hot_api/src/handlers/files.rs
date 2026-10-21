//! File management handlers

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{Duration, Utc};
use hot::db::Features;
use hot::db::api_key::ApiKey;
use hot::db::env::Env;
use hot::db::file::{self, FileRecord};
use hot::db::file_upload::{self, PartInfo};
use hot::file_storage::{
    self, CommitBase, FileStorage, FileStorageContext, compute_part_size, validate_part_number,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

type SharedFileStorage = Arc<Box<dyn FileStorage>>;

fn effective_file_max_bytes(conf: &hot::val::Val, plan_max: i64) -> i64 {
    file_storage::FileStorageContext::resolve_file_max_bytes(
        file_storage::FileStorageContext::conf_file_max_bytes(conf),
        plan_max,
    )
}

/// Refuse to route a persisted upload through a backend other than the one
/// that created it. Switching backends does not migrate in-flight uploads.
fn ensure_upload_backend(
    record: &file_upload::FileUploadRecord,
    file_storage: &SharedFileStorage,
) -> Result<(), (StatusCode, Json<ApiErrorResponse>)> {
    if record.storage_backend == file_storage.storage_type() {
        return Ok(());
    }
    Err((
        StatusCode::CONFLICT,
        Json(ApiErrorResponse::new(
            "upload_backend_mismatch",
            format!(
                "Upload was started on the '{}' storage backend, but this installation now uses '{}'; start a new upload",
                record.storage_backend,
                file_storage.storage_type()
            ),
        )),
    ))
}

/// How long a completion claim keeps the expiry sweep away. Retried
/// completions renew it; only a completion that stopped making progress
/// (a crashed process) outlives it.
const COMPLETION_LEASE_HOURS: i64 = 1;

/// Abort an upload whose completion found the file changed underneath it.
/// Failures are logged; a claim that could not finish returns to `pending`,
/// where the client or the expiry sweep can abort it again.
async fn abandon_upload(
    db: &hot::db::DatabasePool,
    file_storage: &dyn FileStorage,
    record: &file_upload::FileUploadRecord,
    org_id: Uuid,
    ctx: &FileStorageContext,
) {
    let Some(env_id) = record.env_id else { return };
    if let Err(e) = file_upload::abandon_completion(db, record.upload_id, org_id, env_id).await {
        tracing::warn!(upload_id = %record.upload_id, "Failed to abandon upload completion: {}", e);
        return;
    }
    let aborted = match record.backend_upload_id.as_deref() {
        Some(backend_upload_id) => {
            file_storage
                .abort_multipart_upload(backend_upload_id, &record.path, ctx)
                .await
        }
        None => Ok(()),
    };
    let finished = match aborted {
        Ok(()) => file_upload::abort_upload(db, record.upload_id, org_id, env_id).await,
        Err(e) => Err(e),
    };
    if let Err(e) = finished {
        tracing::warn!(upload_id = %record.upload_id, "Failed to abort conflicting upload: {}", e);
        let _ = file_upload::release_cleanup_claim(db, record.upload_id, org_id, env_id).await;
    }
}

fn file_response_from_record(record: &FileRecord) -> FileResponse {
    FileResponse {
        file_id: record.file_id,
        path: record.path.clone(),
        size: record.size,
        etag: record.etag.clone(),
        content_type: record.content_type.clone(),
        storage_backend: record.storage_backend.clone(),
        created_by_run_id: record.created_by_run_id,
        updated_by_run_id: record.updated_by_run_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

fn file_response_from_metadata(meta: &file_storage::FileMetadata) -> FileResponse {
    FileResponse {
        file_id: meta.file_id,
        path: meta.path.clone(),
        size: meta.size,
        etag: meta.etag.clone(),
        content_type: meta.content_type.clone(),
        storage_backend: meta.storage_backend.clone(),
        created_by_run_id: meta.created_by_run_id,
        updated_by_run_id: meta.updated_by_run_id,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
    }
}

fn part_conflict() -> (StatusCode, Json<ApiErrorResponse>) {
    (
        StatusCode::CONFLICT,
        Json(ApiErrorResponse::new(
            "part_already_uploaded",
            "Part content is already reserved; abort the upload to replace it",
        )),
    )
}

async fn recorded_part_retry(
    db: &hot::db::DatabasePool,
    record: &file_upload::FileUploadRecord,
    part_number: i32,
    content_hash: &str,
    content_md5: &str,
) -> Result<Option<PartInfo>, (StatusCode, Json<ApiErrorResponse>)> {
    let parts: Vec<PartInfo> =
        serde_json::from_value(record.parts_manifest.clone()).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;
    let Some(part) = parts.into_iter().find(|p| p.part_number == part_number) else {
        return Ok(None);
    };
    let hash = file_upload::part_content_hash(db, record.upload_id, part_number)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e)),
            )
        })?;
    // Old Local/S3 manifests predate reservations and use the body MD5 as ETag.
    if hash
        .as_deref()
        .map_or(part.etag == content_md5, |hash| hash == content_hash)
    {
        Ok(Some(part))
    } else {
        Err(part_conflict())
    }
}

fn part_response(part: PartInfo) -> Json<ApiResponse<UploadPartResponse>> {
    Json(ApiResponse::new(UploadPartResponse {
        part_number: part.part_number,
        size: part.size,
        etag: part.etag,
    }))
}

pub async fn list_files(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Query(params): Query<FileListQueryParams>,
) -> Result<Json<ApiListResponse<FileResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list files.")?;

    let files = file::list_files_by_env(
        &db,
        api_key.env_id,
        params.prefix.as_deref(),
        None,
        Some(params.limit),
        Some(params.offset),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let total = file::get_files_count_by_env(&db, api_key.env_id, params.prefix.as_deref(), None)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let data: Vec<FileResponse> = files.iter().map(file_response_from_record).collect();

    Ok(Json(ApiListResponse::new(
        data,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn get_file(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(file_id): Path<Uuid>,
) -> Result<Json<ApiResponse<FileResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read files.")?;

    let record = file::get_file_by_id_with_env_check(&db, file_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("File: {}", e))),
            )
        })?;

    Ok(Json(ApiResponse::new(file_response_from_record(&record))))
}

pub async fn download_file(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Path(file_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can download files.")?;

    let record = file::get_file_by_id_with_env_check(&db, file_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("File: {}", e))),
            )
        })?;

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let ctx = FileStorageContext::minimal(
        Arc::clone(&db),
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
    );

    let content = file_storage
        .read_file(&record.path, &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to read file: {}",
                    e
                ))),
            )
        })?;

    let content_type = record
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");

    let filename = record.path.rsplit('/').next().unwrap_or(&record.path);

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type.to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", filename),
            ),
            (
                axum::http::header::CONTENT_LENGTH,
                content.len().to_string(),
            ),
        ],
        content,
    ))
}

pub async fn upload_file(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Path(path): Path<String>,
    body: Bytes,
) -> Result<Json<ApiResponse<FileResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can upload files.")?;

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let features = Features::resolve_for_org(&db, &org_id).await;

    let max_file_bytes = effective_file_max_bytes(&conf, features.file_upload_max_bytes());
    let content_size = body.len() as i64;
    if content_size > max_file_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiErrorResponse::new(
                "payload_too_large",
                format!(
                    "File size {} bytes exceeds maximum allowed {} bytes",
                    content_size, max_file_bytes
                ),
            )),
        ));
    }

    let storage_limit = features.storage_bytes();
    if storage_limit >= 0 {
        let current_usage = file::get_storage_usage_by_org(&db, org_id)
            .await
            .unwrap_or(0);
        if current_usage + content_size > storage_limit {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(ApiErrorResponse::new(
                    "storage_quota_exceeded",
                    format!(
                        "Storage quota exceeded: current usage {} + file {} > limit {}",
                        current_usage, content_size, storage_limit
                    ),
                )),
            ));
        }
    }

    let content_type = file_storage::detect_content_type(&path, &body);

    let ctx = FileStorageContext::minimal(
        Arc::clone(&db),
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
    );

    let metadata = file_storage
        .write_file(&path, &body, content_type.as_deref(), &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to write file: {}",
                    e
                ))),
            )
        })?;

    let normalized_path = file_storage::normalize_path(&path).unwrap_or_else(|_| path.clone());

    let record = file::get_file_by_path(&db, &normalized_path, org_id, Some(api_key.env_id))
        .await
        .map(|r| file_response_from_record(&r))
        .unwrap_or_else(|_| FileResponse {
            file_id: metadata.file_id,
            path: metadata.path,
            size: metadata.size,
            etag: metadata.etag,
            content_type: metadata.content_type,
            storage_backend: metadata.storage_backend,
            created_by_run_id: metadata.created_by_run_id,
            updated_by_run_id: metadata.updated_by_run_id,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
        });

    Ok(Json(ApiResponse::new(record)))
}

pub async fn delete_file(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Path(file_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can delete files.")?;

    let record = file::get_file_by_id_with_env_check(&db, file_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("File: {}", e))),
            )
        })?;

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    // Delete through the storage backend so the bytes go with the record:
    // every write stores its bytes at a new location, so bytes left behind
    // here would never be reused or collected.
    let ctx = FileStorageContext::minimal(
        Arc::clone(&db),
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
    );
    if let Err(e) = file_storage.delete_file(&record.path, &ctx).await {
        // The record may be gone even though removing its bytes failed.
        match file::find_file_by_path(&db, &record.path, org_id, Some(api_key.env_id)).await {
            Ok(None) => {
                tracing::warn!(%file_id, "Deleted file record but not its stored bytes: {}", e);
            }
            _ => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Failed to delete file: {}",
                        e
                    ))),
                ));
            }
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn initiate_upload(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Json(req): Json<InitiateUploadRequest>,
) -> Result<
    (StatusCode, Json<ApiResponse<InitiateUploadResponse>>),
    (StatusCode, Json<ApiErrorResponse>),
> {
    super::require_api_key(&auth, "Only API keys can initiate file uploads.")?;

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let normalized_path = file_storage::normalize_path(&req.path).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(&format!(
                "Invalid path: {}",
                e
            ))),
        )
    })?;

    file_storage::validate_path_security(&normalized_path).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(&format!(
                "Invalid path: {}",
                e
            ))),
        )
    })?;

    let features = Features::resolve_for_org(&db, &org_id).await;

    let max_file_bytes = effective_file_max_bytes(&conf, features.file_upload_max_bytes());
    if req.expected_size.is_some_and(|size| size <= 0) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(
                "expected_size must be greater than zero",
            )),
        ));
    }
    let conservative_default = 256_i64 * 1024 * 1024;
    let effective_expected_size = req
        .expected_size
        .unwrap_or_else(|| max_file_bytes.min(conservative_default));

    if let Some(expected_size) = req.expected_size {
        if expected_size > max_file_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(ApiErrorResponse::new(
                    "payload_too_large",
                    format!(
                        "Expected file size {} bytes exceeds maximum allowed {} bytes",
                        expected_size, max_file_bytes
                    ),
                )),
            ));
        }

        let storage_limit = features.storage_bytes();
        if storage_limit >= 0 {
            let current_usage = file::get_storage_usage_by_org(&db, org_id)
                .await
                .unwrap_or(0);
            if current_usage + expected_size > storage_limit {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(ApiErrorResponse::new(
                        "storage_quota_exceeded",
                        format!(
                            "Storage quota would be exceeded: current usage {} + file {} > limit {}",
                            current_usage, expected_size, storage_limit
                        ),
                    )),
                ));
            }
        }
    }

    let part_size = compute_part_size(effective_expected_size as u64);

    let parts_expected = req
        .expected_size
        .map(|s| (s as u64).div_ceil(part_size) as i32);

    let ctx = FileStorageContext::minimal(
        Arc::clone(&db),
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
    );

    let backend_upload_id = file_storage
        .initiate_multipart_upload(&normalized_path, req.content_type.as_deref(), &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to initiate upload: {}",
                    e
                ))),
            )
        })?;

    let expires_at = Utc::now() + Duration::hours(24);
    let max_pending_uploads = conf
        .get_int_or_default("file.multipart.max-pending-uploads", 16)
        .max(1);
    let current_usage = file::get_storage_usage_by_org(&db, org_id)
        .await
        .unwrap_or(0);
    let storage_limit = features.storage_bytes();
    let max_pending_bytes = if storage_limit >= 0 {
        storage_limit.saturating_sub(current_usage)
    } else {
        max_file_bytes.saturating_mul(max_pending_uploads)
    };

    let upload_record_result = file_upload::insert_upload(
        &db,
        &normalized_path,
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
        Some(effective_expected_size),
        req.content_type.as_deref(),
        part_size as i64,
        parts_expected,
        Some(&backend_upload_id),
        file_storage.storage_type(),
        expires_at,
        max_pending_uploads,
        max_pending_bytes,
    )
    .await;
    let upload_record = match upload_record_result {
        Ok(record) => record,
        Err(e) => {
            let _ = file_storage
                .abort_multipart_upload(&backend_upload_id, &normalized_path, &ctx)
                .await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to create upload record: {}",
                    e
                ))),
            ));
        }
    };

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(InitiateUploadResponse {
            upload_id: upload_record.upload_id,
            path: normalized_path,
            part_size: part_size as i64,
            parts_expected,
            expires_at,
        })),
    ))
}

pub async fn upload_part_handler(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Path((upload_id, part_number)): Path<(Uuid, i32)>,
    body: Bytes,
) -> Result<Json<ApiResponse<UploadPartResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can upload file parts.")?;

    validate_part_number(part_number).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(&e)),
        )
    })?;

    // S3 minimum part size is 5 MB (except for the last part)
    const MIN_PART_SIZE: usize = 5 * 1024 * 1024;
    // Reject obviously oversized parts (256 MB is generous for a single part)
    const MAX_PART_SIZE: usize = 256 * 1024 * 1024;
    if body.len() > MAX_PART_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiErrorResponse::new(
                "part_too_large",
                format!(
                    "Part size {} exceeds maximum of {} bytes",
                    body.len(),
                    MAX_PART_SIZE
                ),
            )),
        ));
    }

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let record = file_upload::get_upload(&db, upload_id, org_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("Upload: {}", e))),
            )
        })?;
    ensure_upload_backend(&record, &file_storage)?;

    // Non-final parts must meet S3 minimum size (5 MB)
    if body.len() < MIN_PART_SIZE
        && record
            .parts_expected
            .is_some_and(|total| part_number < total)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::new(
                "part_too_small",
                format!(
                    "Non-final part size {} is below the minimum of {} bytes",
                    body.len(),
                    MIN_PART_SIZE
                ),
            )),
        ));
    }

    if record.status != "pending" {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "upload_not_pending",
                format!("Upload is in '{}' status, not 'pending'", record.status),
            )),
        ));
    }
    if record
        .parts_expected
        .is_some_and(|total| part_number > total)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(
                "part number exceeds the expected part count",
            )),
        ));
    }
    if body.len() as i64 > record.part_size
        && record
            .parts_expected
            .is_none_or(|total| part_number < total)
    {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiErrorResponse::new(
                "part_too_large",
                format!("Part exceeds negotiated size of {} bytes", record.part_size),
            )),
        ));
    }

    let backend_upload_id = record.backend_upload_id.as_deref().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(
                "Upload record missing backend upload ID",
            )),
        )
    })?;

    let ctx = FileStorageContext::minimal(
        Arc::clone(&db),
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
    );

    let content_hash = file_storage::compute_part_hash(&body);
    let content_md5 = file_storage::compute_md5(&body);
    if let Some(part) =
        recorded_part_retry(&db, &record, part_number, &content_hash, &content_md5).await?
    {
        return Ok(part_response(part));
    }
    let claim_token = match file_upload::reserve_part(
        &db,
        upload_id,
        org_id,
        api_key.env_id,
        part_number,
        &content_hash,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e)),
        )
    })? {
        file_upload::PartReservation::Claimed(owner) => owner,
        file_upload::PartReservation::DifferentContent => return Err(part_conflict()),
        file_upload::PartReservation::Busy => {
            let current = file_upload::get_upload(&db, upload_id, org_id, api_key.env_id)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiErrorResponse::internal_error(&e)),
                    )
                })?;
            if let Some(part) =
                recorded_part_retry(&db, &current, part_number, &content_hash, &content_md5).await?
            {
                return Ok(part_response(part));
            }
            return Err((
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::new(
                    "part_in_progress",
                    "This part is being uploaded; retry after it finishes",
                )),
            ));
        }
    };
    // A previous owner may have recorded the part between our first lookup
    // and the reservation. Never send a recorded part to S3 again.
    let current = file_upload::get_upload(&db, upload_id, org_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e)),
            )
        })?;
    if let Some(part) =
        recorded_part_retry(&db, &current, part_number, &content_hash, &content_md5).await?
    {
        return Ok(part_response(part));
    }
    let uploaded = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        file_storage.upload_part(backend_upload_id, &record.path, part_number, &body, &ctx),
    )
    .await;
    let etag = match uploaded {
        Ok(Ok(etag)) => etag,
        result => {
            let _ = file_upload::release_part(&db, upload_id, part_number, claim_token).await;
            let error = match result {
                Ok(Err(e)) => e,
                _ => "Part upload timed out".to_string(),
            };
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&error)),
            ));
        }
    };

    let part_size = body.len() as i64;

    let record_result = file_upload::record_part(
        &db,
        upload_id,
        org_id,
        api_key.env_id,
        &PartInfo {
            part_number,
            size: part_size,
            etag: etag.clone(),
        },
        claim_token,
    )
    .await;
    if let Err(e) = record_result {
        // Never abort the upload here: the request may be a retry of a part
        // already recorded, or a late duplicate racing a completion that now
        // owns the upload. Unrecorded bytes stay in staging until the upload
        // completes or is aborted.
        let current = file_upload::get_upload(&db, upload_id, org_id, api_key.env_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Failed to record part: {}",
                        e
                    ))),
                )
            })?;
        if current.status != "pending" {
            return Err((
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::new(
                    "upload_not_pending",
                    format!("Upload is in '{}' status, not 'pending'", current.status),
                )),
            ));
        }
        let _ = file_upload::release_part(&db, upload_id, part_number, claim_token).await;
        if let Some(part) =
            recorded_part_retry(&db, &current, part_number, &content_hash, &content_md5).await?
        {
            return Ok(part_response(part));
        }
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to record part: {e}"
            ))),
        ));
    }

    Ok(Json(ApiResponse::new(UploadPartResponse {
        part_number,
        size: part_size,
        etag,
    })))
}

pub async fn complete_upload(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Path(upload_id): Path<Uuid>,
) -> Result<Json<ApiResponse<FileResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can complete file uploads.")?;

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let record = file_upload::get_upload(&db, upload_id, org_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("Upload: {}", e))),
            )
        })?;
    ensure_upload_backend(&record, &file_storage)?;

    if record.status == "completed" {
        if let Some(result) = record.completion_result {
            let response = serde_json::from_value(result).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?;
            return Ok(Json(ApiResponse::new(response)));
        }
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "completion_result_unavailable",
                "This upload completed before response snapshots were recorded",
            )),
        ));
    }

    // `completing` means an earlier completion attempt was interrupted
    // (crash, lost response, failed commit); completion resumes it.
    if record.status != "pending" && record.status != "completing" {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "upload_not_pending",
                format!("Upload is in '{}' status, not 'pending'", record.status),
            )),
        ));
    }
    if let Some(parts_expected) = record.parts_expected
        && (record.parts_received != parts_expected
            || record.expected_size != Some(record.bytes_received))
    {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "upload_incomplete",
                format!(
                    "Upload has {}/{} parts and {}/{} bytes",
                    record.parts_received,
                    parts_expected,
                    record.bytes_received,
                    record.expected_size.unwrap_or_default()
                ),
            )),
        ));
    }

    // Re-check quota with actual bytes_received
    let features = Features::resolve_for_org(&db, &org_id).await;
    let storage_limit = features.storage_bytes();
    if storage_limit >= 0 {
        let current_usage = file::get_storage_usage_by_org(&db, org_id)
            .await
            .unwrap_or(0);
        if current_usage + record.bytes_received > storage_limit {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(ApiErrorResponse::new(
                    "storage_quota_exceeded",
                    format!(
                        "Storage quota exceeded: current usage {} + file {} > limit {}",
                        current_usage, record.bytes_received, storage_limit
                    ),
                )),
            ));
        }
    }

    let backend_upload_id = record.backend_upload_id.as_deref().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(
                "Upload record missing backend upload ID",
            )),
        )
    })?;

    let parts_list: Vec<PartInfo> =
        serde_json::from_value(record.parts_manifest.clone()).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to parse parts manifest: {}",
                    e
                ))),
            )
        })?;

    let mut parts: Vec<(i32, String)> = parts_list
        .into_iter()
        .map(|p| (p.part_number, p.etag))
        .collect();
    parts.sort_by_key(|(num, _)| *num);

    let ctx = FileStorageContext::minimal(
        Arc::clone(&db),
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
    );

    // Claim the upload before assembling bytes so a concurrent abort or the
    // expiry sweep cannot delete them mid-completion. The claim is a lease:
    // the sweep only reclaims a completion that stopped making progress.
    let base = file_upload::begin_completion(
        &db,
        upload_id,
        org_id,
        api_key.env_id,
        Utc::now() + Duration::hours(COMPLETION_LEASE_HOURS),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new("upload_not_pending", e)),
        )
    })?;

    let metadata = match file_storage
        .complete_multipart_upload(
            backend_upload_id,
            &record.path,
            &parts,
            record.bytes_received,
            record.content_type.as_deref(),
            &CommitBase::from_column(Some(base.as_str())),
            &ctx,
        )
        .await
    {
        Ok(meta) => meta,
        Err(e) => {
            if e.starts_with(file_storage::COMPLETION_CONFLICT) {
                // The file changed after this completion began, so the upload
                // can never be applied: abort it rather than leave it retryable.
                abandon_upload(&db, file_storage.as_ref().as_ref(), &record, org_id, &ctx).await;
                return Err((
                    StatusCode::CONFLICT,
                    Json(ApiErrorResponse::new("upload_conflict", e)),
                ));
            }
            // Hand the upload back so the client can retry or abort it.
            if let Err(release_error) =
                file_upload::release_completion(&db, upload_id, org_id, api_key.env_id).await
            {
                tracing::warn!(%upload_id, "Failed to release upload completion: {}", release_error);
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to complete upload: {}",
                    e
                ))),
            ));
        }
    };

    let response = file_response_from_metadata(&metadata);
    let result = serde_json::to_value(&response).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;
    file_upload::complete_upload(&db, upload_id, org_id, api_key.env_id, &result)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to mark upload complete: {}",
                    e
                ))),
            )
        })?;

    Ok(Json(ApiResponse::new(response)))
}

pub async fn abort_upload(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Extension(file_storage): Extension<SharedFileStorage>,
    Path(upload_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can abort file uploads.")?;

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let record = file_upload::get_upload(&db, upload_id, org_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("Upload: {}", e))),
            )
        })?;
    ensure_upload_backend(&record, &file_storage)?;

    if record.status == "aborted" {
        return Ok(StatusCode::NO_CONTENT);
    }

    // Claim the upload before deleting anything, so an abort can never
    // delete parts out from under a completion that has already started.
    file_upload::begin_abort(&db, upload_id, org_id, api_key.env_id)
        .await
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::new(
                    "upload_not_pending",
                    format!("Upload is in '{}' status, not 'pending'", record.status),
                )),
            )
        })?;

    if let Some(backend_upload_id) = record.backend_upload_id.as_deref() {
        let ctx = FileStorageContext::minimal(
            Arc::clone(&db),
            org_id,
            Some(api_key.env_id),
            api_key.created_by_user_id,
        );

        if let Err(e) = file_storage
            .abort_multipart_upload(backend_upload_id, &record.path, &ctx)
            .await
        {
            if let Err(release_error) =
                file_upload::release_cleanup_claim(&db, upload_id, org_id, api_key.env_id).await
            {
                tracing::warn!(%upload_id, "Failed to release upload abort claim: {}", release_error);
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to abort upload: {}",
                    e
                ))),
            ));
        }
    }

    file_upload::abort_upload(&db, upload_id, org_id, api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to mark upload aborted: {}",
                    e
                ))),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
}
