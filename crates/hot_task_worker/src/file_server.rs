//! Per-task HTTP file server for container file access.
//!
//! Spun up alongside each container task, this server exposes the Hot file
//! storage backend over a unix socket (and optional TCP fallback). The
//! `hotbox` binary inside the container connects to the socket to read/write
//! files in the user's environment.
//!
//! ## Endpoints
//!
//!   GET    /files/<path>          — read file bytes
//!   PUT    /files/<path>          — write file bytes (body = raw content)
//!   HEAD   /files/<path>          — file metadata as X-File-Meta JSON header
//!   GET    /files?prefix=<prefix> — list files as JSON array
//!   DELETE /files/<path>          — delete file
//!   GET    /health                — liveness check

use hot::db::DatabasePool;
use hot::file_storage::FileStorage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, oneshot};
use uuid::Uuid;

const GLOBAL_CONNECTION_LIMIT: usize = 256;
const PER_TASK_CONNECTION_LIMIT: usize = 32;
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_SIZE: usize = 16 * 1024 * 1024;
// Match the product's maximum file size. Streaming and transfer semaphores
// bound worker memory/concurrency without silently reducing the storage plan.
const HARD_MAX_READ_SIZE: usize = 50 * 1024 * 1024 * 1024;
const MAX_BUFFERED_READ_SIZE: usize = MAX_BODY_SIZE;
const HARD_MAX_LIST_FILES: usize = 1_000;
const MAX_LIST_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const GLOBAL_TRANSFER_LIMIT: usize = 4;
const PER_TASK_TRANSFER_LIMIT: usize = 2;
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_QUEUE_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
static GLOBAL_CONNECTIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(GLOBAL_CONNECTION_LIMIT)));
static GLOBAL_TRANSFERS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(GLOBAL_TRANSFER_LIMIT)));
static MAX_READ_SIZE: LazyLock<usize> = LazyLock::new(|| {
    configured_limit(
        "HOT_TASK_FILE_MAX_TRANSFER_BYTES",
        HARD_MAX_READ_SIZE,
        HARD_MAX_READ_SIZE,
    )
});
static MAX_LIST_FILES: LazyLock<usize> = LazyLock::new(|| {
    configured_limit(
        "HOT_TASK_FILE_LIST_LIMIT",
        HARD_MAX_LIST_FILES,
        HARD_MAX_LIST_FILES,
    )
});

/// Context for the file server — identifies the task's org/env/user scope.
#[derive(Clone)]
pub struct FileServerContext {
    pub org_id: Uuid,
    pub env_id: Uuid,
    pub user_id: Uuid,
    pub run_id: Option<Uuid>,
    pub auth_token: String,
    pub db: Arc<DatabasePool>,
    pub storage: Arc<dyn FileStorage>,
}

/// Transport type for the file server.
#[derive(Debug, Clone)]
pub enum FileServerTransport {
    Unix(PathBuf),
    Tcp(u16),
    /// Kernel AF_VSOCK (used by QEMU/Kata). Host binds VMADDR_CID_ANY:port.
    #[cfg(all(target_os = "linux", feature = "kata"))]
    VsockAf {
        port: u32,
    },
    /// Hybrid vsock UDS (used by Firecracker). Preserved for future bare-metal Firecracker use.
    #[cfg(all(target_os = "linux", feature = "kata"))]
    VsockUds {
        port: u32,
        uds_path: PathBuf,
    },
}

/// Handle to a running file server. Dropping it signals shutdown.
pub struct FileServerHandle {
    pub transport: FileServerTransport,
    auth_token: String,
    shutdown_tx: oneshot::Sender<()>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl FileServerHandle {
    /// Get the socket path (only valid for Unix transport).
    pub fn socket_path(&self) -> &Path {
        match &self.transport {
            FileServerTransport::Unix(p) => p,
            FileServerTransport::Tcp(_) => Path::new(""),
            #[cfg(all(target_os = "linux", feature = "kata"))]
            FileServerTransport::VsockAf { .. } | FileServerTransport::VsockUds { .. } => {
                Path::new("")
            }
        }
    }

    /// Get the TCP port (only valid for Tcp transport).
    pub fn tcp_port(&self) -> Option<u16> {
        match &self.transport {
            FileServerTransport::Tcp(port) => Some(*port),
            _ => None,
        }
    }

    /// Get the vsock port (only valid for Vsock transport).
    #[cfg(all(target_os = "linux", feature = "kata"))]
    pub fn vsock_port(&self) -> Option<u32> {
        match &self.transport {
            FileServerTransport::VsockAf { port } | FileServerTransport::VsockUds { port, .. } => {
                Some(*port)
            }
            _ => None,
        }
    }

    /// Whether this server uses vsock transport (either AF_VSOCK or hybrid UDS).
    pub fn is_vsock(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "kata"))]
        if matches!(
            self.transport,
            FileServerTransport::VsockAf { .. } | FileServerTransport::VsockUds { .. }
        ) {
            return true;
        }
        false
    }

    /// Whether this server uses TCP transport.
    pub fn is_tcp(&self) -> bool {
        matches!(self.transport, FileServerTransport::Tcp(_))
    }

    /// Shared bearer token required by clients connecting to the file server.
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// Shut down the file server and wait for it to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.join_handle.await;
        match &self.transport {
            FileServerTransport::Unix(path) => {
                let _ = tokio::fs::remove_file(path).await;
            }
            FileServerTransport::Tcp(_) => {}
            #[cfg(all(target_os = "linux", feature = "kata"))]
            FileServerTransport::VsockAf { .. } => {}
            #[cfg(all(target_os = "linux", feature = "kata"))]
            FileServerTransport::VsockUds { uds_path, .. } => {
                let _ = tokio::fs::remove_file(uds_path).await;
            }
        }
    }
}

/// Start a per-task file server on a unix socket.
///
/// Creates a unix socket at `socket_dir/hotbox-{task_id}.sock` and starts
/// serving HTTP requests. Returns a handle for lifecycle management.
#[cfg(unix)]
pub async fn start(
    task_id: &Uuid,
    socket_dir: &Path,
    ctx: FileServerContext,
) -> Result<FileServerHandle, std::io::Error> {
    let socket_path = socket_dir.join(format!("hotbox-{}.sock", task_id));

    // Remove stale socket if it exists
    let _ = tokio::fs::remove_file(&socket_path).await;

    // Ensure directory exists and is accessible to container user
    tokio::fs::create_dir_all(socket_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(0o770));
    }

    let listener = UnixListener::bind(&socket_path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o770));
    }

    tracing::debug!(task_id = %task_id, socket = %socket_path.display(), "File server started on unix socket");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let auth_token = ctx.auth_token.clone();

    let socket_path_clone = socket_path.clone();
    let join_handle = tokio::spawn(async move {
        serve_unix(listener, ctx, shutdown_rx).await;
        tracing::debug!(socket = %socket_path_clone.display(), "File server stopped");
    });

    Ok(FileServerHandle {
        transport: FileServerTransport::Unix(socket_path),
        auth_token,
        shutdown_tx,
        join_handle,
    })
}

/// Pre-bound AF_VSOCK listener, ready to be handed to `start_vsock_af_with_listener`.
///
/// Created by `reserve_vsock_port` so the actual port is known before the
/// container is created (the guest needs `HOTBOX_VSOCK_PORT` in its env).
#[cfg(all(target_os = "linux", feature = "kata"))]
pub struct ReservedVsockPort {
    pub port: u32,
    listener: tokio_vsock::VsockListener,
}

/// Bind an AF_VSOCK port, retrying with random alternatives on collision.
///
/// Returns the listener and the actual bound port. Call this *before* creating
/// the container so the env var matches the port we actually got.
#[cfg(all(target_os = "linux", feature = "kata"))]
pub fn reserve_vsock_port(preferred: u32) -> Result<ReservedVsockPort, std::io::Error> {
    use tokio_vsock::{VsockAddr, VsockListener};

    match VsockListener::bind(VsockAddr::new(libc::VMADDR_CID_ANY, preferred)) {
        Ok(listener) => {
            return Ok(ReservedVsockPort {
                port: preferred,
                listener,
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::warn!(port = preferred, "vsock port in use, trying alternatives");
        }
        Err(e) => return Err(e),
    }

    // Preferred port taken — try sequential alternatives in the high range.
    // Start from preferred+1 and wrap within [9200, 9200+65535].
    for i in 1..=64u32 {
        let alt = 9200 + ((preferred - 9200 + i) & 0xFFFF);
        match VsockListener::bind(VsockAddr::new(libc::VMADDR_CID_ANY, alt)) {
            Ok(listener) => {
                tracing::debug!(
                    original = preferred,
                    actual = alt,
                    "vsock bound to alternative port"
                );
                return Ok(ReservedVsockPort {
                    port: alt,
                    listener,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!(
            "no available vsock port after 16 attempts (preferred: {})",
            preferred
        ),
    ))
}

/// Start serving on a pre-reserved AF_VSOCK listener.
#[cfg(all(target_os = "linux", feature = "kata"))]
pub async fn start_vsock_af(
    task_id: &Uuid,
    reserved: ReservedVsockPort,
    ctx: FileServerContext,
) -> FileServerHandle {
    let port = reserved.port;

    tracing::debug!(
        task_id = %task_id,
        vsock_port = port,
        "File server started on AF_VSOCK"
    );

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let auth_token = ctx.auth_token.clone();

    let join_handle = tokio::spawn(async move {
        serve_vsock_af(reserved.listener, ctx, shutdown_rx).await;
        tracing::debug!(port, "File server (AF_VSOCK) stopped");
    });

    FileServerHandle {
        transport: FileServerTransport::VsockAf { port },
        auth_token,
        shutdown_tx,
        join_handle,
    }
}

/// Serve connections from a kernel AF_VSOCK listener.
#[cfg(all(target_os = "linux", feature = "kata"))]
async fn serve_vsock_af(
    listener: tokio_vsock::VsockListener,
    ctx: FileServerContext,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let task_connections = Arc::new(Semaphore::new(PER_TASK_CONNECTION_LIMIT));
    let task_transfers = Arc::new(Semaphore::new(PER_TASK_TRANSFER_LIMIT));
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let Ok(global_permit) = Arc::clone(&GLOBAL_CONNECTIONS).try_acquire_owned() else {
                            continue;
                        };
                        let Ok(task_permit) = Arc::clone(&task_connections).try_acquire_owned() else {
                            continue;
                        };
                        let ctx = ctx.clone();
                        let task_transfers = Arc::clone(&task_transfers);
                        tokio::spawn(async move {
                            let _permits = (global_permit, task_permit);
                            if let Err(e) =
                                handle_vsock_af_connection(stream, ctx, task_transfers).await
                            {
                                tracing::debug!("File server AF_VSOCK connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("File server AF_VSOCK accept error: {}", e);
                    }
                }
            }
        }
    }
}

/// Handle a single connection from a VsockStream (AF_VSOCK).
#[cfg(all(target_os = "linux", feature = "kata"))]
async fn handle_vsock_af_connection(
    stream: tokio_vsock::VsockStream,
    ctx: FileServerContext,
    task_transfers: Arc<Semaphore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, write_half) = tokio::io::split(stream);
    let reader = BufReader::new(read_half);
    handle_request(reader, write_half, ctx, task_transfers).await
}

/// Start a per-task file server on a Firecracker hybrid vsock UDS path.
///
/// **Preserved for future use** — when running Firecracker directly (e.g. on
/// bare metal instances), the VMM uses hybrid vsock: guest CID=2:port maps to
/// a Unix domain socket `<vsock_uds_path>_<port>` on the host. This is NOT
/// used with QEMU/Kata which uses kernel AF_VSOCK instead.
///
/// See `start_vsock_af()` for the active QEMU implementation.
#[cfg(all(target_os = "linux", feature = "kata"))]
pub async fn start_vsock_uds(
    task_id: &Uuid,
    listener_path: &std::path::Path,
    port: u32,
    ctx: FileServerContext,
) -> Result<FileServerHandle, std::io::Error> {
    // Remove stale socket if it exists
    let _ = tokio::fs::remove_file(listener_path).await;

    let listener = UnixListener::bind(listener_path)?;

    tracing::debug!(
        task_id = %task_id,
        vsock_port = port,
        path = %listener_path.display(),
        "File server started on vsock UDS"
    );

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let auth_token = ctx.auth_token.clone();

    let path_display = listener_path.display().to_string();
    let join_handle = tokio::spawn(async move {
        serve_unix(listener, ctx, shutdown_rx).await;
        tracing::debug!(path = %path_display, "File server (vsock UDS) stopped");
    });

    Ok(FileServerHandle {
        transport: FileServerTransport::VsockUds {
            port,
            uds_path: listener_path.to_path_buf(),
        },
        auth_token,
        shutdown_tx,
        join_handle,
    })
}

#[cfg(unix)]
async fn serve_unix(
    listener: UnixListener,
    ctx: FileServerContext,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let task_connections = Arc::new(Semaphore::new(PER_TASK_CONNECTION_LIMIT));
    let task_transfers = Arc::new(Semaphore::new(PER_TASK_TRANSFER_LIMIT));
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let Ok(global_permit) = Arc::clone(&GLOBAL_CONNECTIONS).try_acquire_owned() else {
                            continue;
                        };
                        let Ok(task_permit) = Arc::clone(&task_connections).try_acquire_owned() else {
                            continue;
                        };
                        let ctx = ctx.clone();
                        let task_transfers = Arc::clone(&task_transfers);
                        tokio::spawn(async move {
                            let _permits = (global_permit, task_permit);
                            if let Err(e) = handle_connection(stream, ctx, task_transfers).await {
                                tracing::debug!("File server connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("File server accept error: {}", e);
                    }
                }
            }
        }
    }
}

/// Start a per-task file server on a TCP port (for macOS Docker where unix socket
/// bind-mounts don't work through VirtioFS). Binds to 127.0.0.1:0 (OS-assigned port).
pub async fn start_tcp(
    task_id: &Uuid,
    ctx: FileServerContext,
) -> Result<FileServerHandle, std::io::Error> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tracing::debug!(task_id = %task_id, port, "File server started on TCP");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let auth_token = ctx.auth_token.clone();
    let join_handle = tokio::spawn(async move {
        serve_tcp(listener, ctx, shutdown_rx).await;
        tracing::debug!(port, "File server (TCP) stopped");
    });

    Ok(FileServerHandle {
        transport: FileServerTransport::Tcp(port),
        auth_token,
        shutdown_tx,
        join_handle,
    })
}

async fn serve_tcp(
    listener: tokio::net::TcpListener,
    ctx: FileServerContext,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let task_connections = Arc::new(Semaphore::new(PER_TASK_CONNECTION_LIMIT));
    let task_transfers = Arc::new(Semaphore::new(PER_TASK_TRANSFER_LIMIT));
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let Ok(global_permit) = Arc::clone(&GLOBAL_CONNECTIONS).try_acquire_owned() else {
                            continue;
                        };
                        let Ok(task_permit) = Arc::clone(&task_connections).try_acquire_owned() else {
                            continue;
                        };
                        let ctx = ctx.clone();
                        let task_transfers = Arc::clone(&task_transfers);
                        tokio::spawn(async move {
                            let _permits = (global_permit, task_permit);
                            if let Err(e) =
                                handle_tcp_connection(stream, ctx, task_transfers).await
                            {
                                tracing::debug!("File server TCP connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("File server TCP accept error: {}", e);
                    }
                }
            }
        }
    }
}

async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    ctx: FileServerContext,
    task_transfers: Arc<Semaphore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    handle_request(reader, write_half, ctx, task_transfers).await
}

#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    ctx: FileServerContext,
    task_transfers: Arc<Semaphore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    handle_request(reader, write_half, ctx, task_transfers).await
}

/// Handle a single HTTP/1.1 request. Generic over transport (Unix socket, TCP, vsock).
async fn handle_request<R, W>(
    mut reader: BufReader<R>,
    mut write_half: W,
    ctx: FileServerContext,
    task_transfers: Arc<Semaphore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let request_deadline = tokio::time::Instant::now() + REQUEST_DEADLINE;

    // Read request line
    let request_line = match tokio::time::timeout_at(
        request_deadline,
        read_bounded_line(&mut reader, MAX_REQUEST_LINE_BYTES),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return Ok(()),
        _ => {
            write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
            return Ok(());
        }
    };
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() != 3 || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1") {
        write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
        return Ok(());
    }
    let method = parts[0];
    let uri = parts[1];

    // Read headers
    let mut content_length: usize = 0;
    let mut saw_content_length = false;
    let mut header_count = 0usize;
    let mut header_bytes = 0usize;
    let mut headers = HashMap::new();
    loop {
        let line = match tokio::time::timeout_at(
            request_deadline,
            read_bounded_line(&mut reader, MAX_HEADER_BYTES),
        )
        .await
        {
            Ok(Ok(Some(line))) => line,
            _ => {
                write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
                return Ok(());
            }
        };
        header_bytes = header_bytes.saturating_add(line.len());
        if line == "\r\n" || line == "\n" {
            break;
        }
        header_count += 1;
        if header_count > MAX_HEADER_COUNT || header_bytes > MAX_HEADER_BYTES {
            write_http_response(
                &mut write_half,
                431,
                "text/plain",
                b"Request headers too large",
            )
            .await?;
            return Ok(());
        }
        let Some((key, value)) = line.split_once(':') else {
            write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
            return Ok(());
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if key.is_empty() || key == "transfer-encoding" {
            write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
            return Ok(());
        }
        if key == "content-length" {
            if saw_content_length {
                write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
                return Ok(());
            }
            saw_content_length = true;
            content_length = match value.parse() {
                Ok(value) => value,
                Err(_) => {
                    write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
                    return Ok(());
                }
            };
        }
        headers.insert(key, value);
    }

    if !is_authorized(&headers, &ctx.auth_token) {
        write_http_response(&mut write_half, 401, "text/plain", b"Unauthorized").await?;
        return Ok(());
    }
    if !matches!(method, "GET" | "PUT" | "HEAD" | "DELETE") {
        write_http_response(&mut write_half, 405, "text/plain", b"Method not allowed").await?;
        return Ok(());
    }
    if method != "PUT" && content_length != 0 {
        write_http_response(&mut write_half, 400, "text/plain", b"Bad request").await?;
        return Ok(());
    }

    // Authenticated file operations are more memory- and I/O-intensive than
    // health checks. Keep their concurrency low even when many connections
    // have passed authentication.
    let _transfer_permits = if uri.starts_with("/files") {
        let Ok(Ok(global_permit)) = tokio::time::timeout(
            TRANSFER_QUEUE_TIMEOUT,
            Arc::clone(&GLOBAL_TRANSFERS).acquire_owned(),
        )
        .await
        else {
            write_http_response(&mut write_half, 503, "text/plain", b"File server busy").await?;
            return Ok(());
        };
        let Ok(Ok(task_permit)) =
            tokio::time::timeout(TRANSFER_QUEUE_TIMEOUT, task_transfers.acquire_owned()).await
        else {
            write_http_response(&mut write_half, 503, "text/plain", b"File server busy").await?;
            return Ok(());
        };
        Some((global_permit, task_permit))
    } else {
        None
    };

    // The storage API currently requires one byte slice, so stream into a
    // strictly bounded buffer instead of allocating Content-Length up front.
    let body = if content_length > MAX_BODY_SIZE {
        write_http_response(&mut write_half, 413, "text/plain", b"Payload too large").await?;
        return Ok(());
    } else if content_length > 0 {
        let mut buf = Vec::with_capacity(content_length.min(64 * 1024));
        let mut remaining = content_length;
        let mut chunk = [0u8; 64 * 1024];
        while remaining > 0 {
            let count = remaining.min(chunk.len());
            tokio::time::timeout(READ_IDLE_TIMEOUT, reader.read_exact(&mut chunk[..count]))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "body read idle")
                })??;
            buf.extend_from_slice(&chunk[..count]);
            remaining -= count;
        }
        buf
    } else {
        vec![]
    };

    // Route request
    let file_ctx = hot::file_storage::FileStorageContext {
        db: ctx.db.clone(),
        org_id: ctx.org_id,
        env_id: Some(ctx.env_id),
        user_id: ctx.user_id,
        run_id: ctx.run_id,
        run_provenance: None,
        file_max_bytes_conf: None,
    };

    if uri == "/health" {
        write_http_response(&mut write_half, 200, "text/plain", b"ok").await?;
    } else if uri.starts_with("/files?prefix=") {
        // GET /files?prefix=<prefix> — list files
        let prefix = uri.strip_prefix("/files?prefix=").unwrap_or("");
        let prefix = urlencoding_decode(prefix);
        match tokio::time::timeout(
            REQUEST_DEADLINE,
            handle_list(&mut write_half, &ctx.storage, &file_ctx, &prefix),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                write_http_response(
                    &mut write_half,
                    504,
                    "text/plain",
                    b"File operation timed out",
                )
                .await?;
            }
        }
    } else if let Some(path) = uri.strip_prefix("/files/") {
        let path = urlencoding_decode(path);
        match method {
            "GET" => handle_read(&mut write_half, &ctx.storage, &file_ctx, &path).await?,
            "PUT" => {
                match tokio::time::timeout(
                    REQUEST_DEADLINE,
                    handle_write(&mut write_half, &ctx.storage, &file_ctx, &path, body),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        write_http_response(
                            &mut write_half,
                            504,
                            "text/plain",
                            b"File operation timed out",
                        )
                        .await?;
                    }
                }
            }
            "HEAD" => {
                match tokio::time::timeout(
                    REQUEST_DEADLINE,
                    handle_head(&mut write_half, &ctx.storage, &file_ctx, &path),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        write_http_response(
                            &mut write_half,
                            504,
                            "text/plain",
                            b"File operation timed out",
                        )
                        .await?;
                    }
                }
            }
            "DELETE" => {
                match tokio::time::timeout(
                    REQUEST_DEADLINE,
                    handle_delete(&mut write_half, &ctx.storage, &file_ctx, &path),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        write_http_response(
                            &mut write_half,
                            504,
                            "text/plain",
                            b"File operation timed out",
                        )
                        .await?;
                    }
                }
            }
            _ => {
                write_http_response(&mut write_half, 405, "text/plain", b"Method not allowed")
                    .await?;
            }
        }
    } else {
        write_http_response(&mut write_half, 404, "text/plain", b"Not found").await?;
    }

    Ok(())
}

fn is_authorized(headers: &HashMap<String, String>, expected_token: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected_token.as_bytes()))
}

fn constant_time_eq(actual: &[u8], expected: &[u8]) -> bool {
    let mut difference = actual.len() ^ expected.len();
    let max_len = actual.len().max(expected.len());
    for index in 0..max_len {
        difference |= usize::from(
            actual.get(index).copied().unwrap_or(0) ^ expected.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<String>, std::io::Error> {
    use tokio::io::AsyncBufReadExt;

    let mut bytes = Vec::with_capacity(256);
    loop {
        let available = tokio::time::timeout(READ_IDLE_TIMEOUT, reader.fill_buf())
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "header read idle"))??;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unterminated HTTP line",
                ))
            };
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(consumed) > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP line too large",
            ));
        }
        bytes.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if bytes.last() == Some(&b'\n') {
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "HTTP line is not UTF-8")
            });
        }
    }
}

async fn handle_read<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    storage: &Arc<dyn FileStorage>,
    ctx: &hot::file_storage::FileStorageContext,
    path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream_result =
        match tokio::time::timeout(REQUEST_DEADLINE, storage.open_file_stream(path, ctx)).await {
            Ok(result) => result,
            Err(_) => {
                write_http_response(writer, 504, "text/plain", b"File operation timed out").await?;
                return Ok(());
            }
        };

    match stream_result {
        Ok(Some(mut stream)) => {
            let size = match checked_read_size(stream.metadata.size) {
                Ok(size) => size,
                Err(()) => {
                    write_http_response(writer, 413, "text/plain", b"File too large").await?;
                    return Ok(());
                }
            };
            write_streaming_response(
                writer,
                "application/octet-stream",
                size,
                stream.reader.as_mut(),
            )
            .await?;
        }
        Ok(None) => {
            let metadata =
                match tokio::time::timeout(REQUEST_DEADLINE, storage.get_file_metadata(path, ctx))
                    .await
                {
                    Ok(Ok(metadata)) => metadata,
                    Ok(Err(e)) => {
                        let msg = format!("File read error: {}", e);
                        write_http_response(writer, 404, "text/plain", msg.as_bytes()).await?;
                        return Ok(());
                    }
                    Err(_) => {
                        write_http_response(writer, 504, "text/plain", b"File operation timed out")
                            .await?;
                        return Ok(());
                    }
                };
            let size = match checked_read_size(metadata.size) {
                Ok(size) => size,
                Err(()) => {
                    write_http_response(writer, 413, "text/plain", b"File too large").await?;
                    return Ok(());
                }
            };
            if size > MAX_BUFFERED_READ_SIZE {
                write_http_response(writer, 413, "text/plain", b"File too large").await?;
                return Ok(());
            }
            let data =
                match tokio::time::timeout(REQUEST_DEADLINE, storage.read_file(path, ctx)).await {
                    Ok(result) => result?,
                    Err(_) => {
                        write_http_response(writer, 504, "text/plain", b"File operation timed out")
                            .await?;
                        return Ok(());
                    }
                };
            if data.len() > size {
                write_http_response(writer, 500, "text/plain", b"File size changed").await?;
                return Ok(());
            }
            write_http_response(writer, 200, "application/octet-stream", &data).await?;
        }
        Err(e) => {
            let msg = format!("File read error: {}", e);
            write_http_response(writer, 404, "text/plain", msg.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn handle_write<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    storage: &Arc<dyn FileStorage>,
    ctx: &hot::file_storage::FileStorageContext,
    path: &str,
    body: Vec<u8>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match storage.write_file(path, &body, None, ctx).await {
        Ok(_meta) => {
            write_http_response(writer, 200, "text/plain", b"OK").await?;
        }
        Err(e) => {
            let msg = format!("File write error: {}", e);
            write_http_response(writer, 500, "text/plain", msg.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn handle_head<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    storage: &Arc<dyn FileStorage>,
    ctx: &hot::file_storage::FileStorageContext,
    path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match storage.get_file_metadata(path, ctx).await {
        Ok(meta) => {
            let meta_json = serde_json::json!({
                "file-id": meta.file_id.to_string(),
                "path": meta.path,
                "size": meta.size,
                "etag": meta.etag,
                "content-type": meta.content_type,
                "storage-backend": meta.storage_backend,
                "created-at": meta.created_at.timestamp_millis(),
                "updated-at": meta.updated_at.timestamp_millis(),
            });
            let meta_str = serde_json::to_string(&meta_json).unwrap_or_default();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-File-Meta: {}\r\nConnection: close\r\n\r\n",
                meta_str
            );
            writer.write_all(header.as_bytes()).await?;
        }
        Err(e) => {
            let msg = format!("File not found: {}", e);
            write_http_response(writer, 404, "text/plain", msg.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn handle_delete<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    storage: &Arc<dyn FileStorage>,
    ctx: &hot::file_storage::FileStorageContext,
    path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match storage.delete_file(path, ctx).await {
        Ok(_) => {
            write_http_response(writer, 200, "text/plain", b"Deleted").await?;
        }
        Err(e) => {
            let msg = format!("Delete error: {}", e);
            write_http_response(writer, 500, "text/plain", msg.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn handle_list<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    storage: &Arc<dyn FileStorage>,
    ctx: &hot::file_storage::FileStorageContext,
    prefix: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let query_limit = MAX_LIST_FILES.saturating_add(1);
    match storage.list_files_bounded(prefix, query_limit, ctx).await {
        Ok(files) => {
            if files.len() > *MAX_LIST_FILES {
                write_http_response(
                    writer,
                    413,
                    "text/plain",
                    b"File listing exceeds the configured limit",
                )
                .await?;
                return Ok(());
            }
            let json_files: Vec<serde_json::Value> = files
                .into_iter()
                .map(|meta| {
                    serde_json::json!({
                        "path": meta.path,
                        "size": meta.size,
                        "content-type": meta.content_type,
                        "created-at": meta.created_at.timestamp_millis(),
                    })
                })
                .collect();
            let body = serde_json::to_vec(&json_files).unwrap_or_default();
            if body.len() > MAX_LIST_RESPONSE_BYTES {
                write_http_response(writer, 413, "text/plain", b"File listing too large").await?;
            } else {
                write_http_response(writer, 200, "application/json", &body).await?;
            }
        }
        Err(e) => {
            let msg = format!("List error: {}", e);
            write_http_response(writer, 500, "text/plain", msg.as_bytes()).await?;
        }
    }
    Ok(())
}

async fn write_http_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), std::io::Error> {
    write_http_response_with_headers(writer, status, content_type, &[], body).await
}

async fn write_http_response_with_headers<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(), std::io::Error> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    };

    let mut header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
        status,
        status_text,
        content_type,
        body.len()
    );
    for (name, value) in extra_headers {
        header.push_str(name);
        header.push_str(": ");
        header.push_str(value);
        header.push_str("\r\n");
    }
    header.push_str("Connection: close\r\n\r\n");
    write_all_with_idle_timeout(writer, header.as_bytes()).await?;
    write_all_with_idle_timeout(writer, body).await?;
    Ok(())
}

async fn write_streaming_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    content_type: &str,
    size: usize,
    reader: std::pin::Pin<&mut (dyn tokio::io::AsyncRead + Send)>,
) -> Result<(), std::io::Error> {
    write_streaming_response_with_idle_timeout(
        writer,
        content_type,
        size,
        reader,
        READ_IDLE_TIMEOUT,
    )
    .await
}

async fn write_streaming_response_with_idle_timeout<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    content_type: &str,
    size: usize,
    mut reader: std::pin::Pin<&mut (dyn tokio::io::AsyncRead + Send)>,
    idle_timeout: Duration,
) -> Result<(), std::io::Error> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content_type, size
    );
    write_all_with_timeout(writer, header.as_bytes(), idle_timeout).await?;

    let mut remaining = size;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(buffer.len());
        let read = tokio::time::timeout(idle_timeout, reader.as_mut().read(&mut buffer[..count]))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "file stream read idle")
            })??;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file ended before recorded size",
            ));
        }
        write_all_with_timeout(writer, &buffer[..read], idle_timeout).await?;
        remaining -= read;
    }
    Ok(())
}

async fn write_all_with_idle_timeout<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> Result<(), std::io::Error> {
    write_all_with_timeout(writer, bytes, READ_IDLE_TIMEOUT).await
}

async fn write_all_with_timeout<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    idle_timeout: Duration,
) -> Result<(), std::io::Error> {
    tokio::time::timeout(idle_timeout, writer.write_all(bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "file stream write idle"))?
}

fn checked_read_size(size: i64) -> Result<usize, ()> {
    let size = usize::try_from(size).map_err(|_| ())?;
    if size > *MAX_READ_SIZE {
        return Err(());
    }
    Ok(size)
}

fn configured_limit(name: &str, default: usize, hard_max: usize) -> usize {
    parse_configured_limit(std::env::var(name).ok().as_deref(), default, hard_max)
}

fn parse_configured_limit(value: Option<&str>, default: usize, hard_max: usize) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .min(hard_max)
}

/// Minimal percent-decoding for URL paths.
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            let byte = hex_digit(hi) * 16 + hex_digit(lo);
            result.push(byte as char);
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use hot::file_storage::FileMetadata;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    #[test]
    fn test_urlencoding_decode() {
        assert_eq!(urlencoding_decode("hello%20world"), "hello world");
        assert_eq!(urlencoding_decode("path/to/file.txt"), "path/to/file.txt");
        assert_eq!(urlencoding_decode("a%2Fb"), "a/b");
        assert_eq!(urlencoding_decode("100%25"), "100%");
    }

    #[test]
    fn test_hex_digit() {
        assert_eq!(hex_digit(b'0'), 0);
        assert_eq!(hex_digit(b'9'), 9);
        assert_eq!(hex_digit(b'a'), 10);
        assert_eq!(hex_digit(b'f'), 15);
        assert_eq!(hex_digit(b'A'), 10);
        assert_eq!(hex_digit(b'F'), 15);
    }

    #[test]
    fn test_constant_time_token_comparison() {
        assert!(constant_time_eq(b"task-secret", b"task-secret"));
        assert!(!constant_time_eq(b"task-secret", b"other-secret"));
        assert!(!constant_time_eq(b"task-secret", b"task-secret-longer"));
    }

    #[tokio::test]
    async fn test_bounded_line_rejects_oversized_input() {
        let input = vec![b'a'; MAX_REQUEST_LINE_BYTES + 1];
        let mut reader = tokio::io::BufReader::new(input.as_slice());
        let result = read_bounded_line(&mut reader, MAX_REQUEST_LINE_BYTES).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bounded_line_requires_terminator() {
        let mut reader = tokio::io::BufReader::new(b"GET / HTTP/1.1".as_slice());
        let result = read_bounded_line(&mut reader, MAX_REQUEST_LINE_BYTES).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn streaming_progress_resets_the_idle_timeout() {
        let idle_timeout = Duration::from_secs(1);
        let (mut source_writer, mut source_reader) = tokio::io::duplex(1);
        let producer = tokio::spawn(async move {
            for byte in [b'a', b'b'] {
                tokio::time::sleep(Duration::from_millis(600)).await;
                source_writer.write_all(&[byte]).await.unwrap();
            }
        });
        let mut output = tokio::io::sink();
        let reader = std::pin::Pin::new(&mut source_reader);

        write_streaming_response_with_idle_timeout(
            &mut output,
            "application/octet-stream",
            2,
            reader,
            idle_timeout,
        )
        .await
        .expect("steady streaming progress should not hit an absolute request deadline");
        producer.await.unwrap();
    }

    // -- Integration tests with MockFileStorage --

    struct MockFileStorage {
        files: Mutex<HashMap<String, Vec<u8>>>,
        buffered_reads: AtomicUsize,
        streamed_reads: AtomicUsize,
    }

    impl MockFileStorage {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                buffered_reads: AtomicUsize::new(0),
                streamed_reads: AtomicUsize::new(0),
            }
        }
    }

    fn mock_metadata(path: &str, size: i64) -> FileMetadata {
        let now = chrono::Utc::now();
        FileMetadata {
            file_id: Uuid::nil(),
            path: path.to_string(),
            size,
            etag: Some("mock-etag".to_string()),
            content_type: Some("application/octet-stream".to_string()),
            storage_backend: "mock".to_string(),
            storage_path: format!("mock/{}", path),
            org_id: Uuid::nil(),
            env_id: None,
            created_by_run_id: None,
            updated_by_run_id: None,
            created_at: now,
            updated_at: now,
            created_by_user_id: Uuid::nil(),
            updated_by_user_id: None,
        }
    }

    #[async_trait::async_trait]
    impl hot::file_storage::FileStorage for MockFileStorage {
        async fn write_file(
            &self,
            path: &str,
            content: &[u8],
            _content_type: Option<&str>,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<hot::file_storage::FileMetadata, String> {
            let size = content.len() as i64;
            self.files
                .lock()
                .await
                .insert(path.to_string(), content.to_vec());
            Ok(mock_metadata(path, size))
        }

        async fn write_file_if(
            &self,
            path: &str,
            content: &[u8],
            _content_type: Option<&str>,
            expected_etag: Option<&str>,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<Option<hot::file_storage::FileMetadata>, String> {
            let mut files = self.files.lock().await;
            let current = files.get(path).map(|b| hot::file_storage::compute_md5(b));
            if current.as_deref() != expected_etag {
                return Ok(None);
            }
            let size = content.len() as i64;
            files.insert(path.to_string(), content.to_vec());
            Ok(Some(mock_metadata(path, size)))
        }

        async fn read_file(
            &self,
            path: &str,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<Vec<u8>, String> {
            self.buffered_reads.fetch_add(1, Ordering::Relaxed);
            self.files
                .lock()
                .await
                .get(path)
                .cloned()
                .ok_or_else(|| format!("Not found: {}", path))
        }

        async fn open_file_stream(
            &self,
            path: &str,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<Option<hot::file_storage::FileReadStream>, String> {
            let data = self
                .files
                .lock()
                .await
                .get(path)
                .cloned()
                .ok_or_else(|| format!("Not found: {}", path))?;
            self.streamed_reads.fetch_add(1, Ordering::Relaxed);
            Ok(Some(hot::file_storage::FileReadStream {
                metadata: mock_metadata(path, data.len() as i64),
                reader: Box::pin(std::io::Cursor::new(data)),
            }))
        }

        async fn delete_file(
            &self,
            path: &str,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<bool, String> {
            Ok(self.files.lock().await.remove(path).is_some())
        }

        async fn file_exists(
            &self,
            path: &str,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<bool, String> {
            Ok(self.files.lock().await.contains_key(path))
        }

        async fn get_file_metadata(
            &self,
            path: &str,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<hot::file_storage::FileMetadata, String> {
            let files = self.files.lock().await;
            match files.get(path) {
                Some(data) => Ok(mock_metadata(path, data.len() as i64)),
                None => Err(format!("Not found: {}", path)),
            }
        }

        async fn list_files(
            &self,
            prefix: &str,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<Vec<hot::file_storage::FileMetadata>, String> {
            let files = self.files.lock().await;
            let results: Vec<_> = files
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| mock_metadata(k, v.len() as i64))
                .collect();
            Ok(results)
        }

        async fn list_files_bounded(
            &self,
            prefix: &str,
            limit: usize,
            _ctx: &hot::file_storage::FileStorageContext,
        ) -> Result<Vec<hot::file_storage::FileMetadata>, String> {
            let files = self.files.lock().await;
            Ok(files
                .iter()
                .filter(|(path, _)| path.starts_with(prefix))
                .take(limit)
                .map(|(path, data)| mock_metadata(path, data.len() as i64))
                .collect())
        }

        fn storage_type(&self) -> &str {
            "mock"
        }
    }

    /// Helper: start a file server with MockFileStorage, return (handle, socket_path).
    async fn start_test_server() -> (FileServerHandle, PathBuf) {
        start_test_server_with_storage(Arc::new(MockFileStorage::new())).await
    }

    async fn start_test_server_with_storage(
        storage: Arc<dyn FileStorage>,
    ) -> (FileServerHandle, PathBuf) {
        // Use a short task ID to keep socket path under SUN_LEN (104 bytes on macOS)
        let task_id = Uuid::new_v4();
        let socket_dir = PathBuf::from("/tmp/hbx");
        let _ = std::fs::create_dir_all(&socket_dir);
        // MockFileStorage ignores the DB, but FileServerContext requires one.
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("Failed to create in-memory SQLite pool");
        let db = hot::db::DatabasePool::Sqlite(pool);

        let ctx = FileServerContext {
            org_id: Uuid::nil(),
            env_id: Uuid::nil(),
            user_id: Uuid::nil(),
            run_id: None,
            auth_token: "test-token".to_string(),
            db: Arc::new(db),
            storage,
        };

        let handle = start(&task_id, &socket_dir, ctx)
            .await
            .expect("Failed to start file server");
        let socket_path = handle.socket_path().to_path_buf();
        (handle, socket_path)
    }

    /// Helper: send a raw HTTP request over a unix socket and parse the response.
    async fn http_request(
        socket: &Path,
        method: &str,
        uri: &str,
        body: Option<&[u8]>,
    ) -> (u16, HashMap<String, String>, Vec<u8>) {
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();

        let content_length = body.map_or(0, |b| b.len());
        let req = format!(
            "{} {} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer test-token\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            method, uri, content_length
        );
        write_half.write_all(req.as_bytes()).await.unwrap();
        if let Some(data) = body {
            write_half.write_all(data).await.unwrap();
        }
        write_half.shutdown().await.unwrap();

        let mut reader = tokio::io::BufReader::new(read_half);

        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.unwrap();
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let mut headers = HashMap::new();
        let mut resp_content_length: usize = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let k = key.trim().to_lowercase();
                let v = value.trim().to_string();
                if k == "content-length" {
                    resp_content_length = v.parse().unwrap_or(0);
                }
                headers.insert(k, v);
            }
        }

        let mut resp_body = vec![0u8; resp_content_length];
        if resp_content_length > 0 {
            reader.read_exact(&mut resp_body).await.unwrap();
        }

        (status, headers, resp_body)
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let (handle, socket) = start_test_server().await;
        let (status, _, body) = http_request(&socket, "GET", "/health", None).await;
        assert_eq!(status, 200);
        assert_eq!(body, b"ok");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_write_and_read_file() {
        let storage = Arc::new(MockFileStorage::new());
        let (handle, socket) = start_test_server_with_storage(storage.clone()).await;
        let content = b"hello, hot storage!";

        let (status, _, _) = http_request(&socket, "PUT", "/files/test.txt", Some(content)).await;
        assert_eq!(status, 200);

        let (status, _, body) = http_request(&socket, "GET", "/files/test.txt", None).await;
        assert_eq!(status, 200);
        assert_eq!(body, content);
        assert_eq!(storage.streamed_reads.load(Ordering::Relaxed), 1);
        assert_eq!(storage.buffered_reads.load(Ordering::Relaxed), 0);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let (handle, socket) = start_test_server().await;

        let (status, _, _) = http_request(&socket, "GET", "/files/nope.txt", None).await;
        assert_eq!(status, 404);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_head_file_metadata() {
        let (handle, socket) = start_test_server().await;
        let content = b"metadata test content";

        http_request(&socket, "PUT", "/files/meta.txt", Some(content)).await;

        let (status, headers, body) = http_request(&socket, "HEAD", "/files/meta.txt", None).await;
        assert_eq!(status, 200);
        assert!(body.is_empty(), "HEAD response should have no body");

        let meta_str = headers
            .get("x-file-meta")
            .expect("Missing X-File-Meta header");
        let meta: serde_json::Value = serde_json::from_str(meta_str).unwrap();
        assert_eq!(meta["path"], "meta.txt");
        assert_eq!(meta["size"], content.len() as i64);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_head_nonexistent_file() {
        let (handle, socket) = start_test_server().await;

        let (status, _, _) = http_request(&socket, "HEAD", "/files/ghost.txt", None).await;
        assert_eq!(status, 404);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_delete_file() {
        let (handle, socket) = start_test_server().await;
        http_request(&socket, "PUT", "/files/del.txt", Some(b"delete me")).await;

        let (status, _, _) = http_request(&socket, "DELETE", "/files/del.txt", None).await;
        assert_eq!(status, 200);

        let (status, _, _) = http_request(&socket, "GET", "/files/del.txt", None).await;
        assert_eq!(status, 404);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_list_files() {
        let (handle, socket) = start_test_server().await;
        http_request(&socket, "PUT", "/files/docs/a.txt", Some(b"aaa")).await;
        http_request(&socket, "PUT", "/files/docs/b.txt", Some(b"bb")).await;
        http_request(&socket, "PUT", "/files/other.txt", Some(b"x")).await;

        let (status, _, body) = http_request(&socket, "GET", "/files?prefix=docs/", None).await;
        assert_eq!(status, 200);

        let files: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(files.len(), 2);
        let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"docs/a.txt"));
        assert!(paths.contains(&"docs/b.txt"));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_list_files_empty_prefix() {
        let (handle, socket) = start_test_server().await;
        http_request(&socket, "PUT", "/files/one.txt", Some(b"1")).await;

        let (status, _, body) = http_request(&socket, "GET", "/files?prefix=", None).await;
        assert_eq!(status, 200);

        let files: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(files.len(), 1);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_oversized_file_listing_fails_instead_of_returning_partial_results() {
        let storage = Arc::new(MockFileStorage::new());
        {
            let mut files = storage.files.lock().await;
            for index in 0..=*MAX_LIST_FILES {
                files.insert(format!("docs/{index:04}.txt"), vec![b'x']);
            }
        }
        let (handle, socket) = start_test_server_with_storage(storage).await;

        let (status, headers, body) =
            http_request(&socket, "GET", "/files?prefix=docs/", None).await;
        assert_eq!(status, 413);
        assert!(!headers.contains_key("x-hotbox-list-truncated"));
        assert_eq!(body, b"File listing exceeds the configured limit");

        handle.shutdown().await;
    }

    #[test]
    fn test_transfer_configuration_cannot_exceed_hard_caps() {
        assert_eq!(parse_configured_limit(Some("1024"), 100, 500), 500);
        assert_eq!(parse_configured_limit(Some("25"), 100, 500), 25);
        assert_eq!(parse_configured_limit(Some("0"), 100, 500), 100);
        assert_eq!(parse_configured_limit(Some("invalid"), 100, 500), 100);
        assert_eq!(parse_configured_limit(None, 100, 500), 100);
    }

    #[test]
    fn test_read_size_is_bounded() {
        assert_eq!(checked_read_size(*MAX_READ_SIZE as i64), Ok(*MAX_READ_SIZE));
        assert_eq!(checked_read_size(*MAX_READ_SIZE as i64 + 1), Err(()));
        assert_eq!(checked_read_size(-1), Err(()));
    }

    #[tokio::test]
    async fn test_method_not_allowed() {
        let (handle, socket) = start_test_server().await;

        let (status, _, _) = http_request(&socket, "PATCH", "/files/test.txt", None).await;
        assert_eq!(status, 405);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_not_found_path() {
        let (handle, socket) = start_test_server().await;

        let (status, _, _) = http_request(&socket, "GET", "/unknown", None).await;
        assert_eq!(status, 404);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_urlencoded_path() {
        let (handle, socket) = start_test_server().await;
        let content = b"encoded path content";

        let (status, _, _) =
            http_request(&socket, "PUT", "/files/my%20file.txt", Some(content)).await;
        assert_eq!(status, 200);

        let (status, _, body) = http_request(&socket, "GET", "/files/my%20file.txt", None).await;
        assert_eq!(status, 200);
        assert_eq!(body, content);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn test_rejects_missing_auth() {
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

        let (handle, socket) = start_test_server().await;
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();

        write_half
            .write_all(b"GET /health HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        write_half.shutdown().await.unwrap();

        let mut reader = tokio::io::BufReader::new(read_half);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.unwrap();
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut resp = Vec::new();
        reader.read_to_end(&mut resp).await.unwrap();

        assert_eq!(status, 401);

        handle.shutdown().await;
    }
}
