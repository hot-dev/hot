//! Log accumulator for streaming container output capture
//!
//! Provides real-time log capture during container/VM execution,
//! ensuring partial output is preserved on timeout or crash.
//!
//! ## Backend support
//!
//! - **Docker**: Uses bollard's `docker.logs()` with `follow: true`
//! - **Kata/Firecracker**: Uses FIFO-based IO from containerd task creation

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::query_parameters::LogsOptionsBuilder;
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const FINALIZE_TIMEOUT_SECS: u64 = 5;

/// Per-stream cap to prevent unbounded memory growth from container output.
const MAX_LOG_BYTES: usize = 10 * 1024 * 1024; // 10 MB
const TRUNCATION_NOTICE: &str = "\n[hot-task-worker] output truncated at 10 MB\n";

/// Accumulates stdout/stderr from a container in real time.
///
/// Captures output as it's produced so partial output is available
/// even if the container times out or crashes.
pub struct LogAccumulator {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    handle: JoinHandle<()>,
}

impl LogAccumulator {
    /// Create from a Docker container's log stream (`follow: true`).
    pub fn from_docker(docker: &Docker, container_id: &str) -> Self {
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));

        let log_options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .follow(true)
            .build();

        let mut log_stream = docker.logs(container_id, Some(log_options));
        let stdout_clone = stdout.clone();
        let stderr_clone = stderr.clone();

        let handle = tokio::spawn(async move {
            while let Some(log_result) = log_stream.next().await {
                if let Ok(log) = log_result {
                    match log {
                        LogOutput::StdOut { message } => {
                            let mut buf = stdout_clone.lock().await;
                            if buf.len() < MAX_LOG_BYTES {
                                let chunk = String::from_utf8_lossy(&message);
                                let remaining = MAX_LOG_BYTES - buf.len();
                                if chunk.len() <= remaining {
                                    buf.push_str(&chunk);
                                } else {
                                    buf.push_str(&chunk[..remaining]);
                                    buf.push_str(TRUNCATION_NOTICE);
                                }
                            }
                        }
                        LogOutput::StdErr { message } => {
                            let mut buf = stderr_clone.lock().await;
                            if buf.len() < MAX_LOG_BYTES {
                                let chunk = String::from_utf8_lossy(&message);
                                let remaining = MAX_LOG_BYTES - buf.len();
                                if chunk.len() <= remaining {
                                    buf.push_str(&chunk);
                                } else {
                                    buf.push_str(&chunk[..remaining]);
                                    buf.push_str(TRUNCATION_NOTICE);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        Self {
            stdout,
            stderr,
            handle,
        }
    }

    /// Create from host-side FIFOs (Firecracker/containerd).
    #[cfg(all(target_os = "linux", feature = "kata"))]
    pub fn from_fifos(stdout_path: String, stderr_path: String) -> Self {
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));

        let stdout_clone = stdout.clone();
        let stderr_clone = stderr.clone();

        let handle = tokio::spawn(async move {
            let stdout_task = tokio::task::spawn_blocking(move || {
                read_fifo_stream(&stdout_path, stdout_clone, "stdout");
            });
            let stderr_task = tokio::task::spawn_blocking(move || {
                read_fifo_stream(&stderr_path, stderr_clone, "stderr");
            });

            let _ = tokio::join!(stdout_task, stderr_task);
        });

        Self {
            stdout,
            stderr,
            handle,
        }
    }

    /// Snapshot current accumulated output without consuming the accumulator.
    pub async fn snapshot(&self) -> (String, String) {
        let stdout = self.stdout.lock().await.clone();
        let stderr = self.stderr.lock().await.clone();
        (stdout, stderr)
    }

    /// Wait for the log stream to complete and return final output.
    pub async fn finalize(self) -> (String, String) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(FINALIZE_TIMEOUT_SECS),
            self.handle,
        )
        .await;

        let stdout = self.stdout.lock().await.clone();
        let stderr = self.stderr.lock().await.clone();
        (stdout, stderr)
    }
}

#[cfg(all(target_os = "linux", feature = "kata"))]
fn read_fifo_stream(path: &str, buf: Arc<Mutex<String>>, stream_name: &str) {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            buf.blocking_lock().push_str(&format!(
                "[hot-task-worker] failed to open {} fifo '{}': {}\n",
                stream_name, path, e
            ));
            return;
        }
    };

    let mut chunk = [0u8; 4096];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut locked = buf.blocking_lock();
                if locked.len() >= MAX_LOG_BYTES {
                    break;
                }
                let text = String::from_utf8_lossy(&chunk[..n]);
                let remaining = MAX_LOG_BYTES - locked.len();
                if text.len() <= remaining {
                    locked.push_str(&text);
                } else {
                    locked.push_str(&text[..remaining]);
                    locked.push_str(TRUNCATION_NOTICE);
                    break;
                }
            }
            Err(e) => {
                buf.blocking_lock().push_str(&format!(
                    "[hot-task-worker] failed reading {} fifo '{}': {}\n",
                    stream_name, path, e
                ));
                break;
            }
        }
    }
}
