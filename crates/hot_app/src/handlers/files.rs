use crate::auth::Session;
use crate::handlers::list_query;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use hot::db::DatabasePool;
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

    let page = list_query::PageParams::parse(&params, FILES_PER_PAGE);
    let query_filters = list_query::SearchAndTimeRange::parse(&params, chrono::Utc::now());
    let search_term = query_filters.search_term();

    // Get files for current environment
    let (files, total_files) = if let Some(env) = &session.current_env {
        let files = hot::db::file::list_files_by_env(
            &db,
            env.env_id,
            search_term,
            query_filters.time_range_cutoff,
            Some(FILES_PER_PAGE),
            Some(page.offset),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get files by env {}: {}", env.env_id, e);
            Vec::new()
        });

        let total = hot::db::file::get_files_count_by_env(
            &db,
            env.env_id,
            search_term,
            query_filters.time_range_cutoff,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get files count by env {}: {}", env.env_id, e);
            0
        });

        (files, total)
    } else {
        (Vec::new(), 0)
    };

    let pagination = list_query::PaginationWindow::new(total_files, &page);

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
        current_page_num: page.current_page_num,
        total_pages: pagination.total_pages,
        start_page: pagination.start_page,
        end_page: pagination.end_page,
        has_next_page: pagination.has_next_page,
        has_prev_page: pagination.has_prev_page,
        total_files,
        selected_time_range: query_filters.selected_time_range,
        search_query: query_filters.search_query,
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
        Err(_) => super::not_found_page(&session, "File", "/files", "Back to files"),
    }
}

pub async fn file_download_handler(
    Path(file_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    axum::extract::Extension(conf): axum::extract::Extension<hot::val::Val>,
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

    // Read through the configured storage backend. It re-authorizes the
    // path against the database and refuses records owned by a different
    // backend instead of reading them with ambient credentials.
    let storage = match download_storage(&conf).await {
        Ok(storage) => storage,
        Err(e) => {
            tracing::error!("File storage unavailable for download: {}", e);
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("File storage unavailable"))
                .unwrap();
        }
    };
    let ctx = hot::file_storage::FileStorageContext::minimal(
        db.clone(),
        file.org_id,
        file.env_id,
        file.created_by_user_id,
    );
    let (body, content_length) = match storage.open_file_stream(&file.path, &ctx).await {
        Ok(Some(stream)) => (
            Body::from_stream(reader_stream(stream.reader)),
            stream.metadata.size,
        ),
        Ok(None) => match storage.read_file(&file.path, &ctx).await {
            Ok(bytes) => {
                let len = bytes.len() as i64;
                (Body::from(bytes), len)
            }
            Err(e) => {
                tracing::error!("Failed to read file {}: {}", file_id, e);
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Failed to read file from storage"))
                    .unwrap();
            }
        },
        Err(e) => {
            tracing::error!("Failed to open file {}: {}", file_id, e);
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Failed to read file from storage"))
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
        .header(header::CONTENT_LENGTH, content_length)
        .body(body)
        .unwrap()
}

/// File storage shared by download requests (clients hold connection pools).
static DOWNLOAD_STORAGE: tokio::sync::OnceCell<Arc<dyn hot::file_storage::FileStorage>> =
    tokio::sync::OnceCell::const_new();

async fn download_storage(
    conf: &hot::val::Val,
) -> Result<Arc<dyn hot::file_storage::FileStorage>, String> {
    DOWNLOAD_STORAGE
        .get_or_try_init(|| async {
            hot::file_storage::file_storage_from_config(conf)
                .await
                .map(Arc::from)
        })
        .await
        .cloned()
}

/// Forward an async reader as a body stream in bounded chunks.
fn reader_stream(
    reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send {
    futures::stream::unfold(reader, |mut reader| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), reader))
            }
            Err(e) => Some((Err(e), reader)),
        }
    })
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
