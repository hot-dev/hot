//! WebSocket client module for ::hot::ws functions
//!
//! Provides bidirectional WebSocket connections using tokio-tungstenite,
//! bridged to the synchronous Hot VM via mpsc channels.

use crate::lang::hot::iter::{HotIterator, IteratorBox};
use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use futures::{SinkExt, StreamExt};
use indexmap::IndexMap;
use serde_json::Value as JsonValue;
use std::any::Any;
use std::hash::Hasher;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http;

type WsReceiver = Arc<Mutex<mpsc::Receiver<Result<Val, String>>>>;

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

// ----------------------------------------------------------------------------
// WsReceiverHolder — type-erased holder for the raw mpsc receiver
// Used for timeout-based receive without downcasting through dyn HotIterator
// ----------------------------------------------------------------------------

#[derive(Debug)]
struct WsReceiverHolder {
    receiver: WsReceiver,
}

impl crate::val::ValBox for WsReceiverHolder {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
    fn clone_box(&self) -> Box<dyn crate::val::ValBox> {
        Box::new(WsReceiverHolder {
            receiver: Arc::clone(&self.receiver),
        })
    }
    fn equals(&self, other: &dyn crate::val::ValBox) -> bool {
        other
            .as_any()
            .downcast_ref::<WsReceiverHolder>()
            .is_some_and(|o| Arc::ptr_eq(&self.receiver, &o.receiver))
    }
    fn hash(&self, _state: &mut dyn Hasher) {}
    fn to_string(&self) -> String {
        "WsReceiverHolder".to_string()
    }
    fn compare(&self, _other: &dyn crate::val::ValBox) -> Option<std::cmp::Ordering> {
        None
    }
    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"$type": "WsReceiverHolder"}))
    }
    fn type_name(&self) -> &'static str {
        "WsReceiverHolder"
    }
}

// ----------------------------------------------------------------------------
// WsCommand — messages from the Hot VM to the background write task
// ----------------------------------------------------------------------------

enum WsCommand {
    Send(Message),
    Close,
}

// ----------------------------------------------------------------------------
// WsConnectionHandle — opaque handle stored in Val::Box for send/close
// ----------------------------------------------------------------------------

#[derive(Debug)]
pub struct WsConnectionHandle {
    id: String,
    write_tx: mpsc::Sender<WsCommand>,
    closed: Arc<AtomicBool>,
}

impl crate::val::ValBox for WsConnectionHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn clone_box(&self) -> Box<dyn crate::val::ValBox> {
        Box::new(WsConnectionHandle {
            id: self.id.clone(),
            write_tx: self.write_tx.clone(),
            closed: Arc::clone(&self.closed),
        })
    }

    fn equals(&self, other: &dyn crate::val::ValBox) -> bool {
        other
            .as_any()
            .downcast_ref::<WsConnectionHandle>()
            .is_some_and(|o| self.id == o.id)
    }

    fn hash(&self, _state: &mut dyn Hasher) {}

    fn to_string(&self) -> String {
        format!("WsConnection<{}>", self.id)
    }

    fn compare(&self, _other: &dyn crate::val::ValBox) -> Option<std::cmp::Ordering> {
        None
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "$type": "WsConnection",
            "id": self.id
        }))
    }

    fn type_name(&self) -> &'static str {
        "WsConnection"
    }
}

// Implement Debug for WsCommand so the mpsc channel compiles with diagnostics
impl std::fmt::Debug for WsCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsCommand::Send(_) => write!(f, "WsCommand::Send(...)"),
            WsCommand::Close => write!(f, "WsCommand::Close"),
        }
    }
}

// ----------------------------------------------------------------------------
// WsMessageIterator — yields incoming WebSocket messages to the Hot VM
// ----------------------------------------------------------------------------

#[derive(Debug)]
pub struct WsMessageIterator {
    receiver: WsReceiver,
    done: bool,
}

impl HotIterator for WsMessageIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if self.done {
            return Ok((Val::Null, true));
        }

        let chunk: Result<Option<Val>, String> = {
            let mut guard = self.receiver.lock().map_err(|e| e.to_string())?;
            guard.blocking_recv().transpose()
        };
        let chunk = chunk?;

        match chunk {
            Some(val) => Ok((val, false)),
            None => {
                self.done = true;
                Ok((Val::Null, true))
            }
        }
    }

    fn stream_data_type(&self) -> Option<&str> {
        Some("ws:message")
    }

    fn should_emit_stream_data(&self) -> bool {
        true
    }
}

// ----------------------------------------------------------------------------
// Public API functions
// ----------------------------------------------------------------------------

/// Connect to a WebSocket server.
///
/// # Arguments
/// * 1 arg:  url (Str)
/// * 2 args: url (Str), options (Map with optional `headers`, `subprotocols`)
///
/// # Returns
/// A Map `{id: Str, messages: Iterator, $ws: WsConnectionHandle}`
pub fn connect(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(err_val(
            "::hot::ws/connect: expected 1-2 args (url [, options])".to_string(),
        ));
    }

    let url = match &args[0] {
        Val::Str(s) => s.to_string(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::ws/connect: url must be a string".to_string(),
            ));
        }
    };

    let headers: IndexMap<Val, Val> = if args.len() > 1 {
        match &args[1] {
            Val::Map(m) => match m.get(&Val::from("headers")) {
                Some(Val::Map(h)) => (**h).clone(),
                _ => IndexMap::new(),
            },
            Val::Null => IndexMap::new(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::ws/connect: options must be a map".to_string(),
                ));
            }
        }
    } else {
        IndexMap::new()
    };

    tokio::runtime::Handle::current().block_on(async { ws_connect_async(&url, &headers).await })
}

async fn ws_connect_async(url: &str, headers: &IndexMap<Val, Val>) -> HotResult<Val> {
    let mut request = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return HotResult::Err(err_val(format!("::hot::ws/connect: invalid URL: {}", e))),
    };

    for (key, value) in headers.iter() {
        if let (Val::Str(k), Val::Str(v)) = (key, value) {
            let header_value: http::HeaderValue = match v.parse() {
                Ok(hv) => hv,
                Err(e) => {
                    return HotResult::Err(err_val(format!(
                        "::hot::ws/connect: invalid header value for '{}': {}",
                        k, e
                    )));
                }
            };
            if let Ok(header_name) = k.parse::<http::HeaderName>() {
                request.headers_mut().insert(header_name, header_value);
            }
        }
    }

    let (ws_stream, _response) = match tokio_tungstenite::connect_async(request).await {
        Ok(r) => r,
        Err(e) => {
            return HotResult::Err(err_val(format!(
                "::hot::ws/connect: connection failed: {}",
                e
            )));
        }
    };

    let (mut write, mut read) = ws_stream.split();

    let conn_id = uuid::Uuid::new_v4().to_string();
    let closed = Arc::new(AtomicBool::new(false));

    let (write_tx, mut write_rx) = mpsc::channel::<WsCommand>(32);
    let (msg_tx, msg_rx) = mpsc::channel::<Result<Val, String>>(32);

    let closed_for_read = Arc::clone(&closed);

    // Background write task: drains commands from the Hot VM
    tokio::spawn(async move {
        while let Some(cmd) = write_rx.recv().await {
            match cmd {
                WsCommand::Send(msg) => {
                    if let Err(e) = write.send(msg).await {
                        tracing::warn!("WebSocket write error: {}", e);
                        break;
                    }
                }
                WsCommand::Close => {
                    let _ = write.close().await;
                    break;
                }
            }
        }
    });

    // Background read task: forwards incoming messages to the Hot VM
    tokio::spawn(async move {
        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(msg) => {
                    let val = match &msg {
                        Message::Text(text) => {
                            let s = text.to_string();
                            match serde_json::from_str::<JsonValue>(&s) {
                                Ok(json) => serde_json::from_value(json).unwrap_or(Val::from(s)),
                                Err(_) => Val::from(s),
                            }
                        }
                        Message::Binary(data) => Val::Bytes(data.to_vec()),
                        Message::Close(_) => {
                            closed_for_read.store(true, Ordering::Relaxed);
                            break;
                        }
                        Message::Ping(_) | Message::Pong(_) => continue,
                        _ => continue,
                    };
                    if msg_tx.send(Ok(val)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = msg_tx.send(Err(format!("WebSocket error: {}", e))).await;
                    closed_for_read.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    let msg_rx_arc = Arc::new(Mutex::new(msg_rx));
    let iterator = WsMessageIterator {
        receiver: Arc::clone(&msg_rx_arc),
        done: false,
    };
    let iter_box = IteratorBox::new(Box::new(iterator));

    let handle = WsConnectionHandle {
        id: conn_id.clone(),
        write_tx,
        closed,
    };

    let rx_holder = WsReceiverHolder {
        receiver: msg_rx_arc,
    };

    let mut conn_map: IndexMap<Val, Val> = IndexMap::new();
    conn_map.insert(Val::from("id"), Val::from(conn_id));
    conn_map.insert(Val::from("messages"), Val::Box(Box::new(iter_box)));
    conn_map.insert(Val::from("$ws"), Val::Box(Box::new(handle)));
    conn_map.insert(Val::from("$ws_rx"), Val::Box(Box::new(rx_holder)));

    HotResult::Ok(Val::Map(Box::new(conn_map)))
}

/// Send a message over an open WebSocket connection.
///
/// # Arguments
/// * `conn` - WsConnection map (from `connect`)
/// * `data` - Data to send (Str → text frame, Bytes → binary frame, Map/Vec → JSON text frame)
///
/// # Returns
/// `true` on success
pub fn send(args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(err_val(
            "::hot::ws/send: expected 2 args (connection, data)".to_string(),
        ));
    }

    let handle = match extract_handle("::hot::ws/send", &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    if handle.closed.load(Ordering::Relaxed) {
        return HotResult::Err(err_val("::hot::ws/send: connection is closed".to_string()));
    }

    let message = val_to_message(&args[1]);

    match handle.write_tx.blocking_send(WsCommand::Send(message)) {
        Ok(()) => HotResult::Ok(Val::from(true)),
        Err(e) => HotResult::Err(err_val(format!("::hot::ws/send: send failed: {}", e))),
    }
}

/// Receive a single message from a WebSocket connection.
///
/// # Arguments
/// * `conn` - WsConnection map (from `connect`)
/// * `opts` (optional) - Map with options: `{timeout: Int}` (milliseconds)
///
/// Blocks until a message arrives, the connection closes (returns Null),
/// or the timeout expires (returns Null).
pub fn receive(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(err_val(
            "::hot::ws/receive: expected 1-2 args (connection, opts?)".to_string(),
        ));
    }

    let timeout_ms: Option<u64> = if args.len() == 2 {
        match &args[1] {
            Val::Map(m) => m.get(&Val::from("timeout")).and_then(|v| match v {
                Val::Int(i) => Some(*i as u64),
                Val::Dec(d) => Some(d.to_string().parse::<u64>().unwrap_or(0)),
                _ => None,
            }),
            _ => None,
        }
    } else {
        None
    };

    let iter_box = match extract_messages_iter("::hot::ws/receive", &args[0]) {
        Ok(ib) => ib,
        Err(e) => return HotResult::Err(e),
    };

    let mut guard = match iter_box.inner.lock() {
        Ok(g) => g,
        Err(e) => return HotResult::Err(err_val(format!("::hot::ws/receive: lock error: {}", e))),
    };

    // Use a wrapper that supports timeout via try_recv polling on the raw receiver
    if let Some(ms) = timeout_ms {
        // Access the raw receiver from the WsMessageIterator inside the IteratorBox.
        // The IteratorBox stores an Arc<Mutex<Box<dyn HotIterator>>>, and we can't
        // downcast through the trait object. Instead, we access the receiver stored
        // on the connection map directly.
        let receiver = match extract_raw_receiver("::hot::ws/receive", &args[0]) {
            Ok(r) => r,
            Err(e) => return HotResult::Err(e),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        match receive_with_timeout_inner(&receiver, deadline) {
            Ok(Some(val)) => HotResult::Ok(val),
            Ok(None) => HotResult::Ok(Val::Null),
            Err(e) => HotResult::Err(err_val(format!("::hot::ws/receive: {}", e))),
        }
    } else {
        match guard.next() {
            Ok((val, done)) => {
                if done {
                    HotResult::Ok(Val::Null)
                } else {
                    HotResult::Ok(val)
                }
            }
            Err(e) => HotResult::Err(err_val(format!("::hot::ws/receive: {}", e))),
        }
    }
}

fn receive_with_timeout_inner(
    receiver: &WsReceiver,
    deadline: std::time::Instant,
) -> Result<Option<Val>, String> {
    let mut rx = receiver.lock().map_err(|e| e.to_string())?;
    loop {
        match rx.try_recv() {
            Ok(Ok(val)) => return Ok(Some(val)),
            Ok(Err(e)) => return Err(e),
            Err(mpsc::error::TryRecvError::Disconnected) => return Ok(None),
            Err(mpsc::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

/// Close a WebSocket connection.
pub fn close(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(err_val(
            "::hot::ws/close: expected 1 arg (connection)".to_string(),
        ));
    }

    let handle = match extract_handle("::hot::ws/close", &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };
    handle.closed.store(true, Ordering::Relaxed);

    match handle.write_tx.blocking_send(WsCommand::Close) {
        Ok(()) => HotResult::Ok(Val::from(true)),
        Err(_) => HotResult::Ok(Val::from(true)), // already closed
    }
}

/// Check whether a WebSocket connection is still open.
pub fn is_open(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(err_val(
            "::hot::ws/is-open: expected 1 arg (connection)".to_string(),
        ));
    }

    let handle = match extract_handle("::hot::ws/is-open", &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };
    HotResult::Ok(Val::from(!handle.closed.load(Ordering::Relaxed)))
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

fn extract_handle<'a>(fn_name: &str, conn: &'a Val) -> Result<&'a WsConnectionHandle, Val> {
    let conn_map = match conn {
        Val::Map(m) => m,
        _ => {
            return Err(err_val(format!(
                "{}: first arg must be a WsConnection",
                fn_name
            )));
        }
    };

    match conn_map.get(&Val::from("$ws")) {
        Some(Val::Box(b)) => match b.as_any().downcast_ref::<WsConnectionHandle>() {
            Some(h) => Ok(h),
            None => Err(err_val(format!("{}: invalid connection handle", fn_name))),
        },
        _ => Err(err_val(format!(
            "{}: invalid connection (missing $ws handle)",
            fn_name
        ))),
    }
}

fn extract_messages_iter<'a>(fn_name: &str, conn: &'a Val) -> Result<&'a IteratorBox, Val> {
    let conn_map = match conn {
        Val::Map(m) => m,
        _ => {
            return Err(err_val(format!(
                "{}: first arg must be a WsConnection",
                fn_name
            )));
        }
    };

    match conn_map.get(&Val::from("messages")) {
        Some(Val::Box(b)) => match b.as_any().downcast_ref::<IteratorBox>() {
            Some(ib) => Ok(ib),
            None => Err(err_val(format!("{}: invalid messages iterator", fn_name))),
        },
        _ => Err(err_val(format!(
            "{}: invalid connection (missing messages)",
            fn_name
        ))),
    }
}

fn extract_raw_receiver(fn_name: &str, conn: &Val) -> Result<WsReceiver, Val> {
    let conn_map = match conn {
        Val::Map(m) => m,
        _ => {
            return Err(err_val(format!(
                "{}: first arg must be a WsConnection",
                fn_name
            )));
        }
    };

    match conn_map.get(&Val::from("$ws_rx")) {
        Some(Val::Box(b)) => match b.as_any().downcast_ref::<WsReceiverHolder>() {
            Some(holder) => Ok(Arc::clone(&holder.receiver)),
            None => Err(err_val(format!("{}: invalid receiver holder", fn_name))),
        },
        _ => Err(err_val(format!(
            "{}: invalid connection (missing $ws_rx)",
            fn_name
        ))),
    }
}

fn val_to_message(val: &Val) -> Message {
    match val {
        Val::Str(s) => Message::Text(s.to_string().into()),
        Val::Bytes(b) => Message::Binary(b.clone().into()),
        Val::Map(_) | Val::Vec(_) => {
            let json_value: JsonValue = val.into();
            Message::Text(json_value.to_string().into())
        }
        Val::Null => Message::Text("null".into()),
        other => Message::Text(other.to_string().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_val_to_message_str() {
        let val = Val::from("hello");
        let msg = val_to_message(&val);
        assert!(matches!(msg, Message::Text(_)));
    }

    #[test]
    fn test_val_to_message_map() {
        let mut map = IndexMap::new();
        map.insert(Val::from("type"), Val::from("test"));
        let val = Val::Map(Box::new(map));
        let msg = val_to_message(&val);
        assert!(matches!(msg, Message::Text(_)));
    }

    #[test]
    fn test_val_to_message_bytes() {
        let val = Val::Bytes(vec![1, 2, 3]);
        let msg = val_to_message(&val);
        assert!(matches!(msg, Message::Binary(_)));
    }
}
