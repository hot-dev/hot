//! Input framing for `exit`.
//!
//! tower-lsp marks the service exited when the `exit` notification arrives,
//! but its read loop only notices on the next read, so a client that sends
//! `exit` and waits for the server to quit waits until it closes stdin. This
//! reader passes messages through unchanged and ends the input right after
//! an `exit` notification, so the server stops at once.

use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

pub struct ExitAware<R> {
    inner: R,
    /// Bytes read but not yet framed into a whole message.
    raw: Vec<u8>,
    /// The message being handed out, and how much of it has been.
    out: Vec<u8>,
    out_pos: usize,
    /// No more input after `out`.
    done: bool,
}

impl<R> ExitAware<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            raw: Vec::new(),
            out: Vec::new(),
            out_pos: 0,
            done: false,
        }
    }
}

/// The length of the first whole message in `raw`, and whether it is the
/// `exit` notification. Headers without a length pass through as they are.
fn next_message(raw: &[u8]) -> Option<(usize, bool)> {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers = String::from_utf8_lossy(&raw[..header_end]);
    let Some(length) = headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    }) else {
        return Some((header_end, false));
    };
    let end = header_end.checked_add(length)?;
    if raw.len() < end {
        return None;
    }
    let is_exit = serde_json::from_slice::<serde_json::Value>(&raw[header_end..end])
        .ok()
        .is_some_and(|message| {
            message.get("method").and_then(|m| m.as_str()) == Some("exit")
                && message.get("id").is_none()
        });
    Some((end, is_exit))
}

impl<R: AsyncRead + Unpin> AsyncRead for ExitAware<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.out_pos < this.out.len() {
                let n = buf.remaining().min(this.out.len() - this.out_pos);
                buf.put_slice(&this.out[this.out_pos..this.out_pos + n]);
                this.out_pos += n;
                return Poll::Ready(Ok(()));
            }
            if this.done {
                return Poll::Ready(Ok(()));
            }
            if let Some((length, is_exit)) = next_message(&this.raw) {
                this.out = this.raw.drain(..length).collect();
                this.out_pos = 0;
                this.done = is_exit;
                continue;
            }
            let mut chunk = [0u8; 16 * 1024];
            let mut read = ReadBuf::new(&mut chunk);
            match Pin::new(&mut this.inner).poll_read(cx, &mut read) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) if read.filled().is_empty() => {
                    // End of input: hand out what is left.
                    this.out = std::mem::take(&mut this.raw);
                    this.out_pos = 0;
                    this.done = true;
                }
                Poll::Ready(Ok(())) => this.raw.extend_from_slice(read.filled()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    #[tokio::test]
    async fn input_ends_after_exit() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = ExitAware::new(server);
        let first = frame(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        let exit = frame(r#"{"jsonrpc":"2.0","method":"exit"}"#);
        // Split mid-header to exercise reassembly; keep the pipe open.
        client.write_all(&first[..5]).await.unwrap();
        client.write_all(&first[5..]).await.unwrap();
        client.write_all(&exit).await.unwrap();
        let mut seen = Vec::new();
        reader.read_to_end(&mut seen).await.unwrap();
        assert_eq!(seen, [first, exit].concat());
    }
}
