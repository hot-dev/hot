//! `hot api` — API server entry point.

use hot::stream::StreamPubSub;
use hot::val::Val;
use tracing::{error, info};

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
    // The server has its own Ctrl-C handler that triggers graceful shutdown.
    // It panics on a fatal startup error; a standalone server then exits
    // non-zero so its supervisor sees the failure.
    if let Err(error) = server.await {
        error!("hot.dev: API stopped unexpectedly: {error}");
        if env == Env::Production {
            std::process::exit(1);
        }
        return;
    }

    if env == Env::Production {
        info!("hot.dev: API shut down");
    }
}
