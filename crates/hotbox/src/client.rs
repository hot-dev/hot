//! HTTP client that talks to the host-side file server over a unix socket.
//!
//! Uses raw tokio I/O — no HTTP framework dependency. The file server speaks
//! minimal HTTP/1.1, so we can construct requests by hand.
//!
//! Transport selection:
//! - Unix socket (default): connects to `HOTBOX_SOCKET` (default `/hot/hotbox.sock`)
//! - TCP fallback: if `HOTBOX_URL` is set (e.g. `http://localhost:9119`)

use std::fmt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const DEFAULT_SOCKET: &str = "/hot/hotbox.sock";

#[derive(Clone)]
enum Transport {
    Unix(String),
    Tcp(String, u16),
    /// vsock transport for Firecracker microVMs. (cid, port)
    /// CID 2 = host from inside a guest VM.
    #[cfg(target_os = "linux")]
    Vsock(u32, u32),
}

pub struct HotboxClient {
    transport: Transport,
    auth_token: Option<String>,
}

#[derive(Debug)]
pub struct HotboxError {
    pub message: String,
    pub status: Option<u16>,
}

impl fmt::Display for HotboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(status) = self.status {
            write!(f, "{} (HTTP {})", self.message, status)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl From<std::io::Error> for HotboxError {
    fn from(e: std::io::Error) -> Self {
        HotboxError {
            status: None,
            message: e.to_string(),
        }
    }
}

impl HotboxClient {
    /// Create a client from environment.
    ///
    /// Transport selection:
    /// - `HOTBOX_TRANSPORT=vsock` + `HOTBOX_VSOCK_PORT` -> vsock (Firecracker)
    /// - `HOTBOX_URL` -> TCP
    /// - `HOTBOX_SOCKET` -> unix socket (Docker, default)
    pub fn from_env() -> Self {
        // vsock transport for Firecracker VMs
        #[cfg(target_os = "linux")]
        if std::env::var("HOTBOX_TRANSPORT").ok().as_deref() == Some("vsock") {
            let port: u32 = std::env::var("HOTBOX_VSOCK_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9119);
            // CID 2 = VMADDR_CID_HOST (connect to the host from inside the VM)
            return Self {
                transport: Transport::Vsock(2, port),
                auth_token: std::env::var("HOTBOX_AUTH_TOKEN").ok(),
            };
        }

        if let Ok(url) = std::env::var("HOTBOX_URL")
            && let Some((host, port)) = parse_host_port(&url)
        {
            return Self {
                transport: Transport::Tcp(host, port),
                auth_token: std::env::var("HOTBOX_AUTH_TOKEN").ok(),
            };
        }

        let socket_path =
            std::env::var("HOTBOX_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());

        Self {
            transport: Transport::Unix(socket_path),
            auth_token: std::env::var("HOTBOX_AUTH_TOKEN").ok(),
        }
    }

    /// Create a client connected to a specific unix socket path.
    #[cfg(test)]
    pub fn unix(socket_path: &str) -> Self {
        Self {
            transport: Transport::Unix(socket_path.to_string()),
            auth_token: std::env::var("HOTBOX_AUTH_TOKEN").ok(),
        }
    }

    /// Read a file from Hot storage, returning its bytes.
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, HotboxError> {
        let uri = format!("/files/{}", path);
        let resp = self.request("GET", &uri, None).await?;
        if resp.status != 200 {
            return Err(HotboxError {
                status: Some(resp.status),
                message: String::from_utf8_lossy(&resp.body).to_string(),
            });
        }
        Ok(resp.body)
    }

    /// Write bytes to a file in Hot storage.
    pub async fn write_file(&self, path: &str, data: Vec<u8>) -> Result<(), HotboxError> {
        let uri = format!("/files/{}", path);
        let resp = self.request("PUT", &uri, Some(data)).await?;
        if resp.status != 200 {
            return Err(HotboxError {
                status: Some(resp.status),
                message: String::from_utf8_lossy(&resp.body).to_string(),
            });
        }
        Ok(())
    }

    /// Get file metadata without downloading the content.
    pub async fn file_info(&self, path: &str) -> Result<serde_json::Value, HotboxError> {
        let uri = format!("/files/{}", path);
        let resp = self.request("HEAD", &uri, None).await?;
        if resp.status != 200 {
            return Err(HotboxError {
                status: Some(resp.status),
                message: format!("File not found: {}", path),
            });
        }

        if let Some(meta_str) = resp.headers.get("x-file-meta") {
            let meta: serde_json::Value =
                serde_json::from_str(meta_str).unwrap_or(serde_json::Value::Null);
            Ok(meta)
        } else {
            let size: i64 = resp
                .headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let content_type = resp
                .headers
                .get("content-type")
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".to_string());

            Ok(serde_json::json!({
                "path": path,
                "size": size,
                "content-type": content_type,
            }))
        }
    }

    /// List files by prefix.
    pub async fn list_files(&self, prefix: &str) -> Result<Vec<serde_json::Value>, HotboxError> {
        let uri = format!("/files?prefix={}", prefix);
        let resp = self.request("GET", &uri, None).await?;
        if resp.status != 200 {
            return Err(HotboxError {
                status: Some(resp.status),
                message: String::from_utf8_lossy(&resp.body).to_string(),
            });
        }

        let files: Vec<serde_json::Value> =
            serde_json::from_slice(&resp.body).map_err(|e| HotboxError {
                status: None,
                message: format!("Failed to parse file list: {}", e),
            })?;

        Ok(files)
    }

    /// Send a raw HTTP/1.1 request over the configured transport.
    /// Retries connection errors with exponential backoff (up to ~10s total).
    async fn request(
        &self,
        method: &str,
        uri: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, HotboxError> {
        let delays = [100, 250, 500, 1000, 2000, 3000];
        let mut last_err = None;

        for (attempt, delay_ms) in std::iter::once(&0u64).chain(delays.iter()).enumerate() {
            if *delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
            }

            let result = self.try_request(method, uri, body.clone()).await;

            match result {
                Ok(resp) => return Ok(resp),
                Err(e) if e.status.is_some() => {
                    // HTTP-level error (got a response) — don't retry
                    return Err(e);
                }
                Err(e) => {
                    // Connection/IO error — retry
                    if attempt < delays.len() {
                        eprintln!(
                            "hotbox: connection failed (attempt {}/{}): {}",
                            attempt + 1,
                            delays.len() + 1,
                            e.message
                        );
                    }
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| HotboxError {
            status: None,
            message: "All connection attempts failed".to_string(),
        }))
    }

    /// Single attempt to connect and send an HTTP request.
    async fn try_request(
        &self,
        method: &str,
        uri: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, HotboxError> {
        match &self.transport {
            Transport::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path).await?;
                send_http(stream, method, uri, body, self.auth_token.as_deref()).await
            }
            Transport::Tcp(host, port) => {
                let stream = tokio::net::TcpStream::connect((host.as_str(), *port)).await?;
                send_http(stream, method, uri, body, self.auth_token.as_deref()).await
            }
            #[cfg(target_os = "linux")]
            Transport::Vsock(cid, port) => {
                let stream = vsock_connect(*cid, *port).await?;
                send_http(stream, method, uri, body, self.auth_token.as_deref()).await
            }
        }
    }
}

struct HttpResponse {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

/// Send an HTTP/1.1 request over any AsyncRead+AsyncWrite stream and parse the response.
async fn send_http<S>(
    stream: S,
    method: &str,
    uri: &str,
    body: Option<Vec<u8>>,
    auth_token: Option<&str>,
) -> Result<HttpResponse, HotboxError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);

    // Write request line + headers
    let content_length = body.as_ref().map_or(0, |b| b.len());
    let auth_header = auth_token
        .map(|token| format!("Authorization: Bearer {}\r\n", token))
        .unwrap_or_default();
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: hotbox\r\n{}Connection: close\r\nContent-Length: {}\r\n\r\n",
        method, uri, auth_header, content_length
    );
    write_half.write_all(request.as_bytes()).await?;

    if let Some(data) = body {
        write_half.write_all(&data).await?;
    }
    write_half.shutdown().await?;

    // Read response
    let mut reader = BufReader::new(read_half);

    // Status line: "HTTP/1.1 200 OK\r\n"
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    let status = parse_status_code(&status_line);

    // Headers
    let mut headers = std::collections::HashMap::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim().to_string();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    // Body
    let mut resp_body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut resp_body).await?;
    }

    Ok(HttpResponse {
        status,
        headers,
        body: resp_body,
    })
}

fn parse_status_code(line: &str) -> u16 {
    // "HTTP/1.1 200 OK" -> 200
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Connect to a vsock address (Linux only, for Firecracker VMs).
///
/// Uses raw socket syscalls to create an AF_VSOCK connection, then wraps
/// the fd in a tokio `UnixStream` (which works for any connected fd).
#[cfg(target_os = "linux")]
async fn vsock_connect(cid: u32, port: u32) -> Result<tokio::net::UnixStream, std::io::Error> {
    use std::os::unix::io::FromRawFd;

    // AF_VSOCK = 40 on Linux
    const AF_VSOCK: libc::c_int = 40;

    let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // struct sockaddr_vm { sa_family_t svm_family; unsigned short svm_reserved1;
    //                      unsigned int svm_port; unsigned int svm_cid; ... }
    #[repr(C)]
    struct SockaddrVm {
        svm_family: u16,
        svm_reserved1: u16,
        svm_port: u32,
        svm_cid: u32,
        svm_flags: u8,
        svm_zero: [u8; 3],
    }

    let addr = SockaddrVm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_flags: 0,
        svm_zero: [0; 3],
    };

    let ret = unsafe {
        libc::connect(
            fd,
            &addr as *const SockaddrVm as *const libc::sockaddr,
            std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    // Wrap the connected fd in a tokio UnixStream (works for any stream fd)
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(std_stream)
}

fn parse_host_port(url: &str) -> Option<(String, u16)> {
    let url = url.strip_prefix("http://").unwrap_or(url);
    let url = url.trim_end_matches('/');
    if let Some((host, port_str)) = url.split_once(':') {
        let port: u16 = port_str.parse().ok()?;
        Some((host.to_string(), port))
    } else {
        Some((url.to_string(), 80))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_code() {
        assert_eq!(parse_status_code("HTTP/1.1 200 OK\r\n"), 200);
        assert_eq!(parse_status_code("HTTP/1.1 404 Not Found\r\n"), 404);
        assert_eq!(
            parse_status_code("HTTP/1.1 500 Internal Server Error\r\n"),
            500
        );
        assert_eq!(parse_status_code("garbage"), 0);
    }

    #[test]
    fn test_parse_host_port() {
        assert_eq!(
            parse_host_port("http://localhost:9119"),
            Some(("localhost".to_string(), 9119))
        );
        assert_eq!(
            parse_host_port("http://127.0.0.1:8080/"),
            Some(("127.0.0.1".to_string(), 8080))
        );
        assert_eq!(
            parse_host_port("http://host"),
            Some(("host".to_string(), 80))
        );
    }

    #[test]
    fn test_transport_unix_default() {
        // Without env vars set, should default to unix socket
        let client = HotboxClient::unix("/tmp/test.sock");
        assert!(matches!(client.transport, Transport::Unix(ref p) if p == "/tmp/test.sock"));
    }
}
