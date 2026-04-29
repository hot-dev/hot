use crate::auth::Session;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use hot::db::{Build, DatabasePool, Project};
use hot::val::Val;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct SourceFileQuery {
    pub path: String,
    pub line: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SourceSearchQuery {
    pub q: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub regex: bool,
}

pub async fn source_tree_handler(
    Path(build_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let build = match load_accessible_build(&db, &session, &build_id).await {
        Ok(build) => build,
        Err(response) => return response,
    };

    match crate::source_browser::list_source_files(&db, &conf, &build).await {
        Ok(tree) => Json(tree).into_response(),
        Err(e) => source_error(StatusCode::BAD_REQUEST, &e),
    }
}

pub async fn source_file_handler(
    Path(build_id): Path<Uuid>,
    Query(query): Query<SourceFileQuery>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let build = match load_accessible_build(&db, &session, &build_id).await {
        Ok(build) => build,
        Err(response) => return response,
    };

    match crate::source_browser::read_source_file(&db, &conf, &build, &query.path, query.line).await
    {
        Ok(file) => Json(file).into_response(),
        Err(e) => source_error(StatusCode::NOT_FOUND, &e),
    }
}

pub async fn source_search_handler(
    Path(build_id): Path<Uuid>,
    Query(query): Query<SourceSearchQuery>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let build = match load_accessible_build(&db, &session, &build_id).await {
        Ok(build) => build,
        Err(response) => return response,
    };

    match crate::source_browser::search_source_files(
        &db,
        &conf,
        &build,
        &query.q,
        query.case_sensitive,
        query.regex,
    )
    .await
    {
        Ok(results) => Json(results).into_response(),
        Err(e) => source_error(StatusCode::BAD_REQUEST, &e),
    }
}

async fn load_accessible_build(
    db: &DatabasePool,
    session: &Session,
    build_id: &Uuid,
) -> Result<Build, axum::response::Response> {
    let current_env_id = session
        .current_env_id()
        .ok_or_else(|| source_error(StatusCode::FORBIDDEN, "environment not selected"))?;
    let build = Build::get_build(db, build_id)
        .await
        .map_err(|_| source_error(StatusCode::NOT_FOUND, "build not found"))?;
    let project = Project::get_project(db, &build.project_id)
        .await
        .map_err(|_| source_error(StatusCode::NOT_FOUND, "project not found"))?;

    if project.env_id != current_env_id || !session.has_env_access(&project.env_id) {
        return Err(source_error(StatusCode::FORBIDDEN, "access denied"));
    }

    Ok(build)
}

fn source_error(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(json!({ "error": message }))).into_response()
}
