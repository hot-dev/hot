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

/// Store text that is safe for Postgres JSONB, within the output budget.
/// Both Docker and FIFO output can contain arbitrary bytes.
fn append_log(buf: &mut String, bytes: &[u8]) {
    if buf.len() >= MAX_LOG_BYTES {
        // Output that filled the budget exactly was whole until more came.
        if !bytes.is_empty() && !buf.ends_with(TRUNCATION_NOTICE) {
            buf.push_str(TRUNCATION_NOTICE);
        }
        return;
    }
    let text = String::from_utf8_lossy(bytes);
    let text = if text.contains('\0') {
        std::borrow::Cow::Owned(text.replace('\0', "\u{fffd}"))
    } else {
        text
    };
    let remaining = MAX_LOG_BYTES - buf.len();
    if text.len() <= remaining {
        buf.push_str(&text);
    } else {
        let mut end = remaining;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        buf.push_str(&text[..end]);
        buf.push_str(TRUNCATION_NOTICE);
    }
}

/// One stream's output as it arrives in pieces. A piece can end partway
/// through a multibyte character, so its incomplete tail waits for the next
/// piece instead of decoding to U+FFFD on its own.
#[derive(Default)]
struct LogStream {
    pending: Vec<u8>,
}

impl LogStream {
    fn push(&mut self, buf: &mut String, bytes: &[u8]) {
        if self.pending.is_empty() {
            let keep = incomplete_utf8_tail(bytes);
            append_log(buf, &bytes[..bytes.len() - keep]);
            self.pending.extend_from_slice(&bytes[bytes.len() - keep..]);
        } else {
            let mut joined = std::mem::take(&mut self.pending);
            joined.extend_from_slice(bytes);
            let keep = incomplete_utf8_tail(&joined);
            append_log(buf, &joined[..joined.len() - keep]);
            self.pending = joined.split_off(joined.len() - keep);
        }
    }

    /// The stream ended: a tail still incomplete is invalid, and says so.
    fn finish(&mut self, buf: &mut String) {
        let pending = std::mem::take(&mut self.pending);
        append_log(buf, &pending);
    }
}

/// How many trailing bytes of `bytes` begin a UTF-8 character they don't
/// finish (at most 3).
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    let start = bytes.len().saturating_sub(3);
    for i in (start..bytes.len()).rev() {
        // The last byte that isn't a continuation byte starts the last character.
        if bytes[i] & 0xC0 != 0x80 {
            return match std::str::from_utf8(&bytes[i..]) {
                Err(error) if error.error_len().is_none() => bytes.len() - i,
                _ => 0,
            };
        }
    }
    0
}

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
            let (mut out, mut err) = (LogStream::default(), LogStream::default());
            while let Some(log_result) = log_stream.next().await {
                if let Ok(log) = log_result {
                    match log {
                        LogOutput::StdOut { message } => {
                            let mut buf = stdout_clone.lock().await;
                            out.push(&mut buf, &message);
                        }
                        LogOutput::StdErr { message } => {
                            let mut buf = stderr_clone.lock().await;
                            err.push(&mut buf, &message);
                        }
                        _ => {}
                    }
                }
            }
            out.finish(&mut *stdout_clone.lock().await);
            err.finish(&mut *stderr_clone.lock().await);
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
    let mut stream = LogStream::default();
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut locked = buf.blocking_lock();
                stream.push(&mut locked, &chunk[..n]);
                // Output that only fills the budget may be all there is:
                // read on, and stop once more arrives and is cut.
                if locked.ends_with(TRUNCATION_NOTICE) {
                    return;
                }
            }
            Err(e) => {
                let mut locked = buf.blocking_lock();
                stream.finish(&mut locked);
                locked.push_str(&format!(
                    "[hot-task-worker] failed reading {} fifo '{}': {}\n",
                    stream_name, path, e
                ));
                return;
            }
        }
    }
    stream.finish(&mut buf.blocking_lock());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_output_is_safe_text() {
        let mut buf = String::new();
        append_log(&mut buf, b"before\0after\xff");
        assert_eq!(buf, "before\u{fffd}after\u{fffd}");
        assert!(!buf.contains('\0'));
    }

    #[test]
    fn truncation_respects_utf8_and_replacement_expansion() {
        for bytes in ["🙂".as_bytes(), b"\0"] {
            let mut buf = "a".repeat(MAX_LOG_BYTES - 1);
            append_log(&mut buf, bytes);
            assert!(buf.ends_with(TRUNCATION_NOTICE));
            assert!(!buf.contains('\0'));
            let truncated_len = buf.len();
            append_log(&mut buf, b"ignored");
            assert_eq!(buf.len(), truncated_len);
        }
    }

    #[test]
    fn output_that_exactly_fills_the_budget_is_marked_once_more_arrives() {
        let mut buf = String::new();
        append_log(&mut buf, "a".repeat(MAX_LOG_BYTES).as_bytes());
        assert_eq!(buf.len(), MAX_LOG_BYTES, "whole output is not marked cut");
        append_log(&mut buf, b"");
        assert!(!buf.ends_with(TRUNCATION_NOTICE));
        append_log(&mut buf, b"more");
        assert!(buf.ends_with(TRUNCATION_NOTICE));
        let truncated_len = buf.len();
        append_log(&mut buf, b"more");
        assert_eq!(buf.len(), truncated_len, "the notice is added once");
    }

    #[test]
    fn a_character_split_across_chunks_decodes_whole() {
        let text = "héllo 🙂 wörld ✓";
        let bytes = text.as_bytes();
        // Every split point, including inside each multibyte character.
        for split in 0..=bytes.len() {
            let (mut buf, mut stream) = (String::new(), LogStream::default());
            stream.push(&mut buf, &bytes[..split]);
            stream.push(&mut buf, &bytes[split..]);
            stream.finish(&mut buf);
            assert_eq!(buf, text, "split at {split}");
        }
        // One byte at a time.
        let (mut buf, mut stream) = (String::new(), LogStream::default());
        for byte in bytes {
            stream.push(&mut buf, std::slice::from_ref(byte));
        }
        stream.finish(&mut buf);
        assert_eq!(buf, text);
    }

    #[test]
    fn invalid_bytes_still_decode_as_replacements() {
        let (mut buf, mut stream) = (String::new(), LogStream::default());
        // A lone continuation byte and a bad lead are invalid now; a 4-byte
        // lead the stream never finishes is invalid at its end.
        stream.push(&mut buf, b"a\x80b\xffc");
        stream.push(&mut buf, b"d\xf0\x9f");
        assert_eq!(buf, "a\u{fffd}b\u{fffd}cd");
        stream.finish(&mut buf);
        assert_eq!(buf, "a\u{fffd}b\u{fffd}cd\u{fffd}");
    }
}
