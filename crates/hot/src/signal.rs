//! Unified shutdown signal handling for all Hot services.
//!
//! Listens for both SIGINT (Ctrl+C) and SIGTERM (ECS/Docker graceful stop).
//! Without SIGTERM handling, PID 1 processes in containers silently ignore
//! the signal and get hard-killed after the `stopTimeout` window.
//!
//! A single background listener fans shutdown out to every `shutdown_signal()`
//! waiter via a watch channel. SIGTERM always starts graceful shutdown.
//! Interactive policies such as "press Ctrl+C again to force quit" belong to
//! callers like `hot dev`; this shared module never exits the process itself.

use std::sync::OnceLock;

use tokio::sync::{broadcast, watch};
use tracing::info;

static SHUTDOWN_STATE: OnceLock<ShutdownState> = OnceLock::new();

struct ShutdownState {
    shutdown_tx: watch::Sender<bool>,
    signal_tx: broadcast::Sender<ShutdownSignal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

fn shutdown_state() -> &'static ShutdownState {
    SHUTDOWN_STATE.get_or_init(|| {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let (signal_tx, _signal_rx) = broadcast::channel(16);
        tokio::spawn(run_signal_listener(shutdown_tx.clone(), signal_tx.clone()));
        ShutdownState {
            shutdown_tx,
            signal_tx,
        }
    })
}

async fn run_signal_listener(
    shutdown_tx: watch::Sender<bool>,
    signal_tx: broadcast::Sender<ShutdownSignal>,
) {
    loop {
        let signal = wait_for_os_signal().await;

        let _ = signal_tx.send(signal);

        if *shutdown_tx.borrow() {
            info!(
                signal = ?signal,
                "hot: additional shutdown signal received; graceful shutdown already in progress"
            );
            continue;
        }

        shutdown_tx.send_replace(true);

        match signal {
            ShutdownSignal::Interrupt => {
                info!("hot: graceful shutdown requested by Ctrl+C");
            }
            ShutdownSignal::Terminate => {
                info!("hot: graceful shutdown requested by SIGTERM");
            }
        }
    }
}

async fn wait_for_os_signal() -> ShutdownSignal {
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
                ShutdownSignal::Interrupt
            }
            _ = sigterm.recv() => ShutdownSignal::Terminate
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C signal handler");
        ShutdownSignal::Interrupt
    }
}

/// Wait for either SIGINT or SIGTERM.
///
/// On Unix, this listens for both signals. On other platforms, falls back to
/// `ctrl_c()` only. Multiple concurrent callers all unblock on the first signal.
pub async fn shutdown_signal() {
    let tx = shutdown_state().shutdown_tx.clone();
    let mut rx = tx.subscribe();

    if *rx.borrow_and_update() {
        return;
    }

    let _ = rx.changed().await;
}

/// Subscribe to every OS shutdown signal observed by the process.
///
/// This is intended for interactive callers that need policy on top of the
/// shared graceful-shutdown trigger, such as `hot dev` handling repeated
/// Ctrl+C. Callers should subscribe before starting long-running services.
pub fn shutdown_signals() -> broadcast::Receiver<ShutdownSignal> {
    shutdown_state().signal_tx.subscribe()
}
