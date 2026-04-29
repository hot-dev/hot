//! Unified shutdown signal handling for all Hot services.
//!
//! Listens for both SIGINT (Ctrl+C) and SIGTERM (ECS/Docker graceful stop).
//! Without SIGTERM handling, PID 1 processes in containers silently ignore
//! the signal and get hard-killed after the `stopTimeout` window.

/// Wait for either SIGINT or SIGTERM.
///
/// On Unix, this listens for both signals. On other platforms, falls back to
/// `ctrl_c()` only.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(e) = result {
                    tracing::error!("Failed to listen for SIGINT: {}", e);
                }
            }
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C signal handler");
    }
}
