#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port = std::env::var("HOT_DOCS_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(4688);
    let addr = format!("127.0.0.1:{port}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind hot_docs preview server");
    tracing::info!("hot_docs preview listening on http://{addr}");

    axum::serve(listener, hot_docs::preview_router())
        .await
        .expect("hot_docs preview server failed");
}
