use crate::lang::hot::r#type::{HotResult, untype_recursive};
use crate::val::Val;
use crate::{validate_args, validate_args_range};
use indexmap::IndexMap;
use serde_json::Value as JsonValue;

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

/// Make an HTTP request.
///
/// Accepts either:
/// - 1 arg: a Map/HttpRequest with `method`, `url`, and optionally `headers`, `body`
/// - 4 args: positional (method, url, headers, body)
pub fn request(args: &[Val]) -> HotResult<Val> {
    let (method, url, headers, body);

    if args.len() == 1 {
        // Single arg: expect a Map (possibly typed as HttpRequest).
        // Strip type wrapping so {$type, $val: {…}} or {$type, field…}
        // both resolve to a plain map with the domain fields.
        let untyped = match untype_recursive(&args[0]) {
            HotResult::Ok(v) => v,
            _ => args[0].clone(),
        };
        let map = match &untyped {
            Val::Map(m) => m,
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: expected an HttpRequest or Map".to_string(),
                ));
            }
        };

        method = match map.get(&Val::from("method")) {
            Some(Val::Str(s)) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: method is required and must be a string".to_string(),
                ));
            }
        };

        url = match map.get(&Val::from("url")) {
            Some(Val::Str(s)) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: url is required and must be a string".to_string(),
                ));
            }
        };

        headers = match map.get(&Val::from("headers")) {
            Some(Val::Map(m)) => m.clone(),
            Some(Val::Null) | None => Box::new(IndexMap::new()),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: headers must be a map".to_string(),
                ));
            }
        };

        body = match map.get(&Val::from("body")) {
            Some(val) if !matches!(val, Val::Null) => val.clone(),
            _ => Val::from(""),
        };
    } else if args.len() == 4 {
        // Positional: (method, url, headers, body)
        method = match &args[0] {
            Val::Str(s) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: method must be a string".to_string(),
                ));
            }
        };

        url = match &args[1] {
            Val::Str(s) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: url must be a string".to_string(),
                ));
            }
        };

        headers = match &args[2] {
            Val::Map(m) => m.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request: headers must be a map".to_string(),
                ));
            }
        };

        body = args[3].clone();
    } else {
        return HotResult::Err(err_val(
            "::hot::http/request: expected 1 arg (HttpRequest/Map) or 4 args (method, url, headers, body)".to_string(),
        ));
    }

    // Bridge sync→async. Since VM execution runs in spawn_blocking context,
    // we use Handle::block_on directly (block_in_place panics from spawn_blocking).
    tokio::runtime::Handle::current()
        .block_on(async { make_http_request(&method, &url, &headers, &body).await })
}

/// Build the User-Agent string, appending hot version to any user-provided value
fn build_user_agent(headers: &IndexMap<Val, Val>) -> String {
    let hot_ua = format!("hot/{}", crate::build_info::VERSION);

    // Check if user provided a User-Agent header
    let user_ua = headers
        .iter()
        .find(|(k, _)| matches!(k, Val::Str(s) if s.eq_ignore_ascii_case("user-agent")))
        .and_then(|(_, v)| {
            if let Val::Str(s) = v {
                Some((**s).to_owned())
            } else {
                None
            }
        });

    match user_ua {
        Some(ua) => format!("{} {}", ua, hot_ua),
        None => hot_ua,
    }
}

async fn make_http_request(
    method: &str,
    url: &str,
    headers: &IndexMap<Val, Val>,
    body: &Val,
) -> HotResult<Val> {
    let user_agent = build_user_agent(headers);
    let client = reqwest::Client::builder()
        .user_agent(&user_agent)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(120)) // 2 minute timeout for regular requests
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut request_builder = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => {
            return HotResult::Err(err_val(format!(
                "::hot::http/request: unsupported method: {}",
                method
            )));
        }
    };

    // Add headers (skip User-Agent since it's set on the client)
    for (key, value) in headers.iter() {
        if let (Val::Str(k), Val::Str(v)) = (key, value)
            && !k.eq_ignore_ascii_case("user-agent")
        {
            request_builder = request_builder.header(&**k, &**v);
        }
    }

    // Add body if not empty
    match body {
        Val::Str(s) if !s.is_empty() => {
            request_builder = request_builder.body(s.to_string());
        }
        Val::Map(_) | Val::Vec(_) => {
            // Serialize Val to JsonValue then to string
            let json_value: JsonValue = body.into();
            let json_str = json_value.to_string();
            request_builder = request_builder
                .header("Content-Type", "application/json")
                .body(json_str);
        }
        _ => {}
    }

    // Send the request
    let response = match request_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return HotResult::Err(err_val(format!(
                "::hot::http/request: request failed: {}",
                e
            )));
        }
    };

    // Build response object
    let status = response.status().as_u16() as i64;

    // Get headers
    let mut response_headers: IndexMap<Val, Val> = IndexMap::new();
    for (key, value) in response.headers().iter() {
        if let Ok(v) = value.to_str() {
            response_headers.insert(Val::from(key.to_string()), Val::from(v.to_string()));
        }
    }

    // Get body
    let body_text = match response.text().await {
        Ok(text) => text,
        Err(e) => {
            return HotResult::Err(err_val(format!(
                "::hot::http/request: failed to read body: {}",
                e
            )));
        }
    };

    // Try to parse as JSON, then convert to Val
    let body_val: Val = match serde_json::from_str::<JsonValue>(&body_text) {
        Ok(json) => serde_json::from_value(json).unwrap_or(Val::from(body_text.clone())),
        Err(_) => Val::from(body_text),
    };

    // Build response map
    let mut response_map: IndexMap<Val, Val> = IndexMap::new();
    response_map.insert(Val::from("status"), Val::Int(status));
    response_map.insert(Val::from("headers"), Val::Map(Box::new(response_headers)));
    response_map.insert(Val::from("body"), body_val);

    HotResult::Ok(Val::Map(Box::new(response_map)))
}

/// Make an HTTP GET request
pub fn get(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::http/get", args, 1);

    let url = match &args[0] {
        Val::Str(s) => s.clone(),
        _ => return HotResult::Err(err_val("::hot::http/get: url must be a string".to_string())),
    };

    request(&[
        Val::from("GET"),
        Val::from(url),
        Val::map_empty(),
        Val::from(""),
    ])
}

/// Make an HTTP POST request
pub fn post(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::http/post", args, 1, 2);

    let url = match &args[0] {
        Val::Str(s) => s.clone(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::http/post: url must be a string".to_string(),
            ));
        }
    };

    let body = if args.len() > 1 {
        args[1].clone()
    } else {
        Val::from("")
    };

    request(&[Val::from("POST"), Val::from(url), Val::map_empty(), body])
}

/// Make an HTTP PUT request
pub fn put(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::http/put", args, 1, 2);

    let url = match &args[0] {
        Val::Str(s) => s.clone(),
        _ => return HotResult::Err(err_val("::hot::http/put: url must be a string".to_string())),
    };

    let body = if args.len() > 1 {
        args[1].clone()
    } else {
        Val::from("")
    };

    request(&[Val::from("PUT"), Val::from(url), Val::map_empty(), body])
}

/// Make an HTTP DELETE request
pub fn delete(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::http/delete", args, 1);

    let url = match &args[0] {
        Val::Str(s) => s.clone(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::http/delete: url must be a string".to_string(),
            ));
        }
    };

    request(&[
        Val::from("DELETE"),
        Val::from(url),
        Val::map_empty(),
        Val::from(""),
    ])
}

// ============================================================================
// Streaming HTTP Support
// ============================================================================

use bytes::Bytes;
use futures::StreamExt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::lang::hot::iter::{HotIterator, IteratorBox};

/// HTTP Stream Iterator - yields chunks from a streaming HTTP response
#[derive(Debug)]
pub struct HttpStreamIterator {
    /// Receiver for chunks from the background streaming task
    receiver: Arc<Mutex<mpsc::Receiver<Result<Bytes, String>>>>,
    /// Response status code
    pub status: i64,
    /// Response headers
    pub headers: IndexMap<Val, Val>,
    /// Stream format hint (e.g., "sse", "ndjson", "raw")
    pub format: String,
    /// Buffer for partial SSE events
    sse_buffer: String,
    /// Complete SSE events parsed from the last chunk but not yet yielded
    pending_sse_events: VecDeque<Val>,
    /// Whether the stream is exhausted
    done: bool,
}

impl HotIterator for HttpStreamIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if let Some(event) = self.pending_sse_events.pop_front() {
            return Ok((event, false));
        }

        if self.done {
            return Ok((Val::Null, true));
        }

        // Block to receive next chunk from the streaming response.
        // We use blocking_recv() which is designed for calling from non-async contexts
        // (including from within spawn_blocking tasks). This avoids the need for
        // block_in_place which panics when called from spawn_blocking.
        let chunk: Result<Option<Bytes>, String> = {
            let mut guard = self
                .receiver
                .lock()
                .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
            guard.blocking_recv().transpose()
        };
        let chunk = chunk?;

        match chunk {
            Some(bytes) => {
                // Process based on format
                let value = match self.format.as_str() {
                    "sse" => self.parse_sse_chunk(&bytes),
                    "ndjson" => self.parse_ndjson_chunk(&bytes),
                    _ => {
                        // Raw bytes as string
                        match String::from_utf8(bytes.to_vec()) {
                            Ok(s) => Val::from(s),
                            Err(_) => Val::Bytes(bytes.to_vec()),
                        }
                    }
                };
                Ok((value, false))
            }
            None => {
                self.done = true;
                // Flush any remaining SSE buffer
                if !self.sse_buffer.is_empty() && self.format == "sse" {
                    let remaining = std::mem::take(&mut self.sse_buffer);
                    if let Some(event) = self.parse_sse_event(&remaining) {
                        return Ok((event, false));
                    }
                }
                Ok((Val::Null, true))
            }
        }
    }

    fn stream_data_type(&self) -> Option<&str> {
        Some(match self.format.as_str() {
            "sse" => "http:sse:event",
            "ndjson" => "http:ndjson:line",
            _ => "http:chunk",
        })
    }

    fn should_emit_stream_data(&self) -> bool {
        true // Always emit stream:data for HTTP streams
    }
}

impl HttpStreamIterator {
    /// Parse SSE (Server-Sent Events) chunk - buffers data and returns complete events
    fn parse_sse_chunk(&mut self, bytes: &Bytes) -> Val {
        let chunk_str = String::from_utf8_lossy(bytes);
        self.sse_buffer.push_str(&chunk_str);

        // Look for complete events (double newline)
        let mut events = VecDeque::new();
        while let Some(pos) = self.sse_buffer.find("\n\n") {
            let event_str = self.sse_buffer[..pos].to_string();
            self.sse_buffer = self.sse_buffer[pos + 2..].to_string();

            if let Some(event) = self.parse_sse_event(&event_str) {
                events.push_back(event);
            }
        }

        // Return one event per iterator item. HTTP chunks often coalesce
        // several SSE frames; yielding a Vec here makes consumers that expect
        // one event per next() accidentally skip all of them.
        // The null filter in iter.rs will prevent Null from being published/stored
        if events.is_empty() {
            Val::Null
        } else {
            let first = events.pop_front().unwrap();
            self.pending_sse_events.extend(events);
            first
        }
    }

    /// Parse a single SSE event string into a Val
    fn parse_sse_event(&self, event_str: &str) -> Option<Val> {
        let mut event_type = String::new();
        let mut data_lines = Vec::new();
        let mut id = None;

        for line in event_str.lines() {
            if let Some(stripped) = line.strip_prefix("event:") {
                event_type = stripped.trim().to_string();
            } else if let Some(stripped) = line.strip_prefix("data:") {
                data_lines.push(stripped.trim().to_string());
            } else if let Some(stripped) = line.strip_prefix("id:") {
                id = Some(stripped.trim().to_string());
            }
        }

        if data_lines.is_empty() && event_type.is_empty() {
            return None;
        }

        let data_str = data_lines.join("\n");

        // Try to parse data as JSON
        let data_val: Val = match serde_json::from_str::<JsonValue>(&data_str) {
            Ok(json) => serde_json::from_value(json).unwrap_or(Val::from(data_str.clone())),
            Err(_) => Val::from(data_str),
        };

        let mut event_map: IndexMap<Val, Val> = IndexMap::new();
        if !event_type.is_empty() {
            event_map.insert(Val::from("event"), Val::from(event_type));
        }
        event_map.insert(Val::from("data"), data_val);
        if let Some(id_str) = id {
            event_map.insert(Val::from("id"), Val::from(id_str));
        }

        Some(Val::Map(Box::new(event_map)))
    }

    /// Parse NDJSON (newline-delimited JSON) chunk
    fn parse_ndjson_chunk(&mut self, bytes: &Bytes) -> Val {
        let chunk_str = String::from_utf8_lossy(bytes);
        self.sse_buffer.push_str(&chunk_str);

        // Look for complete lines
        let mut objects = Vec::new();
        while let Some(pos) = self.sse_buffer.find('\n') {
            let line = self.sse_buffer[..pos].trim().to_string();
            self.sse_buffer = self.sse_buffer[pos + 1..].to_string();

            if !line.is_empty() {
                match serde_json::from_str::<JsonValue>(&line) {
                    Ok(json) => {
                        let val: Val =
                            serde_json::from_value(json).unwrap_or(Val::from(line.clone()));
                        objects.push(val);
                    }
                    Err(_) => {
                        objects.push(Val::from(line));
                    }
                }
            }
        }

        if objects.is_empty() {
            Val::Null
        } else if objects.len() == 1 {
            objects.pop().unwrap()
        } else {
            Val::Vec(objects)
        }
    }
}

/// Make a streaming HTTP request that returns an iterator
///
/// # Arguments
/// * `method` - HTTP method (GET, POST, etc.)
/// * `url` - Request URL
/// * `headers` - Request headers
/// * `body` - Request body
/// * `format` - Stream format: "sse", "ndjson", or "raw" (optional, default "raw")
///
/// # Returns
/// * A map with `{status: Int, headers: Map, body: Iterator}`
///
/// Accepts either:
/// - 1 arg: a Map/HttpRequest (format defaults to "raw")
/// - 2 args: a Map/HttpRequest + format string
/// - 4 args: positional (method, url, headers, body) — format defaults to "raw"
/// - 5 args: positional (method, url, headers, body, format)
pub fn request_stream(args: &[Val]) -> HotResult<Val> {
    let (method, url, headers, body, format);

    if args.len() <= 2 {
        // 1-2 args: Map/HttpRequest [, format]
        if args.is_empty() {
            return HotResult::Err(err_val(
                "::hot::http/request-stream: expected 1-2 args (HttpRequest/Map [, format]) or 4-5 args (method, url, headers, body [, format])".to_string(),
            ));
        }

        // Strip type wrapping (same as request)
        let untyped = match untype_recursive(&args[0]) {
            HotResult::Ok(v) => v,
            _ => args[0].clone(),
        };
        let map = match &untyped {
            Val::Map(m) => m,
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: expected an HttpRequest or Map".to_string(),
                ));
            }
        };

        method = match map.get(&Val::from("method")) {
            Some(Val::Str(s)) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: method is required and must be a string"
                        .to_string(),
                ));
            }
        };

        url = match map.get(&Val::from("url")) {
            Some(Val::Str(s)) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: url is required and must be a string".to_string(),
                ));
            }
        };

        headers = match map.get(&Val::from("headers")) {
            Some(Val::Map(m)) => m.clone(),
            Some(Val::Null) | None => Box::new(IndexMap::new()),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: headers must be a map".to_string(),
                ));
            }
        };

        body = match map.get(&Val::from("body")) {
            Some(val) if !matches!(val, Val::Null) => val.clone(),
            _ => Val::from(""),
        };

        format = if args.len() > 1 {
            match &args[1] {
                Val::Str(s) => (**s).to_owned(),
                _ => "raw".to_string(),
            }
        } else {
            "raw".to_string()
        };
    } else if args.len() == 4 || args.len() == 5 {
        // 4-5 args: positional (method, url, headers, body [, format])
        method = match &args[0] {
            Val::Str(s) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: method must be a string".to_string(),
                ));
            }
        };

        url = match &args[1] {
            Val::Str(s) => s.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: url must be a string".to_string(),
                ));
            }
        };

        headers = match &args[2] {
            Val::Map(m) => m.clone(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::http/request-stream: headers must be a map".to_string(),
                ));
            }
        };

        body = args[3].clone();

        format = if args.len() > 4 {
            match &args[4] {
                Val::Str(s) => (**s).to_owned(),
                _ => "raw".to_string(),
            }
        } else {
            "raw".to_string()
        };
    } else {
        return HotResult::Err(err_val(
            "::hot::http/request-stream: expected 1-2 args (HttpRequest/Map [, format]) or 4-5 args (method, url, headers, body [, format])".to_string(),
        ));
    }

    // Bridge sync→async. Since VM execution runs in spawn_blocking context,
    // we use Handle::block_on directly (block_in_place panics from spawn_blocking).
    tokio::runtime::Handle::current()
        .block_on(async { make_streaming_request(&method, &url, &headers, &body, &format).await })
}

async fn make_streaming_request(
    method: &str,
    url: &str,
    headers: &IndexMap<Val, Val>,
    body: &Val,
    format: &str,
) -> HotResult<Val> {
    let user_agent = build_user_agent(headers);
    let client = reqwest::Client::builder()
        .user_agent(&user_agent)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300)) // 5 minute timeout for large requests
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut request_builder = match method.to_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => {
            return HotResult::Err(err_val(format!(
                "::hot::http/request-stream: unsupported method: {}",
                method
            )));
        }
    };

    // Add headers (skip User-Agent since it's set on the client)
    for (key, value) in headers.iter() {
        if let (Val::Str(k), Val::Str(v)) = (key, value)
            && !k.eq_ignore_ascii_case("user-agent")
        {
            request_builder = request_builder.header(&**k, &**v);
        }
    }

    // Add body if not empty
    match body {
        Val::Str(s) if !s.is_empty() => {
            request_builder = request_builder.body(s.to_string());
        }
        Val::Map(_) | Val::Vec(_) => {
            let json_value: JsonValue = body.into();
            let json_str = json_value.to_string();
            request_builder = request_builder
                .header("Content-Type", "application/json")
                .body(json_str);
        }
        _ => {}
    }

    // Send the request
    let response = match request_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return HotResult::Err(err_val(format!(
                "::hot::http/request-stream: request failed: {}",
                e
            )));
        }
    };

    let status = response.status().as_u16() as i64;

    // Get headers
    let mut response_headers: IndexMap<Val, Val> = IndexMap::new();
    for (key, value) in response.headers().iter() {
        if let Ok(v) = value.to_str() {
            response_headers.insert(Val::from(key.to_string()), Val::from(v.to_string()));
        }
    }

    // Create a channel to stream chunks
    let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(32);

    // Spawn a task to stream the response body
    let mut byte_stream = response.bytes_stream();
    tokio::spawn(async move {
        while let Some(chunk_result) = byte_stream.next().await {
            let send_result: Result<(), mpsc::error::SendError<Result<Bytes, String>>> =
                match chunk_result {
                    Ok(bytes) => tx.send(Ok(bytes)).await,
                    Err(e) => tx.send(Err(e.to_string())).await,
                };
            if send_result.is_err() {
                // Receiver dropped, stop streaming
                break;
            }
        }
        // Channel closes when tx is dropped
    });

    // Create the iterator
    let iterator = HttpStreamIterator {
        receiver: Arc::new(Mutex::new(rx)),
        status,
        headers: response_headers.clone(),
        format: format.to_string(),
        sse_buffer: String::new(),
        pending_sse_events: VecDeque::new(),
        done: false,
    };

    let iter_box = IteratorBox::new(Box::new(iterator));

    // Build response map with iterator as body
    let mut response_map: IndexMap<Val, Val> = IndexMap::new();
    response_map.insert(Val::from("status"), Val::Int(status));
    response_map.insert(Val::from("headers"), Val::Map(Box::new(response_headers)));
    response_map.insert(Val::from("body"), Val::Box(Box::new(iter_box)));

    HotResult::Ok(Val::Map(Box::new(response_map)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::StatusCode,
        response::{IntoResponse, Sse, sse::Event},
    };
    use futures::stream;
    use std::convert::Infallible;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// Start a mock server and return its URL
    async fn start_mock_server(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        url
    }

    /// Helper to unwrap HotResult
    fn unwrap_result(result: HotResult<Val>) -> Val {
        match result {
            HotResult::Ok(v) => v,
            HotResult::Err(e) => panic!("Expected Ok, got Err: {:?}", e),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_get_json() {
        // Create a simple JSON endpoint
        let app = Router::new().route(
            "/json",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "message": "hello",
                    "count": 42
                }))
            }),
        );

        let base_url = start_mock_server(app).await;
        let url = format!("{}/json", base_url);

        // Make the request in spawn_blocking to match production context
        // (HTTP functions use Handle::block_on which requires non-async context)
        let response = tokio::task::spawn_blocking(move || {
            let result = super::get(&[Val::from(url)]);
            unwrap_result(result)
        })
        .await
        .unwrap();

        if let Val::Map(map) = response {
            // Check status
            let status = map.get(&Val::from("status")).unwrap();
            assert_eq!(status, &Val::Int(200));

            // Check body
            let body = map.get(&Val::from("body")).unwrap();
            if let Val::Map(body_map) = body {
                let message = body_map.get(&Val::from("message")).unwrap();
                assert_eq!(message, &Val::from("hello"));

                let count = body_map.get(&Val::from("count")).unwrap();
                assert_eq!(count, &Val::Int(42));
            } else {
                panic!("Expected body to be a map");
            }
        } else {
            panic!("Expected response to be a map");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_stream_sse() {
        // Create an SSE endpoint that emits 3 events
        async fn sse_handler() -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
            let stream = stream::iter(vec![
                Ok(Event::default()
                    .event("message")
                    .data(r#"{"text":"hello"}"#)),
                Ok(Event::default()
                    .event("message")
                    .data(r#"{"text":"world"}"#)),
                Ok(Event::default().event("done").data(r#"{"text":"!"}"#)),
            ]);
            Sse::new(stream)
        }

        let app = Router::new().route("/sse", axum::routing::get(sse_handler));
        let base_url = start_mock_server(app).await;
        let url = format!("{}/sse", base_url);

        // Run entire test in spawn_blocking to match production context
        // (HTTP functions use Handle::block_on and blocking_recv which require non-async context)
        let events = tokio::task::spawn_blocking(move || {
            // Make streaming request
            let result = request_stream(&[
                Val::from("GET"),
                Val::from(url),
                Val::map_empty(),
                Val::from(""),
                Val::from("sse"),
            ]);

            let response = unwrap_result(result);

            if let Val::Map(map) = response {
                // Check status
                let status = map.get(&Val::from("status")).unwrap();
                assert_eq!(status, &Val::Int(200));

                // Get the iterator
                let body = map.get(&Val::from("body")).unwrap();
                if let Val::Box(boxed) = body {
                    let iter_box = boxed.as_any().downcast_ref::<IteratorBox>().unwrap();

                    // Collect events
                    let mut events = Vec::new();
                    loop {
                        let mut guard = iter_box.inner.lock().unwrap();
                        let (value, done) = guard.next().unwrap();
                        if done {
                            break;
                        }
                        // Skip null values (partial chunks)
                        if !matches!(value, Val::Null) {
                            events.push(value);
                        }
                    }
                    events
                } else {
                    panic!("Expected body to be a Box (iterator)");
                }
            } else {
                panic!("Expected response to be a map");
            }
        })
        .await
        .unwrap();

        // We should have received 3 events
        assert_eq!(events.len(), 3, "Expected 3 SSE events, got {:?}", events);

        // Check first event
        if let Val::Map(event) = &events[0] {
            let event_type = event.get(&Val::from("event"));
            assert_eq!(event_type, Some(&Val::from("message")));

            let data = event.get(&Val::from("data")).unwrap();
            if let Val::Map(data_map) = data {
                let text = data_map.get(&Val::from("text")).unwrap();
                assert_eq!(text, &Val::from("hello"));
            }
        }
    }

    #[test]
    fn test_http_stream_sse_coalesced_events_yield_individually() {
        let (tx, rx) = mpsc::channel(1);
        tx.blocking_send(Ok(Bytes::from_static(
            b"event: message\ndata: {\"text\":\"hello\"}\n\nevent: message\ndata: {\"text\":\"world\"}\n\n",
        )))
        .unwrap();
        drop(tx);

        let mut iterator = HttpStreamIterator {
            receiver: Arc::new(Mutex::new(rx)),
            status: 200,
            headers: IndexMap::new(),
            format: "sse".to_string(),
            sse_buffer: String::new(),
            pending_sse_events: VecDeque::new(),
            done: false,
        };

        let (first, first_done) = iterator.next().unwrap();
        let (second, second_done) = iterator.next().unwrap();
        let (_, final_done) = iterator.next().unwrap();

        assert!(!first_done);
        assert!(!second_done);
        assert!(final_done);

        if let Val::Map(event) = first {
            assert_eq!(event.get(&Val::from("event")), Some(&Val::from("message")));
            let data = event.get(&Val::from("data")).unwrap();
            if let Val::Map(data_map) = data {
                assert_eq!(data_map.get(&Val::from("text")), Some(&Val::from("hello")));
            } else {
                panic!("Expected first event data to be a map");
            }
        } else {
            panic!("Expected first SSE item to be a map");
        }

        if let Val::Map(event) = second {
            let data = event.get(&Val::from("data")).unwrap();
            if let Val::Map(data_map) = data {
                assert_eq!(data_map.get(&Val::from("text")), Some(&Val::from("world")));
            } else {
                panic!("Expected second event data to be a map");
            }
        } else {
            panic!("Expected second SSE item to be a map");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_stream_ndjson() {
        // Create an NDJSON endpoint
        async fn ndjson_handler() -> impl IntoResponse {
            let body = r#"{"id":1,"name":"first"}
{"id":2,"name":"second"}
{"id":3,"name":"third"}
"#;
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
                body,
            )
        }

        let app = Router::new().route("/ndjson", axum::routing::get(ndjson_handler));
        let base_url = start_mock_server(app).await;
        let url = format!("{}/ndjson", base_url);

        // Run entire test in spawn_blocking to match production context
        let objects = tokio::task::spawn_blocking(move || {
            // Make streaming request
            let result = request_stream(&[
                Val::from("GET"),
                Val::from(url),
                Val::map_empty(),
                Val::from(""),
                Val::from("ndjson"),
            ]);

            let response = unwrap_result(result);

            if let Val::Map(map) = response {
                let body = map.get(&Val::from("body")).unwrap();
                if let Val::Box(boxed) = body {
                    let iter_box = boxed.as_any().downcast_ref::<IteratorBox>().unwrap();

                    // Collect all JSON objects
                    let mut objects = Vec::new();
                    loop {
                        let mut guard = iter_box.inner.lock().unwrap();
                        let (value, done) = guard.next().unwrap();
                        if done {
                            break;
                        }
                        // Handle both single values and vectors (multiple lines in one chunk)
                        match value {
                            Val::Null => continue,
                            Val::Vec(items) => objects.extend(items),
                            other => objects.push(other),
                        }
                    }
                    objects
                } else {
                    panic!("Expected body to be a Box (iterator)");
                }
            } else {
                panic!("Expected response to be a map");
            }
        })
        .await
        .unwrap();

        // We should have 3 JSON objects
        assert_eq!(
            objects.len(),
            3,
            "Expected 3 NDJSON objects, got {:?}",
            objects
        );

        // Check first object
        if let Val::Map(obj) = &objects[0] {
            let id = obj.get(&Val::from("id")).unwrap();
            assert_eq!(id, &Val::Int(1));
            let name = obj.get(&Val::from("name")).unwrap();
            assert_eq!(name, &Val::from("first"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_post_with_json_body() {
        use axum::extract::Json;

        // Echo endpoint that returns the posted body
        async fn echo_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
            axum::Json(body)
        }

        let app = Router::new().route("/echo", axum::routing::post(echo_handler));
        let base_url = start_mock_server(app).await;
        let url = format!("{}/echo", base_url);

        // Create request body
        let mut body_map: IndexMap<Val, Val> = IndexMap::new();
        body_map.insert(Val::from("name"), Val::from("test"));
        body_map.insert(Val::from("value"), Val::Int(123));

        // Make POST request in spawn_blocking to match production context
        let response = tokio::task::spawn_blocking(move || {
            let result = request(&[
                Val::from("POST"),
                Val::from(url),
                Val::map_empty(),
                Val::Map(Box::new(body_map)),
            ]);
            unwrap_result(result)
        })
        .await
        .unwrap();

        if let Val::Map(map) = response {
            let status = map.get(&Val::from("status")).unwrap();
            assert_eq!(status, &Val::Int(200));

            let body = map.get(&Val::from("body")).unwrap();
            if let Val::Map(response_body) = body {
                let name = response_body.get(&Val::from("name")).unwrap();
                assert_eq!(name, &Val::from("test"));
                let value = response_body.get(&Val::from("value")).unwrap();
                assert_eq!(value, &Val::Int(123));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_request_with_map_arg() {
        use axum::extract::Json;

        // Echo endpoint that returns the posted body
        async fn echo_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
            axum::Json(body)
        }

        let app = Router::new().route("/echo", axum::routing::post(echo_handler));
        let base_url = start_mock_server(app).await;
        let url = format!("{}/echo", base_url);

        // Build an HttpRequest-like map (with $type) as a single arg
        let mut req_map: IndexMap<Val, Val> = IndexMap::new();
        req_map.insert(Val::from("$type"), Val::from("HttpRequest"));
        req_map.insert(Val::from("method"), Val::from("POST"));
        req_map.insert(Val::from("url"), Val::from(url));

        let mut body_map: IndexMap<Val, Val> = IndexMap::new();
        body_map.insert(Val::from("greeting"), Val::from("hello"));
        req_map.insert(Val::from("body"), Val::Map(Box::new(body_map)));
        // headers omitted — should default to empty

        let response = tokio::task::spawn_blocking(move || {
            let result = request(&[Val::Map(Box::new(req_map))]);
            unwrap_result(result)
        })
        .await
        .unwrap();

        if let Val::Map(map) = response {
            let status = map.get(&Val::from("status")).unwrap();
            assert_eq!(status, &Val::Int(200));

            let body = map.get(&Val::from("body")).unwrap();
            if let Val::Map(response_body) = body {
                let greeting = response_body.get(&Val::from("greeting")).unwrap();
                assert_eq!(greeting, &Val::from("hello"));
            } else {
                panic!("Expected body to be a map");
            }
        } else {
            panic!("Expected response to be a map");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_request_with_plain_map() {
        use axum::extract::Json;

        // Echo endpoint
        async fn echo_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
            axum::Json(body)
        }

        let app = Router::new().route("/echo", axum::routing::post(echo_handler));
        let base_url = start_mock_server(app).await;
        let url = format!("{}/echo", base_url);

        // Build a plain map (no $type) as a single arg — structural match
        let mut req_map: IndexMap<Val, Val> = IndexMap::new();
        req_map.insert(Val::from("method"), Val::from("POST"));
        req_map.insert(Val::from("url"), Val::from(url));

        let mut headers_map: IndexMap<Val, Val> = IndexMap::new();
        headers_map.insert(Val::from("x-custom-header"), Val::from("test-value"));
        req_map.insert(Val::from("headers"), Val::Map(Box::new(headers_map)));

        let mut body_map: IndexMap<Val, Val> = IndexMap::new();
        body_map.insert(Val::from("data"), Val::from("test"));
        req_map.insert(Val::from("body"), Val::Map(Box::new(body_map)));

        let response = tokio::task::spawn_blocking(move || {
            let result = request(&[Val::Map(Box::new(req_map))]);
            unwrap_result(result)
        })
        .await
        .unwrap();

        if let Val::Map(map) = response {
            let status = map.get(&Val::from("status")).unwrap();
            assert_eq!(status, &Val::Int(200));

            let body = map.get(&Val::from("body")).unwrap();
            if let Val::Map(response_body) = body {
                let data = response_body.get(&Val::from("data")).unwrap();
                assert_eq!(data, &Val::from("test"));
            } else {
                panic!("Expected body to be a map");
            }
        } else {
            panic!("Expected response to be a map");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_request_with_typed_val_wrapping() {
        use axum::extract::Json;

        // Echo endpoint
        async fn echo_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
            axum::Json(body)
        }

        let app = Router::new().route("/echo", axum::routing::post(echo_handler));
        let base_url = start_mock_server(app).await;
        let url = format!("{}/echo", base_url);

        // Build an HttpRequest with $val wrapping — this is how typed struct values
        // actually arrive from the VM: {$type: "...", $val: {method: ..., url: ...}}
        let mut inner_map: IndexMap<Val, Val> = IndexMap::new();
        inner_map.insert(Val::from("method"), Val::from("POST"));
        inner_map.insert(Val::from("url"), Val::from(url));

        let mut body_map: IndexMap<Val, Val> = IndexMap::new();
        body_map.insert(Val::from("msg"), Val::from("wrapped"));
        inner_map.insert(Val::from("body"), Val::Map(Box::new(body_map)));

        let mut req_map: IndexMap<Val, Val> = IndexMap::new();
        req_map.insert(Val::from("$type"), Val::from("::hot::http/HttpRequest"));
        req_map.insert(Val::from("$val"), Val::Map(Box::new(inner_map)));

        let response = tokio::task::spawn_blocking(move || {
            let result = request(&[Val::Map(Box::new(req_map))]);
            unwrap_result(result)
        })
        .await
        .unwrap();

        if let Val::Map(map) = response {
            let status = map.get(&Val::from("status")).unwrap();
            assert_eq!(status, &Val::Int(200));

            let body = map.get(&Val::from("body")).unwrap();
            if let Val::Map(response_body) = body {
                let msg = response_body.get(&Val::from("msg")).unwrap();
                assert_eq!(msg, &Val::from("wrapped"));
            } else {
                panic!("Expected body to be a map");
            }
        } else {
            panic!("Expected response to be a map");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_request_bad_arg_count() {
        // 2 args should fail
        let result = request(&[Val::from("GET"), Val::from("http://example.com")]);
        match result {
            HotResult::Err(_) => {} // expected
            HotResult::Ok(_) => panic!("Expected error for 2 args"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_http_404_error() {
        let app = Router::new();
        let base_url = start_mock_server(app).await;
        let url = format!("{}/nonexistent", base_url);

        // Make request in spawn_blocking to match production context
        let response = tokio::task::spawn_blocking(move || {
            let result = super::get(&[Val::from(url)]);
            unwrap_result(result)
        })
        .await
        .unwrap();

        if let Val::Map(map) = response {
            let status = map.get(&Val::from("status")).unwrap();
            assert_eq!(status, &Val::Int(404));
        }
    }
}
