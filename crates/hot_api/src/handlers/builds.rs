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
use std::str::FromStr;
use uuid::Uuid;

use super::{ListQueryParams, get_and_verify_project};
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

pub async fn list_builds(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list builds.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let builds = Build::get_builds_by_project(
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
        .map(|b| BuildResponse {
            build_id: b.build_id,
            project_id: project.project_id,
            hash: b.hash,
            size: b.size,
            build_type: b.build_type,
            deployed: b.deployed,
            active: b.active,
            runtime_status: b.runtime_status,
            runtime_error: b.runtime_error,
            deployment_sequence: b.deployment_sequence,
            created_at: b.created_at,
            updated_at: b.updated_at,
            storage_path: b.storage_path,
            storage_backend: b.storage_backend,
        })
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

    let builds = Build::get_builds_by_env(
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

        build_responses.push(BuildWithProjectResponse {
            build_id: build.build_id,
            project_id: build.project_id,
            project_name: project.name,
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
        });
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

    Ok(Json(ApiResponse::new(BuildResponse {
        build_id: build.build_id,
        project_id: project.project_id,
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
    })))
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

    let build = build.ok_or((
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::not_found("No deployed build found")),
    ))?;

    Ok(Json(ApiResponse::new(BuildResponse {
        build_id: build.build_id,
        project_id: project.project_id,
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
    })))
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

    let build = build.ok_or((
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::not_found("No live build found")),
    ))?;

    Ok(Json(ApiResponse::new(BuildResponse {
        build_id: build.build_id,
        project_id: project.project_id,
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
    })))
}

pub async fn deploy_build(
    State((db, storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path((project_id_or_slug, build_id)): Path<(String, Uuid)>,
) -> Result<Json<ApiResponse<BuildResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can deploy builds.")?;

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

    // Validate ctx requirements for bundle builds before deploying
    if build.is_bundle() {
        let org_id = super::get_org_id_for_env(&db, &api_key.env_id).await?;
        tracing::info!(
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
        tracing::info!("Ctx requirements validation passed for build {}", build_id);

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
        tracing::info!("Box requirements validation passed for build {}", build_id);

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
        tracing::info!(
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
        hot::lang::event::enqueue_deployment_message(&_conf, build_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e)),
                )
            })?;

        tracing::info!(
            "Bundle build {} accepted and queued for worker activation",
            build_id
        );
    } else {
        tracing::info!("Live build {} activated directly", build_id);
    }

    // Get the updated build
    let deployed_build = Build::get_build(&db, &build_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok(Json(ApiResponse::new(BuildResponse {
        build_id: deployed_build.build_id,
        project_id: project.project_id,
        hash: deployed_build.hash,
        size: deployed_build.size,
        build_type: deployed_build.build_type,
        deployed: deployed_build.deployed,
        active: deployed_build.active,
        runtime_status: deployed_build.runtime_status,
        runtime_error: deployed_build.runtime_error,
        deployment_sequence: deployed_build.deployment_sequence,
        created_at: deployed_build.created_at,
        updated_at: deployed_build.updated_at,
        storage_path: deployed_build.storage_path,
        storage_backend: deployed_build.storage_backend,
    })))
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

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let mut build_file_data: Option<Vec<u8>> = None;
    let mut provided_hash: Option<String> = None;
    let mut provided_build_id: Option<String> = None;

    // Parse multipart form data
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                let data = field.bytes().await.map_err(|e| {
                    tracing::error!(
                        "Build upload failed: could not read file field for project {} by user {}: {}",
                        project.name,
                        api_key.created_by_user_id,
                        e
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ApiErrorResponse::bad_request(&format!(
                            "Failed to read file: {}",
                            e
                        ))),
                    )
                })?;
                build_file_data = Some(data.to_vec());
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
    let build_data = build_file_data.ok_or_else(|| {
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

    // Validate file size
    let file_size = build_data.len() as i32;

    // Get max build size from config. The default must match
    // `hot::build::DEFAULT_REMOTE_BUILD_MAX_BYTES` so the CLI can
    // pre-flight against the same ceiling without having to query us.
    let max_build_size = conf
        .get("build")
        .and_then(|b| b.get("file"))
        .and_then(|f| f.get("max-bytes"))
        .and_then(|v| match v {
            Val::Int(i) => Some(i as usize),
            _ => None,
        })
        .unwrap_or(hot::build::DEFAULT_REMOTE_BUILD_MAX_BYTES as usize);

    if build_data.len() > max_build_size {
        tracing::error!(
            "Build upload rejected: file too large ({} bytes, max {} bytes) for project {} by user {}",
            build_data.len(),
            max_build_size,
            project.name,
            api_key.created_by_user_id
        );
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiErrorResponse::bad_request(&format!(
                "Build file too large (max {} bytes)",
                max_build_size
            ))),
        ));
    }

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
                tracing::info!(
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
                tracing::info!(
                    "Build {} exists in DB but not in storage for project {}, storing file",
                    build_id,
                    project.name
                );

                let storage_path = storage
                    .store_build(&build_id, &env.org_id, &api_key.env_id, build_data.clone())
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
        .store_build(&build_id, &env.org_id, &api_key.env_id, build_data.clone())
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
        hot::build::load_build_manifest_data(&db, &build_id, &api_key.env_id, &build_data).await
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
