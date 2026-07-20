//! Build management handlers

use axum::{
    Extension, Json,
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use hot::db::{api_key::ApiKey, build::Build, project::Project};
use hot::storage::build_zip_filename;
use hot::val::Val;
use once_cell::sync::Lazy;
use std::str::FromStr;
use std::sync::{Arc, Mutex, Weak};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use super::{ListQueryParams, get_and_ensure_active_project, get_and_verify_project};
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

const DEFAULT_BUILD_UPLOAD_CONCURRENCY: i64 = 16;
static BUILD_UPLOAD_SEMAPHORES: Lazy<Mutex<ahash::AHashMap<usize, Weak<Semaphore>>>> =
    Lazy::new(|| Mutex::new(ahash::AHashMap::new()));

fn acquire_build_upload_slot(
    conf: &Val,
) -> Result<Option<OwnedSemaphorePermit>, (StatusCode, Json<ApiErrorResponse>)> {
    let limit = conf.get_int_or_default(
        "api.build-upload-concurrency-limit",
        DEFAULT_BUILD_UPLOAD_CONCURRENCY,
    );
    if limit <= 0 {
        return Ok(None);
    }
    let limit = limit as usize;
    let semaphore = {
        let mut semaphores = BUILD_UPLOAD_SEMAPHORES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(semaphore) = semaphores.get(&limit).and_then(Weak::upgrade) {
            semaphore
        } else {
            let semaphore = Arc::new(Semaphore::new(limit));
            semaphores.insert(limit, Arc::downgrade(&semaphore));
            semaphore
        }
    };
    semaphore.try_acquire_owned().map(Some).map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(
                ApiErrorResponse::new(
                    "rate_limit_exceeded",
                    "Too many build uploads are currently running. Retry after 1 second.",
                )
                .with_retry_after(1),
            ),
        )
    })
}

fn build_runtime_warning_response(build: &Build) -> Option<BuildRuntimeWarningResponse> {
    build
        .runtime_version_warning()
        .map(|warning| BuildRuntimeWarningResponse {
            build_engine_version: warning.build_engine_version,
            build_hot_std_version: warning.build_hot_std_version,
            runtime_version: warning.runtime_version,
            message: warning.message,
        })
}

fn build_response(build: Build, project_id: Uuid) -> BuildResponse {
    let runtime_warning = build_runtime_warning_response(&build);
    BuildResponse {
        build_id: build.build_id,
        project_id,
        hash: build.hash,
        size: build.size,
        build_type: build.build_type,
        deployed: build.deployed,
        active: build.active,
        runtime_status: build.runtime_status,
        runtime_error: build.runtime_error,
        deployment_sequence: build.deployment_sequence,
        created_at: build.created_at,
        updated_at: build.updated_at,
        storage_path: build.storage_path,
        storage_backend: build.storage_backend,
        runtime_warning,
    }
}

fn build_with_project_response(build: Build, project_name: String) -> BuildWithProjectResponse {
    let runtime_warning = build_runtime_warning_response(&build);
    BuildWithProjectResponse {
        build_id: build.build_id,
        project_id: build.project_id,
        project_name,
        hash: build.hash,
        size: build.size,
        build_type: build.build_type,
        deployed: build.deployed,
        active: build.active,
        runtime_status: build.runtime_status,
        runtime_error: build.runtime_error,
        deployment_sequence: build.deployment_sequence,
        created_at: build.created_at,
        updated_at: build.updated_at,
        storage_path: build.storage_path,
        storage_backend: build.storage_backend,
        runtime_warning,
    }
}

pub async fn list_builds(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list builds.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let mut builds = Build::get_builds_by_project(
        &db,
        &project.project_id,
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
    Build::hydrate_manifest_versions_for_builds(&db, &mut builds)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let total = Build::get_count_by_project(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let build_responses: Vec<BuildResponse> = builds
        .into_iter()
        .map(|b| build_response(b, project.project_id))
        .collect();

    Ok(Json(ApiListResponse::new(
        build_responses,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn list_builds_by_env(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<BuildWithProjectResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list builds.")?;

    let mut builds = Build::get_builds_by_env(
        &db,
        &api_key.env_id,
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
    Build::hydrate_manifest_versions_for_builds(&db, &mut builds)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let total = Build::get_count_by_env(&db, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    // Fetch project info for each build
    let mut build_responses = Vec::new();
    for build in builds {
        // Get project for this build
        let project = Project::get_project(&db, &build.project_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?;

        build_responses.push(build_with_project_response(build, project.name));
    }

    Ok(Json(ApiListResponse::new(
        build_responses,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn get_build(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path((project_id_or_slug, build_id)): Path<(String, Uuid)>,
) -> Result<Json<ApiResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read builds.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let mut build = Build::get_build(&db, &build_id)
        .await
        .map_err(|e| match e {
            hot::db::build::BuildError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Build")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;
    Build::hydrate_manifest_versions(&db, &mut build)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    // Verify the build belongs to this project
    if build.project_id != project.project_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Build")),
        ));
    }

    Ok(Json(ApiResponse::new(build_response(
        build,
        project.project_id,
    ))))
}

pub async fn get_deployed_build(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
) -> Result<Json<ApiResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read builds.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let build = Build::get_deployed_build_by_project(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let mut build = build.ok_or((
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::not_found("No deployed build found")),
    ))?;
    Build::hydrate_manifest_versions(&db, &mut build)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse::new(build_response(
        build,
        project.project_id,
    ))))
}

pub async fn get_live_build(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
) -> Result<Json<ApiResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read builds.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let build = Build::get_live_build_by_project(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let mut build = build.ok_or((
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::not_found("No live build found")),
    ))?;
    Build::hydrate_manifest_versions(&db, &mut build)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse::new(build_response(
        build,
        project.project_id,
    ))))
}

pub async fn deploy_build(
    State((db, storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path((project_id_or_slug, build_id)): Path<(String, Uuid)>,
) -> Result<Json<ApiResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can deploy builds.")?;

    let project = get_and_ensure_active_project(&db, &api_key, &project_id_or_slug).await?;

    // Get the build and verify it belongs to this project
    let build = Build::get_build(&db, &build_id)
        .await
        .map_err(|e| match e {
            hot::db::build::BuildError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Build")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    // Verify the build belongs to this project
    if build.project_id != project.project_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Build")),
        ));
    }

    // Validate ctx requirements for bundle builds before deploying
    if build.is_bundle() {
        let org_id = super::get_org_id_for_env(&db, &api_key.env_id).await?;
        tracing::debug!(
            "Validating ctx requirements for build {} before deploy",
            build_id
        );
        // Strict mode is opt-in via the `HOT_DEPLOY_STRICT_CTX` environment
        // variable on the API process. Default is warn-only — the API will
        // log a structured warning but still accept the deploy. Teams that
        // want the hard gate can flip the env var on per-environment.
        let strict = std::env::var("HOT_DEPLOY_STRICT_CTX")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        if let Err(e) = hot::build::validate_ctx_requirements_for_deploy(
            &db,
            &build_id,
            &project.project_id,
            &org_id,
            &api_key.env_id,
            storage.as_ref().as_ref(),
            strict,
        )
        .await
        {
            tracing::warn!("Deploy blocked due to missing ctx vars: {}", e);
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&e)),
            ));
        }
        tracing::debug!("Ctx requirements validation passed for build {}", build_id);

        if let Err(e) = hot::build::validate_box_requirements_for_deploy(
            &db,
            &build_id,
            &org_id,
            &api_key.env_id,
            storage.as_ref().as_ref(),
        )
        .await
        {
            tracing::warn!("Deploy blocked due to box resource requirements: {}", e);
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&e)),
            ));
        }
        tracing::debug!("Box requirements validation passed for build {}", build_id);

        if let Err(e) = hot::build::validate_schedule_requirements_for_deploy(
            &db,
            &build_id,
            &org_id,
            &api_key.env_id,
            &_conf,
            storage.as_ref().as_ref(),
        )
        .await
        {
            tracing::warn!("Deploy blocked due to schedule limits: {}", e);
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&e)),
            ));
        }
        tracing::debug!(
            "Schedule requirements validation passed for build {}",
            build_id
        );
    }

    if build.is_bundle() {
        Build::request_bundle_deployment(&db, &build_id, &api_key.created_by_user_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?;
    } else {
        Build::activate_build_directly(&db, &build_id, &api_key.created_by_user_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?;
    }

    if build.is_bundle() {
        // Enqueue deployment message so a worker prepares the bundle and performs
        // final activation after manifest/runtime data has loaded.
        if let Err(e) = hot::lang::event::enqueue_deployment_message(&_conf, build_id).await {
            let runtime_error = format!("Failed to enqueue deployment message: {e}");
            let _ = Build::mark_runtime_failed(&db, &build_id, &runtime_error).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e)),
            ));
        }

        tracing::info!(
            "Bundle build {} accepted and queued for worker activation",
            build_id
        );
    } else {
        tracing::info!("Live build {} activated directly", build_id);
    }

    // Get the updated build
    let mut deployed_build = Build::get_build(&db, &build_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;
    Build::hydrate_manifest_versions(&db, &mut deployed_build)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse::new(build_response(
        deployed_build,
        project.project_id,
    ))))
}

pub async fn upload_build(
    State((db, storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    mut multipart: Multipart,
) -> Result<
    (
        StatusCode,
        axum::http::HeaderMap,
        Json<ApiResponse<BuildUploadResponse>>,
    ),
    (StatusCode, Json<ApiErrorResponse>),
> {
    super::require_api_key(&auth, "Only API keys can upload builds.")?;

    let project = get_and_ensure_active_project(&db, &api_key, &project_id_or_slug).await?;
    let _upload_permit = acquire_build_upload_slot(&conf)?;

    // The multipart body is streamed to a request-scoped temporary directory.
    // Memory use stays bounded to one multipart chunk rather than the bundle.
    let staging_dir = tempfile::tempdir().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to create build upload staging directory: {}",
                e
            ))),
        )
    })?;
    let build_path = staging_dir.path().join("build.hot.zip");
    let mut build_file_size: Option<usize> = None;
    let mut provided_hash: Option<String> = None;
    let mut provided_build_id: Option<String> = None;

    // The default matches the CLI preflight ceiling.
    let max_build_size = conf
        .get("build")
        .and_then(|b| b.get("file"))
        .and_then(|f| f.get("max-bytes"))
        .and_then(|v| match v {
            Val::Int(i) => Some(i.max(0) as usize),
            _ => None,
        })
        .unwrap_or(hot::build::DEFAULT_REMOTE_BUILD_MAX_BYTES as usize);

    // Parse multipart form data
    loop {
        let Some(mut field) = multipart.next_field().await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&format!(
                    "Failed to read multipart form: {}",
                    e
                ))),
            )
        })?
        else {
            break;
        };
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                let mut staged = tokio::fs::File::create(&build_path).await.map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiErrorResponse::internal_error(&format!(
                            "Failed to stage build upload: {}",
                            e
                        ))),
                    )
                })?;
                let mut size = 0usize;
                while let Some(chunk) = field.chunk().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ApiErrorResponse::bad_request(&format!(
                            "Failed to read file: {}",
                            e
                        ))),
                    )
                })? {
                    size = size.checked_add(chunk.len()).ok_or_else(|| {
                        (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            Json(ApiErrorResponse::bad_request("Build file too large")),
                        )
                    })?;
                    if size > max_build_size {
                        tracing::warn!(
                            size,
                            max_build_size,
                            project = %project.name,
                            "Build upload rejected while streaming oversized file"
                        );
                        return Err((
                            StatusCode::PAYLOAD_TOO_LARGE,
                            Json(ApiErrorResponse::bad_request(&format!(
                                "Build file too large (max {} bytes)",
                                max_build_size
                            ))),
                        ));
                    }
                    staged.write_all(&chunk).await.map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ApiErrorResponse::internal_error(&format!(
                                "Failed to stage build upload: {}",
                                e
                            ))),
                        )
                    })?;
                }
                staged.flush().await.map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiErrorResponse::internal_error(&format!(
                            "Failed to finish staging build upload: {}",
                            e
                        ))),
                    )
                })?;
                build_file_size = Some(size);
            }
            "hash" => {
                let text = field.text().await.map_err(|e| {
                    tracing::error!(
                        "Build upload failed: could not read hash field for project {} by user {}: {}",
                        project.name,
                        api_key.created_by_user_id,
                        e
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ApiErrorResponse::bad_request(&format!(
                            "Failed to read hash: {}",
                            e
                        ))),
                    )
                })?;
                provided_hash = Some(text);
            }
            "build_id" => {
                let text = field.text().await.map_err(|e| {
                    tracing::error!(
                        "Build upload failed: could not read build_id field for project {} by user {}: {}",
                        project.name,
                        api_key.created_by_user_id,
                        e
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ApiErrorResponse::bad_request(&format!(
                            "Failed to read build_id: {}",
                            e
                        ))),
                    )
                })?;
                provided_build_id = Some(text);
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    // Validate we have required fields
    let file_size = build_file_size.ok_or_else(|| {
        tracing::error!(
            "Build upload failed: missing file field for project {} by user {}",
            project.name,
            api_key.created_by_user_id
        );
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(
                "Missing required field: file",
            )),
        )
    })?;

    let build_hash = provided_hash.ok_or_else(|| {
        tracing::error!(
            "Build upload failed: missing hash field for project {} by user {}",
            project.name,
            api_key.created_by_user_id
        );
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(
                "Missing required field: hash",
            )),
        )
    })?;

    let file_size = i32::try_from(file_size).map_err(|_| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiErrorResponse::bad_request(
                "Build file is too large to record",
            )),
        )
    })?;

    // Use provided build ID if available, otherwise generate new one
    let build_id = if let Some(provided_id_str) = provided_build_id {
        Uuid::from_str(&provided_id_str).map_err(|e| {
            tracing::error!(
                "Build upload failed: invalid build_id format '{}' for project {} by user {}: {}",
                provided_id_str,
                project.name,
                api_key.created_by_user_id,
                e
            );
            (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&format!(
                    "Invalid build_id format: {}",
                    e
                ))),
            )
        })?
    } else {
        Uuid::now_v7()
    };

    // Check if build already exists (idempotency check)
    // SECURITY: Must verify the existing build belongs to this project to prevent
    // information disclosure across environments
    if let Ok(existing_build) = Build::get_build(&db, &build_id).await {
        // Verify the existing build belongs to this project
        if existing_build.project_id == project.project_id {
            // Get environment to check storage
            let env = hot::db::Env::get_env(&db, &api_key.env_id)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to get environment for build upload: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiErrorResponse::internal_error(
                            "Failed to get environment",
                        )),
                    )
                })?;

            // Check if the build file exists in storage
            // In local dev with shared database, the record may exist but the file might not
            let file_exists_in_storage = storage
                .exists(&build_id, &env.org_id, &api_key.env_id)
                .await
                .unwrap_or(false);

            if file_exists_in_storage {
                tracing::debug!(
                    "Build {} already exists for project {} (in DB and storage), returning existing build (idempotent upload)",
                    build_id,
                    project.name
                );

                use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
                let mut headers = HeaderMap::new();
                headers.insert(
                    HeaderName::from_static("x-build-exists"),
                    HeaderValue::from_static("true"),
                );

                return Ok((
                    StatusCode::OK,
                    headers,
                    Json(ApiResponse::new(BuildUploadResponse {
                        build_id: existing_build.build_id,
                        project_id: existing_build.project_id,
                        hash: existing_build.hash,
                        size: existing_build.size,
                        storage_path: existing_build.storage_path.unwrap_or_default(),
                        storage_backend: existing_build.storage_backend.unwrap_or_default(),
                        created_at: existing_build.created_at,
                    })),
                ));
            } else {
                // Build record exists but file is missing from storage - store the file
                tracing::debug!(
                    "Build {} exists in DB but not in storage for project {}, storing file",
                    build_id,
                    project.name
                );

                let storage_path = storage
                    .store_build_from_path(&build_id, &env.org_id, &api_key.env_id, &build_path)
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            "Build upload failed: storage error for project {} by user {}: {}",
                            project.name,
                            api_key.created_by_user_id,
                            e
                        );
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ApiErrorResponse::internal_error(&format!(
                                "Failed to store build: {}",
                                e
                            ))),
                        )
                    })?;

                // Update the build record with storage info
                Build::update_build_storage(&db, &build_id, &storage_path, storage.storage_type())
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            "Failed to update build storage info for {}: {}",
                            build_id,
                            e
                        );
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ApiErrorResponse::internal_error(&format!(
                                "Failed to update build storage: {}",
                                e
                            ))),
                        )
                    })?;

                use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
                let mut headers = HeaderMap::new();
                headers.insert(
                    HeaderName::from_static("x-build-exists"),
                    HeaderValue::from_static("false"),
                );

                return Ok((
                    StatusCode::OK,
                    headers,
                    Json(ApiResponse::new(BuildUploadResponse {
                        build_id: existing_build.build_id,
                        project_id: existing_build.project_id,
                        hash: existing_build.hash,
                        size: existing_build.size,
                        storage_path,
                        storage_backend: storage.storage_type().to_string(),
                        created_at: existing_build.created_at,
                    })),
                ));
            }
        } else {
            // Build ID exists but belongs to different project - reject to prevent collisions
            tracing::warn!(
                "Build ID {} collision: requested for project {} but exists in different project",
                build_id,
                project.name
            );
            return Err((
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::bad_request(
                    "Build ID already exists in a different project",
                )),
            ));
        }
    }

    // Get environment to retrieve org_id for storage path
    let env = hot::db::Env::get_env(&db, &api_key.env_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get environment for build upload: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(
                    "Failed to get environment",
                )),
            )
        })?;

    // Store the build file with org/env context
    let storage_path = storage
        .store_build_from_path(&build_id, &env.org_id, &api_key.env_id, &build_path)
        .await
        .map_err(|e| {
            tracing::error!(
                "Build upload failed: storage error for project {} by user {}: {}",
                project.name,
                api_key.created_by_user_id,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to store build: {}",
                    e
                ))),
            )
        })?;

    let storage_backend = storage.storage_type().to_string();

    // Insert build record into database
    Build::insert_build_with_storage(
        &db,
        &build_id,
        &project.project_id,
        &build_hash,
        file_size,
        Build::BUILD_TYPE_BUNDLE,
        &api_key.created_by_user_id,
        Some(&storage_path),
        Some(&storage_backend),
    )
    .await
    .map_err(|e| {
        tracing::error!(
            "Build upload failed: database error for project {} by user {}: {}",
            project.name,
            api_key.created_by_user_id,
            e
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to create build record: {}",
                e
            ))),
        )
    })?;

    // Load event handlers and schedules from the build into the database
    tracing::info!(
        "Loading event handlers and schedules from build {}",
        build_id
    );
    if let Err(e) =
        hot::build::load_build_manifest_data_from_path(&db, &build_id, &api_key.env_id, &build_path)
            .await
    {
        tracing::error!(
            "Failed to load handlers/schedules for build {}: {}",
            build_id,
            e
        );
    }

    tracing::info!(
        "Build {} uploaded for project {} by user {}",
        build_id,
        project.name,
        api_key.created_by_user_id
    );

    // Return the build info
    let build = Build::get_build(&db, &build_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        axum::http::HeaderMap::new(),
        Json(ApiResponse::new(BuildUploadResponse {
            build_id: build.build_id,
            project_id: project.project_id,
            hash: build.hash,
            size: build.size,
            storage_path,
            storage_backend,
            created_at: build.created_at,
        })),
    ))
}

pub async fn download_build(
    State((db, storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path((project_id_or_slug, build_id)): Path<(String, Uuid)>,
) -> Result<Response, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can download builds.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    // Get the build and verify it belongs to this project
    let build = Build::get_build(&db, &build_id)
        .await
        .map_err(|e| match e {
            hot::db::build::BuildError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Build")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    // Verify the build belongs to this project
    if build.project_id != project.project_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Build")),
        ));
    }

    // Get environment to retrieve org_id for storage path
    let env = hot::db::Env::get_env(&db, &api_key.env_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get environment for build download: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(
                    "Failed to get environment",
                )),
            )
        })?;

    // Retrieve the build file from storage with org/env context
    let build_data = storage
        .retrieve_build(&build_id, &env.org_id, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to retrieve build: {}",
                    e
                ))),
            )
        })?;

    // Construct filename
    let filename = build_zip_filename(&build_id);

    tracing::info!(
        "Build {} downloaded for project {} by user {}",
        build_id,
        project.name,
        api_key.created_by_user_id
    );

    // Return as zip file
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                &format!("attachment; filename=\"{}\"", filename),
            ),
        ],
        build_data,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_upload_concurrency_limit_releases_on_drop() {
        let conf = hot::val!({
            "api": {
                "build-upload-concurrency-limit": 1,
            },
        });

        let permit = acquire_build_upload_slot(&conf)
            .expect("first upload should be admitted")
            .expect("enabled limit should return a permit");
        assert!(acquire_build_upload_slot(&conf).is_err());
        drop(permit);
        assert!(acquire_build_upload_slot(&conf).is_ok());
    }

    #[test]
    fn build_upload_concurrency_can_be_disabled() {
        let conf = hot::val!({
            "api": {
                "build-upload-concurrency-limit": 0,
            },
        });

        assert!(
            acquire_build_upload_slot(&conf)
                .expect("disabled limit should admit uploads")
                .is_none()
        );
    }
}
