//! `hot api` and `hot app` — API server and combined runtime entry points.

use hot::stream::StreamPubSub;
use hot::val::Val;
use tracing::info;

use crate::Env;
use crate::build_info;

pub(crate) async fn run_api(env: Env, conf: Val) {
    run_api_with_stream_pubsub(env, conf, None).await
}

pub(crate) async fn run_api_with_stream_pubsub(
    env: Env,
    conf: Val,
    stream_pubsub: Option<std::sync::Arc<StreamPubSub>>,
) {
    info!(
        "hot.dev: API starting, version: {} ({})",
        build_info::VERSION,
        build_info::git_sha_short()
    );

    let server =
        tokio::spawn(
            async move { hot_api::server::run_with_stream_pubsub(conf, stream_pubsub).await },
        );

    // Wait for the server task to complete
    // The server has its own Ctrl-C handler that triggers graceful shutdown
    let _ = server.await;

    if env == Env::Production {
        info!("hot.dev: API shut down");
    }
}

pub(crate) async fn run_app(
    env: Env,
    conf: Val,
    stream_pubsub: Option<std::sync::Arc<StreamPubSub>>,
) {
    info!(
        "hot.dev: APP starting, version: {} ({})",
        build_info::VERSION,
        build_info::git_sha_short()
    );

    let server =
        tokio::spawn(
            async move { hot_app::server::run_with_stream_pubsub(conf, stream_pubsub).await },
        );

    // Wait for the server task to complete
    // The server has its own Ctrl-C handler that triggers graceful shutdown
    let _ = server.await;

    if env == Env::Production {
        info!("hot.dev: APP shut down");
    }
}
