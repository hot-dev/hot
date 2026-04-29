use axum::{Router, routing::get};
use tracing::info;
use crate::val::Val;

pub const DEFAULT_API_PORT: u16 = 4681;

pub async fn run(conf: Val) {
    // Extract port from configuration
    let port = if let Some(Val::Int(p)) = conf.get("port") {
        p as u16
    } else {
        DEFAULT_API_PORT
    };

    let app = Router::<()>::new().route("/", get(handler));

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("hot.dev: API listening on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}

pub async fn handler() -> &'static str {
    "hot.dev api server"
}
