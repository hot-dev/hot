//! TCP client module for ::hot::tcp functions
//!
//! Provides raw TCP client connections for implementing binary protocols
//! (Postgres, Redis, SMTP, ...) in Hot. Connections are held natively and
//! exposed to the Hot VM as opaque handles, bridged from the synchronous
//! VM to tokio via Handle::block_on (same pattern as ::hot::http/::hot::ws).
//!
//! TLS upgrades for these connections live in ::hot::tls (tls.rs), which
//! swaps the Plain stream for a TLS stream in place — Postgres-style
//! STARTTLS negotiation happens on the already-open socket.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use indexmap::IndexMap;
use std::any::Any;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Default timeout for connect and read operations (ms)
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Maximum single-read size we will allocate for
const MAX_READ_SIZE: i64 = 64 * 1024 * 1024;

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

// ----------------------------------------------------------------------------
// Connection state
// ----------------------------------------------------------------------------

pub(crate) enum TcpStreamState {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Closed,
}

pub(crate) struct TcpConnInner {
    pub(crate) id: String,
    pub(crate) host: String,
    pub(crate) stream: tokio::sync::Mutex<TcpStreamState>,
    pub(crate) closed: AtomicBool,
    /// DER of the server's leaf certificate, recorded after a TLS upgrade
    /// (used by ::hot::tls/peer-cert-hash for channel binding).
    pub(crate) peer_cert: std::sync::Mutex<Option<Vec<u8>>>,
}

// ----------------------------------------------------------------------------
// TcpConnectionHandle — opaque handle stored in Val::Box
// ----------------------------------------------------------------------------

pub struct TcpConnectionHandle {
    pub(crate) inner: Arc<TcpConnInner>,
}

impl std::fmt::Debug for TcpConnectionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TcpConnection<{}>", self.inner.id)
    }
}

impl crate::val::ValBox for TcpConnectionHandle {
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
        Box::new(TcpConnectionHandle {
            inner: Arc::clone(&self.inner),
        })
    }

    fn equals(&self, other: &dyn crate::val::ValBox) -> bool {
        other
            .as_any()
            .downcast_ref::<TcpConnectionHandle>()
            .is_some_and(|o| Arc::ptr_eq(&self.inner, &o.inner))
    }

    fn hash(&self, _state: &mut dyn Hasher) {}

    fn to_string(&self) -> String {
        format!("TcpConnection<{}>", self.inner.id)
    }

    fn compare(&self, _other: &dyn crate::val::ValBox) -> Option<std::cmp::Ordering> {
        None
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "$type": "TcpConnection",
            "id": self.inner.id
        }))
    }

    fn type_name(&self) -> &'static str {
        "TcpConnection"
    }
}

// ----------------------------------------------------------------------------
// Option parsing helpers
// ----------------------------------------------------------------------------

fn opt_timeout(fn_name: &str, opts: Option<&Val>) -> Result<Option<Duration>, Val> {
    let ms = match opts {
        Some(Val::Map(m)) => match m.get(&Val::from("timeout")) {
            Some(Val::Int(i)) if *i >= 0 => *i as u64,
            Some(Val::Int(i)) => {
                return Err(err_val(format!(
                    "{}: timeout must be >= 0 (0 = no timeout), got {}",
                    fn_name, i
                )));
            }
            Some(_) => {
                return Err(err_val(format!(
                    "{}: timeout must be an Int (milliseconds)",
                    fn_name
                )));
            }
            None => DEFAULT_TIMEOUT_MS,
        },
        Some(Val::Null) | None => DEFAULT_TIMEOUT_MS,
        Some(_) => {
            return Err(err_val(format!("{}: options must be a map", fn_name)));
        }
    };
    Ok(if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(ms))
    })
}

async fn with_timeout<F, T>(fn_name: &str, timeout: Option<Duration>, fut: F) -> Result<T, Val>
where
    F: std::future::Future<Output = Result<T, Val>>,
{
    match timeout {
        Some(t) => match tokio::time::timeout(t, fut).await {
            Ok(result) => result,
            Err(_) => Err(err_val(format!(
                "{}: timed out after {} ms",
                fn_name,
                t.as_millis()
            ))),
        },
        None => fut.await,
    }
}

pub(crate) fn extract_handle<'a>(
    fn_name: &str,
    conn: &'a Val,
) -> Result<&'a TcpConnectionHandle, Val> {
    let conn_map = match conn {
        Val::Map(m) => m,
        _ => {
            return Err(err_val(format!(
                "{}: first arg must be a TcpConnection",
                fn_name
            )));
        }
    };

    match conn_map.get(&Val::from("$tcp")) {
        Some(Val::Box(b)) => match b.as_any().downcast_ref::<TcpConnectionHandle>() {
            Some(h) => Ok(h),
            None => Err(err_val(format!("{}: invalid connection handle", fn_name))),
        },
        _ => Err(err_val(format!(
            "{}: invalid connection (missing $tcp handle)",
            fn_name
        ))),
    }
}

pub(crate) fn build_conn_map(inner: &Arc<TcpConnInner>, port: i64, tls: bool) -> Val {
    let handle = TcpConnectionHandle {
        inner: Arc::clone(inner),
    };
    let mut conn_map: IndexMap<Val, Val> = IndexMap::new();
    conn_map.insert(Val::from("id"), Val::from(inner.id.clone()));
    conn_map.insert(Val::from("host"), Val::from(inner.host.clone()));
    conn_map.insert(Val::from("port"), Val::Int(port));
    conn_map.insert(Val::from("tls"), Val::from(tls));
    conn_map.insert(Val::from("$tcp"), Val::Box(Box::new(handle)));
    Val::Map(Box::new(conn_map))
}

// ----------------------------------------------------------------------------
// Public API functions
// ----------------------------------------------------------------------------

/// Open a TCP connection.
///
/// # Arguments
/// * 2 args: host (Str), port (Int)
/// * 3 args: host (Str), port (Int), options (Map: `timeout` ms, `nodelay` Bool)
///
/// # Returns
/// A Map `{id: Str, host: Str, port: Int, tls: Bool, $tcp: TcpConnectionHandle}`
pub fn connect(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tcp/connect";

    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(err_val(format!(
            "{}: expected 2-3 args (host, port [, options])",
            FN
        )));
    }

    let host = match &args[0] {
        Val::Str(s) => s.to_string(),
        _ => return HotResult::Err(err_val(format!("{}: host must be a string", FN))),
    };

    let port = match &args[1] {
        Val::Int(p) if *p > 0 && *p <= 65535 => *p,
        Val::Int(p) => {
            return HotResult::Err(err_val(format!("{}: invalid port {}", FN, p)));
        }
        _ => return HotResult::Err(err_val(format!("{}: port must be an Int", FN))),
    };

    let timeout = match opt_timeout(FN, args.get(2)) {
        Ok(t) => t,
        Err(e) => return HotResult::Err(e),
    };

    let nodelay = match args.get(2) {
        Some(Val::Map(m)) => match m.get(&Val::from("nodelay")) {
            Some(Val::Bool(b)) => *b,
            _ => true,
        },
        _ => true,
    };

    let result = tokio::runtime::Handle::current().block_on(async {
        let stream = with_timeout(FN, timeout, async {
            TcpStream::connect((host.as_str(), port as u16))
                .await
                .map_err(|e| {
                    err_val(format!(
                        "{}: connection to {}:{} failed: {}",
                        FN, host, port, e
                    ))
                })
        })
        .await?;

        if let Err(e) = stream.set_nodelay(nodelay) {
            tracing::warn!("{}: set_nodelay failed: {}", FN, e);
        }

        Ok::<TcpStream, Val>(stream)
    });

    let stream = match result {
        Ok(s) => s,
        Err(e) => return HotResult::Err(e),
    };

    let inner = Arc::new(TcpConnInner {
        id: uuid::Uuid::new_v4().to_string(),
        host,
        stream: tokio::sync::Mutex::new(TcpStreamState::Plain(stream)),
        closed: AtomicBool::new(false),
        peer_cert: std::sync::Mutex::new(None),
    });

    HotResult::Ok(build_conn_map(&inner, port, false))
}

/// Read up to `max` bytes from the connection.
///
/// Blocks until at least one byte is available, the peer closes the
/// connection (returns Null), or the timeout expires (returns an error).
///
/// # Arguments
/// * 2 args: connection, max (Int)
/// * 3 args: connection, max (Int), options (Map: `timeout` ms, 0 = no timeout)
pub fn read(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tcp/read";
    read_impl(FN, args, false)
}

/// Read exactly `n` bytes from the connection.
///
/// Blocks until `n` bytes have been received. Returns an error if the
/// peer closes the connection first or the timeout expires.
pub fn read_exact(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tcp/read-exact";
    read_impl(FN, args, true)
}

fn read_impl(fn_name: &str, args: &[Val], exact: bool) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(err_val(format!(
            "{}: expected 2-3 args (connection, size [, options])",
            fn_name
        )));
    }

    let handle = match extract_handle(fn_name, &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    if handle.inner.closed.load(Ordering::Relaxed) {
        return HotResult::Err(err_val(format!("{}: connection is closed", fn_name)));
    }

    let size = match &args[1] {
        Val::Int(n) if *n > 0 && *n <= MAX_READ_SIZE => *n as usize,
        Val::Int(n) => {
            return HotResult::Err(err_val(format!(
                "{}: size must be between 1 and {}, got {}",
                fn_name, MAX_READ_SIZE, n
            )));
        }
        _ => {
            return HotResult::Err(err_val(format!("{}: size must be an Int", fn_name)));
        }
    };

    let timeout = match opt_timeout(fn_name, args.get(2)) {
        Ok(t) => t,
        Err(e) => return HotResult::Err(e),
    };

    let inner = Arc::clone(&handle.inner);
    let result = tokio::runtime::Handle::current().block_on(async {
        with_timeout(fn_name, timeout, async {
            let mut guard = inner.stream.lock().await;
            let mut buf = vec![0u8; size];

            if exact {
                let read_result = match &mut *guard {
                    TcpStreamState::Plain(s) => s.read_exact(&mut buf).await,
                    TcpStreamState::Tls(s) => s.read_exact(&mut buf).await,
                    TcpStreamState::Closed => {
                        return Err(err_val(format!("{}: connection is closed", fn_name)));
                    }
                };
                match read_result {
                    Ok(_) => Ok(Val::Bytes(buf)),
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        Err(err_val(format!(
                            "{}: connection closed before {} bytes arrived",
                            fn_name, size
                        )))
                    }
                    Err(e) => Err(err_val(format!("{}: read failed: {}", fn_name, e))),
                }
            } else {
                let read_result = match &mut *guard {
                    TcpStreamState::Plain(s) => s.read(&mut buf).await,
                    TcpStreamState::Tls(s) => s.read(&mut buf).await,
                    TcpStreamState::Closed => {
                        return Err(err_val(format!("{}: connection is closed", fn_name)));
                    }
                };
                match read_result {
                    Ok(0) => Ok(Val::Null), // clean EOF
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(Val::Bytes(buf))
                    }
                    Err(e) => Err(err_val(format!("{}: read failed: {}", fn_name, e))),
                }
            }
        })
        .await
    });

    match result {
        Ok(v) => HotResult::Ok(v),
        Err(e) => HotResult::Err(e),
    }
}

/// Write data to the connection. Accepts Bytes or Str (sent as UTF-8).
/// Returns the number of bytes written.
pub fn write(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tcp/write";

    if args.len() != 2 {
        return HotResult::Err(err_val(format!(
            "{}: expected 2 args (connection, data)",
            FN
        )));
    }

    let handle = match extract_handle(FN, &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    if handle.inner.closed.load(Ordering::Relaxed) {
        return HotResult::Err(err_val(format!("{}: connection is closed", FN)));
    }

    let data: Vec<u8> = match &args[1] {
        Val::Bytes(b) => b.clone(),
        Val::Str(s) => s.as_bytes().to_vec(),
        _ => {
            return HotResult::Err(err_val(format!("{}: data must be Bytes or Str", FN)));
        }
    };

    let inner = Arc::clone(&handle.inner);
    let len = data.len();
    let result = tokio::runtime::Handle::current().block_on(async {
        let mut guard = inner.stream.lock().await;
        let write_result = match &mut *guard {
            TcpStreamState::Plain(s) => s.write_all(&data).await.and(s.flush().await),
            TcpStreamState::Tls(s) => s.write_all(&data).await.and(s.flush().await),
            TcpStreamState::Closed => {
                return Err(err_val(format!("{}: connection is closed", FN)));
            }
        };
        write_result.map_err(|e| err_val(format!("{}: write failed: {}", FN, e)))
    });

    match result {
        Ok(()) => HotResult::Ok(Val::Int(len as i64)),
        Err(e) => HotResult::Err(e),
    }
}

/// Close the connection. Safe to call more than once.
pub fn close(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tcp/close";

    if args.len() != 1 {
        return HotResult::Err(err_val(format!("{}: expected 1 arg (connection)", FN)));
    }

    let handle = match extract_handle(FN, &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    handle.inner.closed.store(true, Ordering::Relaxed);

    let inner = Arc::clone(&handle.inner);
    tokio::runtime::Handle::current().block_on(async {
        let mut guard = inner.stream.lock().await;
        match &mut *guard {
            TcpStreamState::Plain(s) => {
                let _ = s.shutdown().await;
            }
            TcpStreamState::Tls(s) => {
                let _ = s.shutdown().await;
            }
            TcpStreamState::Closed => {}
        }
        *guard = TcpStreamState::Closed;
    });

    HotResult::Ok(Val::from(true))
}

/// Check whether the connection is still open (i.e. close has not been called).
pub fn is_open(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tcp/is-open";

    if args.len() != 1 {
        return HotResult::Err(err_val(format!("{}: expected 1 arg (connection)", FN)));
    }

    let handle = match extract_handle(FN, &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    HotResult::Ok(Val::from(!handle.inner.closed.load(Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn unwrap_ok(result: HotResult<Val>) -> Val {
        match result {
            HotResult::Ok(v) => v,
            HotResult::Err(e) => panic!("Expected Ok, got Err: {:?}", e),
        }
    }

    fn expect_err(result: HotResult<Val>) -> Val {
        match result {
            HotResult::Err(e) => e,
            HotResult::Ok(v) => panic!("Expected Err, got Ok: {:?}", v),
        }
    }

    /// Start an echo server; returns its port.
    async fn start_echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if socket.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_connect_write_read_roundtrip() {
        let port = start_echo_server().await;

        tokio::task::spawn_blocking(move || {
            let conn = unwrap_ok(connect(&[Val::from("127.0.0.1"), Val::Int(port as i64)]));

            // Connection map shape
            if let Val::Map(m) = &conn {
                assert_eq!(m.get(&Val::from("tls")), Some(&Val::Bool(false)));
                assert_eq!(m.get(&Val::from("host")), Some(&Val::from("127.0.0.1")));
            } else {
                panic!("Expected connection map");
            }

            let written = unwrap_ok(write(&[conn.clone(), Val::Bytes(vec![1, 2, 3, 4, 5])]));
            assert_eq!(written, Val::Int(5));

            let echoed = unwrap_ok(read_exact(&[conn.clone(), Val::Int(5)]));
            assert_eq!(echoed, Val::Bytes(vec![1, 2, 3, 4, 5]));

            // Str data is written as UTF-8
            unwrap_ok(write(&[conn.clone(), Val::from("hey")]));
            let echoed = unwrap_ok(read(&[conn.clone(), Val::Int(1024)]));
            assert_eq!(echoed, Val::Bytes(b"hey".to_vec()));

            unwrap_ok(close(std::slice::from_ref(&conn)));
            let open = unwrap_ok(is_open(std::slice::from_ref(&conn)));
            assert_eq!(open, Val::Bool(false));

            // Operations after close error out
            expect_err(write(&[conn.clone(), Val::from("nope")]));
            expect_err(read(&[conn, Val::Int(1)]));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_read_eof_returns_null() {
        // Server that writes 2 bytes then closes
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(&[0xAB, 0xCD]).await;
                // socket drops -> FIN
            }
        });

        tokio::task::spawn_blocking(move || {
            let conn = unwrap_ok(connect(&[Val::from("127.0.0.1"), Val::Int(port as i64)]));
            let data = unwrap_ok(read_exact(&[conn.clone(), Val::Int(2)]));
            assert_eq!(data, Val::Bytes(vec![0xAB, 0xCD]));
            // Peer closed: read returns Null
            let eof = unwrap_ok(read(&[conn.clone(), Val::Int(16)]));
            assert_eq!(eof, Val::Null);
            // read-exact past EOF errors
            expect_err(read_exact(&[conn, Val::Int(4)]));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_read_timeout() {
        // Server that accepts but never writes
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((_socket, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        tokio::task::spawn_blocking(move || {
            let conn = unwrap_ok(connect(&[Val::from("127.0.0.1"), Val::Int(port as i64)]));
            let mut opts: IndexMap<Val, Val> = IndexMap::new();
            opts.insert(Val::from("timeout"), Val::Int(200));
            let err = expect_err(read(&[conn, Val::Int(16), Val::Map(Box::new(opts))]));
            assert!(err.to_string().contains("timed out"), "got: {:?}", err);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_connect_refused() {
        tokio::task::spawn_blocking(move || {
            // Bind-then-drop to find a port with nothing listening
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let mut opts: IndexMap<Val, Val> = IndexMap::new();
            opts.insert(Val::from("timeout"), Val::Int(2000));
            let err = expect_err(connect(&[
                Val::from("127.0.0.1"),
                Val::Int(port as i64),
                Val::Map(Box::new(opts)),
            ]));
            assert!(
                err.to_string().contains("failed"),
                "expected connection failure, got: {:?}",
                err
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_arg_validation() {
        tokio::task::spawn_blocking(move || {
            expect_err(connect(&[Val::from("localhost")]));
            expect_err(connect(&[Val::from("localhost"), Val::Int(0)]));
            expect_err(connect(&[Val::from("localhost"), Val::Int(99999)]));
            expect_err(connect(&[Val::Int(80), Val::Int(80)]));
            expect_err(read(&[Val::from("not-a-conn"), Val::Int(1)]));
            expect_err(close(&[Val::Null]));
        })
        .await
        .unwrap();
    }
}
