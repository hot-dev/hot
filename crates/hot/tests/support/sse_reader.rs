//! Server-sent events read from a live response body. The suites in `hot_api`
//! and `hot_app` include this file by path.

use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;

/// Longest wait for a cross-instance delivery.
const DELIVERY: Duration = Duration::from_secs(10);

/// A task drains the body continuously, as the HTTP server does for a
/// connected client.
pub struct SseReader {
    events: mpsc::UnboundedReceiver<(String, String)>,
    task: tokio::task::JoinHandle<()>,
}

impl SseReader {
    pub fn new(response: Response) -> Self {
        assert_eq!(response.status(), StatusCode::OK, "SSE request failed");
        let (tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut body = response.into_body().into_data_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = body.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(event) = next_event(&mut buffer) {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
        });
        Self { events, task }
    }

    /// The data of the next event named `name`, skipping other events.
    pub async fn next(&mut self, name: &str) -> String {
        self.next_where(&format!("`{name}` event"), |event, _| event == name)
            .await
    }

    /// The data of the next event `wanted` accepts by name and data, skipping
    /// other events. `what` names it in the failure.
    pub async fn next_where(&mut self, what: &str, wanted: impl Fn(&str, &str) -> bool) -> String {
        let deadline = tokio::time::Instant::now() + DELIVERY;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some((event, data))) if wanted(&event, &data) => return data,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("SSE stream ended while waiting for {what}"),
                Err(_) => panic!("no {what} within {DELIVERY:?}"),
            }
        }
    }
}

impl Drop for SseReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Takes the next complete event that has data out of `buffer`: its name and
/// its joined data lines. An unfinished frame stays for the next chunk.
fn next_event(buffer: &mut String) -> Option<(String, String)> {
    while let Some(end) = buffer.find("\n\n") {
        let frame: String = buffer.drain(..end + 2).collect();
        let mut event = "message".to_string();
        let mut data = Vec::new();
        for line in frame.lines() {
            if let Some(name) = line.strip_prefix("event:") {
                event = name.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
            }
        }
        if !data.is_empty() {
            return Some((event, data.join("\n")));
        }
    }
    None
}

#[test]
fn frames_become_events_once_they_are_complete() {
    let event = |name: &str, data: &str| Some((name.to_string(), data.to_string()));
    let mut buffer = ": keep-alive\n\nevent: run:start\ndata: {\"a\":1}\n\ndata:no space\n\ndata: x\ndata:  y\n\nevent: late\ndata: par"
        .to_string();
    assert_eq!(
        next_event(&mut buffer),
        event("run:start", "{\"a\":1}"),
        "a frame without data is skipped"
    );
    assert_eq!(next_event(&mut buffer), event("message", "no space"));
    assert_eq!(next_event(&mut buffer), event("message", "x\n y"));
    assert_eq!(next_event(&mut buffer), None);
    assert_eq!(buffer, "event: late\ndata: par");

    buffer.push_str("t\n\n");
    assert_eq!(next_event(&mut buffer), event("late", "part"));
    assert!(buffer.is_empty());
}

#[tokio::test]
async fn the_reader_skips_to_the_event_asked_for() {
    let response = Response::new(axum::body::Body::from(
        "event: first\ndata: 1\n\nevent: second\ndata: 2\n\nevent: second\ndata: 3\n\n",
    ));
    let mut reader = SseReader::new(response);
    assert_eq!(reader.next("second").await, "2");
    assert_eq!(
        reader
            .next_where("the third event", |_, data| data == "3")
            .await,
        "3"
    );
}
