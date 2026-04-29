//! Health check handlers

use axum::Json;
use serde_json::{Value, json};

pub async fn root_handler() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "hot.dev api server",
        "version": crate::build_info::VERSION,
        "git_sha": crate::build_info::git_sha_short(),
        "start_time": crate::build_info::start_time_iso()
    }))
}

pub async fn status_handler() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "hot.dev api server",
        "version": crate::build_info::VERSION,
        "git_sha": crate::build_info::git_sha_short(),
        "start_time": crate::build_info::start_time_iso()
    }))
}
