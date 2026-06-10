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
    self, FileStorage, FileStorageContext, compute_part_size, validate_part_number,
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

    file::mark_file_inactive(
        &db,
        &record.path,
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
        None,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to delete file: {}",
                e
            ))),
        )
    })?;

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

    if let Some(expected_size) = req.expected_size {
        let max_file_bytes = effective_file_max_bytes(&conf, features.file_upload_max_bytes());
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

    let part_size = req
        .expected_size
        .map(|s| compute_part_size(s as u64))
        .unwrap_or(file_storage::DEFAULT_PART_SIZE);

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

    let upload_record = file_upload::insert_upload(
        &db,
        &normalized_path,
        org_id,
        Some(api_key.env_id),
        api_key.created_by_user_id,
        req.expected_size,
        req.content_type.as_deref(),
        part_size as i64,
        parts_expected,
        Some(&backend_upload_id),
        file_storage.storage_type(),
        expires_at,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to create upload record: {}",
                e
            ))),
        )
    })?;

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

    let record = file_upload::get_upload(&db, upload_id, org_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("Upload: {}", e))),
            )
        })?;

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

    let etag = file_storage
        .upload_part(backend_upload_id, &record.path, part_number, &body, &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to upload part: {}",
                    e
                ))),
            )
        })?;

    let part_size = body.len() as i64;

    file_upload::record_part(
        &db,
        upload_id,
        org_id,
        &PartInfo {
            part_number,
            size: part_size,
            etag: etag.clone(),
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to record part: {}",
                e
            ))),
        )
    })?;

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

    let record = file_upload::get_upload(&db, upload_id, org_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("Upload: {}", e))),
            )
        })?;

    if record.status != "pending" {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "upload_not_pending",
                format!("Upload is in '{}' status, not 'pending'", record.status),
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

    file_storage
        .complete_multipart_upload(
            backend_upload_id,
            &record.path,
            &parts,
            record.bytes_received,
            record.content_type.as_deref(),
            &ctx,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to complete upload: {}",
                    e
                ))),
            )
        })?;

    file_upload::complete_upload(&db, upload_id, org_id)
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

    let file_record = file::get_file_by_path(&db, &record.path, org_id, Some(api_key.env_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to fetch file record: {}",
                    e
                ))),
            )
        })?;

    Ok(Json(ApiResponse::new(file_response_from_record(
        &file_record,
    ))))
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

    let record = file_upload::get_upload(&db, upload_id, org_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(&format!("Upload: {}", e))),
            )
        })?;

    if record.status != "pending" {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "upload_not_pending",
                format!("Upload is in '{}' status, not 'pending'", record.status),
            )),
        ));
    }

    if let Some(backend_upload_id) = record.backend_upload_id.as_deref() {
        let ctx = FileStorageContext::minimal(
            Arc::clone(&db),
            org_id,
            Some(api_key.env_id),
            api_key.created_by_user_id,
        );

        file_storage
            .abort_multipart_upload(backend_upload_id, &record.path, &ctx)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Failed to abort upload: {}",
                        e
                    ))),
                )
            })?;
    }

    file_upload::abort_upload(&db, upload_id, org_id)
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
