use crate::val;
use crate::val::Val;
use futures::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

static INIT_RUSTLS: Once = Once::new();

/// Set once per process by [`configure`]: how connections authenticate, and a
/// private CA for `rediss://` servers.
static AUTH: OnceLock<RedisAuth> = OnceLock::new();

/// Where a Compute Engine VM's service account access token comes from.
pub const GCP_METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

#[derive(Default)]
struct RedisAuth {
    /// `redis.auth = "gcp-iam"`: Memorystore for Valkey's IAM authentication.
    credentials: Option<Arc<GcpIamCredentials>>,
    /// `redis.ca-cert`: PEM roots trusted instead of the system's.
    ca_cert: Option<Vec<u8>>,
}

/// Apply `redis.auth` and `redis.ca-cert` for every Redis connection this
/// process opens. `redis.auth` is `password` (the credentials in
/// `redis.uri`, the default) or `gcp-iam`, which authenticates as the VM's
/// service account with short-lived access tokens, as Memorystore for
/// Valkey's IAM authentication requires. Both settings apply to standalone
/// servers only. The first call wins.
pub fn configure(conf: &Val) -> Result<(), String> {
    let _ = AUTH.set(redis_auth(conf)?);
    Ok(())
}

fn redis_auth(conf: &Val) -> Result<RedisAuth, String> {
    let auth = conf.get_str_or_default("redis.auth", "password");
    let ca_cert = conf.get_str_or_default("redis.ca-cert", "");
    let credentials = match auth.trim() {
        "" | "password" => None,
        "gcp-iam" => Some(Arc::new(GcpIamCredentials::new(GCP_METADATA_TOKEN_URL))),
        other => {
            return Err(format!(
                "Unsupported redis.auth {other:?}; expected password or gcp-iam"
            ));
        }
    };
    let cluster = conf.get_bool_or_default("redis.cluster", false)
        || is_cluster_uri(&conf.get_str_or_default("redis.uri", ""));
    if cluster && (credentials.is_some() || !ca_cert.trim().is_empty()) {
        return Err(
            "redis.auth = gcp-iam and redis.ca-cert support standalone servers only".into(),
        );
    }
    if !ca_cert.trim().is_empty() && !ca_cert.contains("-----BEGIN CERTIFICATE-----") {
        return Err("redis.ca-cert must hold PEM certificates".into());
    }
    Ok(RedisAuth {
        credentials,
        ca_cert: Some(ca_cert.into_bytes()).filter(|pem| !pem.is_empty()),
    })
}

/// A standalone client for `uri`, trusting `redis.ca-cert` when it is set.
pub fn client(uri: &str) -> ::redis::RedisResult<::redis::Client> {
    client_trusting(uri, AUTH.get().and_then(|auth| auth.ca_cert.clone()))
}

fn client_trusting(uri: &str, ca_cert: Option<Vec<u8>>) -> ::redis::RedisResult<::redis::Client> {
    match ca_cert {
        Some(root_cert) if uri.starts_with("rediss://") => ::redis::Client::build_with_tls(
            uri,
            ::redis::TlsCertificates {
                client_tls: None,
                root_cert: Some(root_cert),
            },
        ),
        _ => ::redis::Client::open(uri),
    }
}

/// Access tokens for the VM's service account, refreshed in the background
/// before they expire. Connections authenticate as Memorystore's `default`
/// user with the current token, and re-authenticate when it changes;
/// connections already authenticated stay so after a token expires.
pub struct GcpIamCredentials {
    endpoint: String,
    tokens: watch::Receiver<Option<String>>,
    refresher: Mutex<Option<watch::Sender<Option<String>>>>,
}

impl GcpIamCredentials {
    pub fn new(endpoint: &str) -> Self {
        let (sender, tokens) = watch::channel(None);
        Self {
            endpoint: endpoint.to_string(),
            tokens,
            refresher: Mutex::new(Some(sender)),
        }
    }

    /// Start refreshing on first use, inside the caller's runtime.
    fn start(&self) {
        let sender = self
            .refresher
            .lock()
            .map(|mut slot| slot.take())
            .unwrap_or(None);
        if let (Some(sender), Ok(runtime)) = (sender, tokio::runtime::Handle::try_current()) {
            runtime.spawn(refresh_tokens(self.endpoint.clone(), sender));
        }
    }
}

/// The password for a one-off synchronous connection, such as an admin
/// command's: the current access token under `redis.auth = "gcp-iam"`,
/// otherwise none (the URI carries any password).
pub async fn one_off_password() -> Result<Option<String>, String> {
    match AUTH.get().and_then(|auth| auth.credentials.as_ref()) {
        Some(credentials) => fetch_token(&reqwest::Client::new(), &credentials.endpoint)
            .await
            .map(|(token, _)| Some(token))
            .map_err(|error| format!("Could not fetch a Google access token for Valkey: {error}")),
        None => Ok(None),
    }
}

/// When to fetch the next token: five minutes before this one expires, so a
/// new connection never starts with a token about to lapse, and at least 30
/// seconds apart.
pub fn token_refresh_after(expires_in_seconds: u64) -> Duration {
    Duration::from_secs(expires_in_seconds.saturating_sub(300).max(30))
}

async fn refresh_tokens(endpoint: String, sender: watch::Sender<Option<String>>) {
    let http = reqwest::Client::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        match fetch_token(&http, &endpoint).await {
            Ok((token, expires_in)) => {
                sender.send_replace(Some(token));
                backoff = Duration::from_secs(1);
                tokio::time::sleep(token_refresh_after(expires_in)).await;
            }
            Err(error) => {
                tracing::warn!(%error, "hot.dev: could not fetch a Google access token for Valkey");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn fetch_token(http: &reqwest::Client, endpoint: &str) -> Result<(String, u64), String> {
    let response = http
        .get(endpoint)
        .header("Metadata-Flavor", "Google")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("metadata server answered {}", response.status()));
    }
    let body: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
    let token = body["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or("metadata server returned no access_token")?;
    Ok((token.to_string(), body["expires_in"].as_u64().unwrap_or(0)))
}

/// Shares one refresher among every connection's subscription.
#[derive(Clone)]
struct SharedCredentials(Arc<GcpIamCredentials>);

impl ::redis::StreamingCredentialsProvider for SharedCredentials {
    fn subscribe(
        &self,
    ) -> Pin<Box<dyn Stream<Item = ::redis::RedisResult<::redis::BasicAuth>> + Send + 'static>>
    {
        self.0.start();
        // The current token first, then each new one.
        Box::pin(futures::stream::unfold(
            (self.0.tokens.clone(), true),
            |(mut tokens, first)| async move {
                if !first && tokens.changed().await.is_err() {
                    return None;
                }
                loop {
                    let token = tokens.borrow_and_update().clone();
                    if let Some(token) = token {
                        let credentials = ::redis::BasicAuth::new("default".to_string(), token);
                        return Some((Ok(credentials), (tokens, false)));
                    }
                    if tokens.changed().await.is_err() {
                        return None;
                    }
                }
            },
        ))
    }
}

/// Build an [`AsyncConnectionConfig`](::redis::AsyncConnectionConfig) for
/// standalone multiplexed connections.
///
/// redis-rs 1.x defaults `AsyncConnectionConfig` to a 500ms per-command
/// response timeout. Hot's standalone connections issue blocking reads
/// (`XREADGROUP ... BLOCK 5000` in the task/event queue and `XREAD BLOCK 30000`
/// in the stream subscriber), so the 500ms default aborts every blocking read
/// with `timed out` and clips any command slower than 500ms under load (XACK,
/// the infrastructure-retry EVAL, XAUTOCLAIM, the task-lease `SET NX`). A
/// clipped XACK/requeue leaves the entry in the consumer PEL; orphan reclaim
/// then re-delivers it, inflating the delivery count until it exhausts the
/// retry budget and lands in the dead-letter queue — stranding the underlying
/// task in `queued` forever.
///
/// We disable the redis-rs response timeout to match cluster mode (whose
/// builder defaults `response_timeout` to `None`). The queue, subscriber, and
/// task-lease wrappers apply operation-aware Tokio deadlines instead: short
/// commands are bounded tightly while blocking reads get enough time to reach
/// their server-side `BLOCK` timeout.
///
/// With `redis.auth = "gcp-iam"`, the config also carries the access-token
/// credentials (see [`configure`]).
pub fn standalone_async_config() -> ::redis::AsyncConnectionConfig {
    let config = ::redis::AsyncConnectionConfig::new().set_response_timeout(None);
    match AUTH.get().and_then(|auth| auth.credentials.clone()) {
        Some(credentials) => config.set_credentials_provider(SharedCredentials(credentials)),
        None => config,
    }
}

/// Whether a Redis command failure indicates the underlying connection is
/// broken and must be replaced (IO errors, dropped/reset connections, RESP
/// protocol desync, ...) rather than a per-command failure (server error
/// replies, type errors, ...).
///
/// redis 1.x `MultiplexedConnection` never reconnects on its own — only
/// `ConnectionManager` does — so after a silent drop (LB idle reaping,
/// failover, ...) every subsequent command on the same connection fails or
/// times out forever. Callers that cache connections must evict the cached
/// connection when this returns true so the next call reconnects fresh.
pub(crate) fn error_indicates_broken_connection(err: &::redis::RedisError) -> bool {
    err.is_io_error() || err.is_connection_dropped() || err.is_unrecoverable_error()
}

/// Initialize Rustls crypto provider (required for TLS connections)
/// This must be called before any TLS connections are established.
pub fn init_crypto_provider() {
    INIT_RUSTLS.call_once(|| {
        // Install aws-lc-rs as the default crypto provider for Rustls.
        // Required by clients that build TLS configs from the process
        // default (rediss://, the GCS SDK's gRPC transport): Hot enables
        // both the aws-lc-rs and ring backends, so rustls can't pick one.
        if let Err(e) = rustls::crypto::aws_lc_rs::default_provider().install_default() {
            tracing::warn!("Failed to install default Rustls crypto provider: {:?}", e);
        }
    });
}

/// Get resolved configuration for Redis settings
pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "uri": "",
        "cluster": false
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}

/// Detect if a Redis URI is for a cluster by checking for multiple endpoints
/// or explicit cluster indication
///
/// Note: AWS Elasticache Valkey Serverless uses cluster mode with a single endpoint
/// and encryption in transit (rediss://). The cluster detection needs to be explicitly
/// configured via the HOT_REDIS_CLUSTER environment variable.
pub fn is_cluster_uri(uri: &str) -> bool {
    // Check for explicit cluster scheme
    if uri.starts_with("rediss+cluster://") || uri.starts_with("redis+cluster://") {
        return true;
    }

    // Check for multiple comma-separated endpoints (explicit cluster format)
    if uri.contains(',') {
        return true;
    }

    // AWS Elasticache typically provides single endpoint even for clusters
    // User must set redis.cluster=true in config for these cases
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::redis::StreamingCredentialsProvider;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn tokens_refresh_five_minutes_before_they_expire() {
        assert_eq!(token_refresh_after(3599), Duration::from_secs(3299));
        assert_eq!(token_refresh_after(200), Duration::from_secs(30));
        assert_eq!(token_refresh_after(0), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn connections_get_the_current_token_then_each_new_one() {
        let credentials = Arc::new(GcpIamCredentials::new("http://127.0.0.1:1/unused"));
        // Take the refresher's place, so the test decides when tokens change.
        let sender = credentials.refresher.lock().unwrap().take().unwrap();
        let mut stream = SharedCredentials(Arc::clone(&credentials)).subscribe();
        sender.send_replace(Some("token-1".to_string()));
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!((first.username(), first.password()), ("default", "token-1"));
        let pending = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(pending.is_err(), "an unchanged token is not sent again");
        sender.send_replace(Some("token-2".to_string()));
        assert_eq!(stream.next().await.unwrap().unwrap().password(), "token-2");
        // A later subscriber starts from the current token.
        let mut late = SharedCredentials(credentials).subscribe();
        assert_eq!(late.next().await.unwrap().unwrap().password(), "token-2");
    }

    #[tokio::test]
    async fn tokens_come_from_the_metadata_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_lowercase();
            let body = r#"{"access_token":"ya29.token","expires_in":3599,"token_type":"Bearer"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let credentials = Arc::new(GcpIamCredentials::new(&format!("http://{address}/token")));
        let mut stream = SharedCredentials(credentials).subscribe();
        let auth = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            (auth.username(), auth.password()),
            ("default", "ya29.token")
        );
        assert!(server.await.unwrap().contains("metadata-flavor: google"));
    }

    #[test]
    fn redis_auth_settings_are_checked() {
        let standalone =
            |extra: Val| val!({"redis": {"uri": "rediss://10.0.0.5:6379"}}).merge(&extra);
        assert!(
            redis_auth(&standalone(val!({})))
                .unwrap()
                .credentials
                .is_none()
        );
        let iam = redis_auth(&standalone(val!({"redis": {"auth": "gcp-iam"}}))).unwrap();
        assert!(iam.credentials.is_some());
        assert!(redis_auth(&standalone(val!({"redis": {"auth": "kerberos"}}))).is_err());
        assert!(
            redis_auth(&standalone(
                val!({"redis": {"ca-cert": "not a certificate"}})
            ))
            .is_err()
        );
        let cluster =
            val!({"redis": {"uri": "rediss://10.0.0.5:6379", "cluster": true, "auth": "gcp-iam"}});
        assert!(
            redis_auth(&cluster)
                .err()
                .unwrap()
                .contains("standalone servers only")
        );
    }

    /// Against a live TLS server whose password is the "token", with a CA
    /// the system does not trust. Run with HOT_TEST_VALKEY_TLS_URI
    /// (rediss://host:port, no password), HOT_TEST_VALKEY_CA (a PEM file),
    /// and HOT_TEST_VALKEY_TOKEN set.
    #[tokio::test]
    #[ignore]
    async fn gcp_iam_tokens_and_a_private_ca_against_a_live_server() {
        let uri = std::env::var("HOT_TEST_VALKEY_TLS_URI").unwrap();
        let ca = std::fs::read(std::env::var("HOT_TEST_VALKEY_CA").unwrap()).unwrap();
        let token = std::env::var("HOT_TEST_VALKEY_TOKEN").unwrap();
        init_crypto_provider();
        let credentials = Arc::new(GcpIamCredentials::new("http://127.0.0.1:1/unused"));
        let sender = credentials.refresher.lock().unwrap().take().unwrap();
        sender.send_replace(Some(token));
        let config = ::redis::AsyncConnectionConfig::new()
            .set_credentials_provider(SharedCredentials(Arc::clone(&credentials)));
        let client = client_trusting(&uri, Some(ca)).unwrap();
        let mut connection = client
            .get_multiplexed_async_connection_with_config(&config)
            .await
            .unwrap();
        let pong: String = ::redis::cmd("PING")
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(pong, "PONG");
        // The system's roots do not include the private CA.
        let untrusted = client_trusting(&uri, None).unwrap();
        assert!(
            untrusted
                .get_multiplexed_async_connection_with_config(&config)
                .await
                .is_err()
        );
        // Without the token the server refuses commands.
        let mut anonymous = client_trusting(
            &uri,
            Some(std::fs::read(std::env::var("HOT_TEST_VALKEY_CA").unwrap()).unwrap()),
        )
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
        let refused: ::redis::RedisResult<String> =
            ::redis::cmd("PING").query_async(&mut anonymous).await;
        assert!(refused.is_err());
    }

    #[test]
    fn a_private_ca_applies_to_tls_connections_only() {
        let pem = b"-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n".to_vec();
        assert!(client_trusting("rediss://10.0.0.5:6379", Some(pem.clone())).is_err());
        assert!(client_trusting("redis://10.0.0.5:6379", Some(pem)).is_ok());
        assert!(client_trusting("rediss://10.0.0.5:6379", None).is_ok());
    }

    #[test]
    fn io_errors_indicate_a_broken_connection() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::TimedOut,
        ] {
            let err = ::redis::RedisError::from(std::io::Error::new(kind, "socket failure"));
            assert!(
                error_indicates_broken_connection(&err),
                "IO error {kind:?} must evict the cached connection"
            );
        }
    }

    #[test]
    fn per_command_failures_do_not_indicate_a_broken_connection() {
        let type_err = ::redis::RedisError::from((
            ::redis::ErrorKind::UnexpectedReturnType,
            "unexpected return type",
        ));
        assert!(!error_indicates_broken_connection(&type_err));

        let server_err = ::redis::RedisError::from((
            ::redis::ErrorKind::Server(::redis::ServerErrorKind::ResponseError),
            "WRONGTYPE Operation against a key holding the wrong kind of value",
        ));
        assert!(!error_indicates_broken_connection(&server_err));
    }
}
