use crate::auth::Session;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use hot::db::DatabasePool;
use hot::time_range::parse_time_range_cutoff;
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

const FILES_PER_PAGE: i64 = 20;

pub async fn files_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> (<env>) / Files
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Files".to_string()));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    // Parse time range filter
    let time_range_param = params.get("time_range");
    let selected_time_range: String = time_range_param
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string());

    // Parse search filter
    let search_param = params.get("search");
    let search_query: String = search_param
        .map(|s| s.to_string())
        .unwrap_or_else(String::new);

    // Calculate offset
    let offset = (current_page_num - 1) * FILES_PER_PAGE;

    let time_range_cutoff =
        parse_time_range_cutoff(time_range_param.map(|s| s.as_str()), chrono::Utc::now());

    // Convert search query to Option<&str>
    let search_term = if !search_query.is_empty() {
        Some(search_query.as_str())
    } else {
        None
    };

    // Get files for current environment
    let (files, total_files) = if let Some(env) = &session.current_env {
        let files = hot::db::file::list_files_by_env(
            &db,
            env.env_id,
            search_term,
            time_range_cutoff,
            Some(FILES_PER_PAGE),
            Some(offset),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get files by env {}: {}", env.env_id, e);
            Vec::new()
        });

        let total =
            hot::db::file::get_files_count_by_env(&db, env.env_id, search_term, time_range_cutoff)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("Failed to get files count by env {}: {}", env.env_id, e);
                    0
                });

        (files, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = if total_files > 0 {
        (total_files + FILES_PER_PAGE - 1) / FILES_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    // Convert FileRecords to FileDisplay
    let files_display: Vec<templates::FileDisplay> = files
        .iter()
        .map(|f| {
            templates::FileDisplay::from_with_timezone(
                f,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let template = templates::FilesList {
        title: "Files",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "files",
            &session,
            breadcrumbs,
        ),
        files: files_display,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_files,
        selected_time_range,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn file_detail_handler(
    Path(file_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get org_id and env_id for access check
    let org_id = match session.current_org_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/files").into_response();
        }
    };
    let env_id = session.current_env_id();

    // Get file details with org/env check
    match hot::db::file::get_file_by_id_secure(&db, file_id, org_id, env_id).await {
        Ok(file) => {
            // Build breadcrumbs: <org> (<env>) / Files / <file_path>
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Files".to_string(),
                "/files".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current(file.path.clone()));

            let file_display = templates::FileDisplay::from_with_timezone(
                &file,
                &session.display_timezone,
                &session.timezone_abbreviation,
            );

            let template = templates::FileDetail {
                title: &file.path,
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "files",
                    &session,
                    breadcrumbs,
                ),
                file: file_display,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // File not found, redirect to files list
            Redirect::to("/files").into_response()
        }
    }
}

pub async fn file_download_handler(
    Path(file_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get org_id and env_id for access check
    let org_id = match session.current_org_id() {
        Some(id) => id,
        None => {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("Access denied"))
                .unwrap();
        }
    };
    let env_id = session.current_env_id();

    // Get file details with org/env check
    let file = match hot::db::file::get_file_by_id_secure(&db, file_id, org_id, env_id).await {
        Ok(f) => f,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("File not found"))
                .unwrap();
        }
    };

    // Read file content from storage based on backend type
    let content = match file.storage_backend.as_str() {
        "local" => match tokio::fs::read(&file.storage_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("Failed to read local file {}: {}", file.storage_path, e);
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Failed to read file"))
                    .unwrap();
            }
        },
        "s3" => match read_s3_file(&file.storage_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("Failed to read S3 file {}: {}", file.storage_path, e);
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Failed to read file from storage"))
                    .unwrap();
            }
        },
        other => {
            tracing::error!("Unsupported storage backend for download: {}", other);
            return Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .body(Body::from(
                    "Download not supported for this storage backend",
                ))
                .unwrap();
        }
    };

    // Determine content type
    let content_type = file
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Extract filename from path
    let filename = file.path.split('/').next_back().unwrap_or(&file.path);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .header(header::CONTENT_LENGTH, content.len())
        .body(Body::from(content))
        .unwrap()
}

/// File data for JSON response
#[derive(Serialize)]
pub struct FileJson {
    pub file_id: String,
    pub path: String,
    pub size: i64,
    pub size_formatted: String,
    pub content_type: Option<String>,
    pub created_by_run_id: Option<String>,
    pub updated_by_run_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Response for run files API
#[derive(Serialize)]
pub struct RunFilesResponse {
    pub success: bool,
    pub files: Vec<FileJson>,
    pub total: i64,
}

/// GET /data/runs/{run_id}/files - Get files created or updated by a run
pub async fn run_files_handler(
    Path(run_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify the run belongs to the current org/env for security
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(RunFilesResponse {
                success: false,
                files: Vec::new(),
                total: 0,
            });
        }
    };

    match hot::db::Run::get_run(&db, &run_id).await {
        Ok(run) if run.env_id == env_id => {}
        Ok(_) | Err(_) => {
            return Json(RunFilesResponse {
                success: false,
                files: Vec::new(),
                total: 0,
            });
        }
    }

    // Fetch files for this run
    let files = hot::db::file::list_files_by_run(&db, run_id, Some(100), None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get files for run {}: {}", run_id, e);
            Vec::new()
        });

    // Filter files to only show those in the current environment (security check)
    let files: Vec<_> = files
        .into_iter()
        .filter(|f| f.env_id == Some(env_id))
        .collect();

    let total = files.len() as i64;

    // Convert to JSON-serializable format
    let files_json: Vec<FileJson> = files
        .iter()
        .map(|f| {
            let size_formatted = if f.size < 1024 {
                format!("{} B", f.size)
            } else if f.size < 1024 * 1024 {
                format!("{:.1} KB", f.size as f64 / 1024.0)
            } else if f.size < 1024 * 1024 * 1024 {
                format!("{:.1} MB", f.size as f64 / (1024.0 * 1024.0))
            } else {
                format!("{:.1} GB", f.size as f64 / (1024.0 * 1024.0 * 1024.0))
            };

            FileJson {
                file_id: f.file_id.to_string(),
                path: f.path.clone(),
                size: f.size,
                size_formatted,
                content_type: f.content_type.clone(),
                created_by_run_id: f.created_by_run_id.map(|id| id.to_string()),
                updated_by_run_id: f.updated_by_run_id.map(|id| id.to_string()),
                created_at: crate::timezone::format_in_timezone(
                    &f.created_at,
                    &session.display_timezone,
                    "%Y-%m-%d %H:%M:%S",
                ) + " "
                    + &session.timezone_abbreviation,
                updated_at: crate::timezone::format_in_timezone(
                    &f.updated_at,
                    &session.display_timezone,
                    "%Y-%m-%d %H:%M:%S",
                ) + " "
                    + &session.timezone_abbreviation,
            }
        })
        .collect();

    Json(RunFilesResponse {
        success: true,
        files: files_json,
        total,
    })
}

/// Read a file from S3 given a storage_path like `s3://bucket/key`.
async fn read_s3_file(storage_path: &str) -> Result<Vec<u8>, String> {
    let path = storage_path
        .strip_prefix("s3://")
        .ok_or_else(|| format!("Invalid S3 storage path: {storage_path}"))?;

    let (bucket, key) = path
        .split_once('/')
        .ok_or_else(|| format!("Invalid S3 storage path (no key): {storage_path}"))?;

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let client = aws_sdk_s3::Client::new(&config);

    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| format!("S3 GetObject failed: {e}"))?;

    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|e| format!("S3 body read failed: {e}"))?;

    Ok(bytes.to_vec())
}
