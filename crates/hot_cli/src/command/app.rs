//! `hot app` — combined runtime entry point.

use hot::stream::StreamPubSub;
use hot::val::Val;
use tracing::{error, info};

use crate::Env;
use crate::build_info;

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
    // The server has its own Ctrl-C handler that triggers graceful shutdown.
    // It panics on a fatal startup error; a standalone server then exits
    // non-zero so its supervisor sees the failure.
    if let Err(error) = server.await {
        error!("hot.dev: APP stopped unexpectedly: {error}");
        if env == Env::Production {
            std::process::exit(1);
        }
        return;
    }

    if env == Env::Production {
        info!("hot.dev: APP shut down");
    }
}
