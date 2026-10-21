//! Shared Google Cloud Storage plumbing for file, blob, and build storage.
//!
//! Runtime operations go through Google's official Rust SDK and authenticate
//! with Application Default Credentials (an attached service account on GCP,
//! Workload Identity Federation in CI or external deployments, or developer
//! ADC locally). No credentials are embedded in configuration and no `gcloud`
//! subprocess is ever spawned.
//!
//! Every request is bounded: a per-client semaphore caps concurrent requests,
//! each request carries a timeout, and the SDK retry loop is limited in both
//! attempts and elapsed time. Clients are shared per process (see
//! [`GcsClient::connect`]). The SDK only retries requests marked idempotent;
//! every request Hot issues is safe to repeat (reads, generation-pinned
//! writes and deletes, `ifGenerationMatch=0` creates, or keys that no other
//! writer can share), so all of them are marked.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use google_cloud_auth::credentials::{Builder as CredentialsBuilder, Credentials};
use google_cloud_gax::error::rpc::Code;
use google_cloud_gax::exponential_backoff::ExponentialBackoffBuilder;
use google_cloud_gax::options::RequestOptionsBuilder;
use google_cloud_gax::retry_policy::RetryPolicyExt;
use google_cloud_storage::client::{Storage, StorageControl};
use google_cloud_storage::model::Object;
use google_cloud_storage::model::compose_object_request::SourceObject;
use google_cloud_storage::model::compose_object_request::source_object::ObjectPreconditions;
use google_cloud_storage::retry_policy::RetryableErrors;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::Instant;

use crate::val::Val;

/// Storage backend identifier persisted in `file.storage_backend`,
/// `blob_object.storage_backend`, `file_upload.storage_backend`, and
/// `build.storage_backend`.
pub const BACKEND_ID: &str = "gcs";

/// Canonical URI scheme for persisted storage locations.
pub const URI_SCHEME: &str = "gs://";

/// Maximum number of source objects accepted by one compose request.
pub const MAX_COMPOSE_SOURCES: usize = 32;

const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 32;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 300;
const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_RETRY_TIME_LIMIT_SECS: u64 = 120;
/// Chunks buffered between the GCS download task and a streaming reader.
const STREAM_CHANNEL_CHUNKS: usize = 4;

// ============================================================================
// Errors
// ============================================================================

/// Classified GCS failure. Messages never include credentials and only name
/// the operation and bucket; object keys are left to debug-level tracing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcsError {
    /// The object (or bucket) does not exist.
    NotFound(String),
    /// Authentication failed or the identity lacks permission on the bucket.
    AccessDenied(String),
    /// A generation precondition did not hold (a concurrent writer won).
    PreconditionFailed(String),
    /// Throttling, timeouts, or server errors that survived the retry loop.
    Transient(String),
    /// Missing or invalid configuration.
    InvalidConfig(String),
    /// Anything else.
    Other(String),
}

impl GcsError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, GcsError::NotFound(_))
    }

    pub fn is_precondition_failed(&self) -> bool {
        matches!(self, GcsError::PreconditionFailed(_))
    }

    /// Classify an SDK error for `operation` (e.g. "read object").
    pub fn from_sdk(operation: &str, err: &google_cloud_storage::Error) -> Self {
        let detail = format!("GCS {operation} failed: {err}");
        if err.is_authentication() {
            return GcsError::AccessDenied(detail);
        }
        if err.is_timeout() || err.is_exhausted() || err.is_connect() || err.is_io() {
            return GcsError::Transient(detail);
        }
        if let Some(status) = err.status() {
            return match status.code {
                Code::NotFound => GcsError::NotFound(detail),
                Code::PermissionDenied | Code::Unauthenticated => GcsError::AccessDenied(detail),
                Code::FailedPrecondition | Code::Aborted => GcsError::PreconditionFailed(detail),
                Code::Unavailable
                | Code::ResourceExhausted
                | Code::DeadlineExceeded
                | Code::Internal => GcsError::Transient(detail),
                Code::InvalidArgument => GcsError::Other(detail),
                _ => match err.http_status_code() {
                    Some(code) => Self::from_http_status(code, detail),
                    None => GcsError::Other(detail),
                },
            };
        }
        match err.http_status_code() {
            Some(code) => Self::from_http_status(code, detail),
            None => GcsError::Other(detail),
        }
    }

    fn from_http_status(code: u16, detail: String) -> Self {
        match code {
            404 => GcsError::NotFound(detail),
            401 | 403 => GcsError::AccessDenied(detail),
            409 | 412 => GcsError::PreconditionFailed(detail),
            408 | 429 | 500..=599 => GcsError::Transient(detail),
            _ => GcsError::Other(detail),
        }
    }
}

impl std::fmt::Display for GcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GcsError::NotFound(m) => write!(f, "not found: {m}"),
            GcsError::AccessDenied(m) => write!(f, "access denied: {m}"),
            GcsError::PreconditionFailed(m) => write!(f, "precondition failed: {m}"),
            GcsError::Transient(m) => write!(f, "transient error: {m}"),
            GcsError::InvalidConfig(m) => write!(f, "invalid GCS configuration: {m}"),
            GcsError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for GcsError {}

impl From<GcsError> for String {
    fn from(e: GcsError) -> Self {
        e.to_string()
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for one GCS-backed storage role (`file` or `build`).
///
/// Precedence for each setting: the `HOT_<ROLE>_GCS_*` environment variable,
/// then `<role>.gcs.*` in Hot configuration, then the default.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GcsConfig {
    pub bucket: String,
    /// Object-name prefix without leading or trailing slashes.
    pub prefix: Option<String>,
    /// Optional API endpoint override (Private Service Connect, restricted
    /// VIP, or a test server). Defaults to the SDK's global endpoint.
    pub endpoint: Option<String>,
    pub max_concurrent_requests: usize,
    pub request_timeout: Duration,
    pub retry_attempts: u32,
    pub retry_time_limit: Duration,
}

impl GcsConfig {
    /// Resolve the configuration for `role` ("file" or "build").
    pub fn from_conf(conf: &Val, role: &str) -> Result<Self, GcsError> {
        Self::resolve(conf, role, |name| std::env::var(name).ok())
    }

    /// Resolution with an injectable environment lookup (for tests).
    pub fn resolve(
        conf: &Val,
        role: &str,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, GcsError> {
        let setting = |name: &str| -> Option<String> {
            let env_name = format!(
                "HOT_{}_GCS_{}",
                role.to_ascii_uppercase(),
                name.to_ascii_uppercase().replace('-', "_")
            );
            env(&env_name)
                .or_else(|| {
                    let value = conf.get_str_or_default(&format!("{role}.gcs.{name}"), "");
                    Some(value)
                })
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty() && v != "null")
        };
        let positive = |name: &str, default: u64| -> Result<u64, GcsError> {
            match setting(name) {
                None => Ok(default),
                Some(raw) => match raw.parse::<u64>() {
                    Ok(v) if v > 0 => Ok(v),
                    _ => Err(GcsError::InvalidConfig(format!(
                        "{role}.gcs.{name} must be a positive integer, got '{raw}'"
                    ))),
                },
            }
        };

        let bucket = setting("bucket").ok_or_else(|| {
            GcsError::InvalidConfig(format!(
                "{role}.gcs.bucket (HOT_{}_GCS_BUCKET) is required when {role} storage type is gcs",
                role.to_ascii_uppercase()
            ))
        })?;
        validate_bucket_name(&bucket).map_err(|e| {
            GcsError::InvalidConfig(format!("{role}.gcs.bucket is not a valid bucket name: {e}"))
        })?;

        let prefix = setting("prefix")
            .map(|p| p.trim_matches('/').to_string())
            .filter(|p| !p.is_empty());
        if let Some(prefix) = &prefix
            && prefix
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Err(GcsError::InvalidConfig(format!(
                "{role}.gcs.prefix must not contain empty, '.', or '..' segments"
            )));
        }

        Ok(Self {
            bucket,
            prefix,
            endpoint: setting("endpoint"),
            max_concurrent_requests: positive(
                "max-concurrent-requests",
                DEFAULT_MAX_CONCURRENT_REQUESTS as u64,
            )? as usize,
            request_timeout: Duration::from_secs(positive(
                "request-timeout-secs",
                DEFAULT_REQUEST_TIMEOUT_SECS,
            )?),
            retry_attempts: positive("retry-attempts", DEFAULT_RETRY_ATTEMPTS as u64)? as u32,
            retry_time_limit: Duration::from_secs(positive(
                "retry-time-limit-secs",
                DEFAULT_RETRY_TIME_LIMIT_SECS,
            )?),
        })
    }
}

/// Basic GCS bucket-name validation (3-63 chars per dot-separated name of at
/// most 222 chars; lowercase letters, digits, `-`, `_`, `.`).
pub fn validate_bucket_name(bucket: &str) -> Result<(), String> {
    if bucket.len() < 3 || bucket.len() > 222 {
        return Err("length must be between 3 and 222 characters".to_string());
    }
    if !bucket
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    {
        return Err(
            "only lowercase letters, digits, '-', '_' and '.' are allowed (no gs:// scheme)"
                .to_string(),
        );
    }
    let first = bucket.chars().next().unwrap_or('-');
    let last = bucket.chars().last().unwrap_or('-');
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return Err("must start and end with a letter or digit".to_string());
    }
    Ok(())
}

/// `gs://bucket/key`.
pub fn gs_uri(bucket: &str, key: &str) -> String {
    format!("{URI_SCHEME}{bucket}/{key}")
}

/// Parse a canonical `gs://bucket/key` location.
pub fn parse_gs_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix(URI_SCHEME)?;
    let (bucket, key) = rest.split_once('/')?;
    if bucket.is_empty() || key.is_empty() {
        return None;
    }
    Some((bucket.to_string(), key.to_string()))
}

/// Join an optional prefix and object-relative parts with `/`.
pub fn join_key<'a>(prefix: Option<&str>, parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut segments: Vec<&str> = Vec::new();
    if let Some(prefix) = prefix {
        segments.push(prefix.trim_matches('/'));
    }
    segments.extend(parts.into_iter().map(|p| p.trim_matches('/')));
    segments.retain(|p| !p.is_empty());
    segments.join("/")
}

// ============================================================================
// Client
// ============================================================================

/// A bucket-scoped GCS client pair (data plane + control plane) with bounded
/// concurrency, timeouts, and retries.
#[derive(Clone)]
pub struct GcsClient {
    storage: Storage,
    control: StorageControl,
    bucket: String,
    bucket_resource: String,
    prefix: Option<String>,
    limiter: Arc<Semaphore>,
    timeout: Duration,
}

impl GcsClient {
    /// The process-wide client for `config`, authenticated with Application
    /// Default Credentials.
    ///
    /// Storage is resolved per task and per request, so clients are built
    /// once and shared: each one owns a token cache (whose refresh task
    /// outlives a dropped client), connection pools, and the concurrency
    /// limit, which has to bound the process rather than each caller.
    pub async fn connect(config: &GcsConfig) -> Result<Self, GcsError> {
        static CLIENTS: OnceLock<tokio::sync::Mutex<HashMap<GcsConfig, GcsClient>>> =
            OnceLock::new();
        let mut clients = CLIENTS.get_or_init(Default::default).lock().await;
        if let Some(client) = clients.get(config) {
            return Ok(client.clone());
        }
        let credentials = CredentialsBuilder::default().build().map_err(|e| {
            GcsError::InvalidConfig(format!(
                "failed to initialize GCS client (Application Default Credentials): {e}"
            ))
        })?;
        let client = Self::connect_with_credentials(config, credentials).await?;
        clients.insert(config.clone(), client.clone());
        Ok(client)
    }

    /// Build an unshared client. The data-plane and control-plane clients
    /// share `credentials`, so they share one token cache.
    pub async fn connect_with_credentials(
        config: &GcsConfig,
        credentials: Credentials,
    ) -> Result<Self, GcsError> {
        // The SDK's gRPC transport builds its TLS config from rustls's
        // process-default crypto provider. Hot enables both rustls backends
        // (aws-lc-rs directly, ring through ureq), so rustls cannot pick one
        // and panics unless a default is installed first.
        crate::redis::init_crypto_provider();

        let retry = || {
            RetryableErrors
                .with_attempt_limit(config.retry_attempts)
                .with_time_limit(config.retry_time_limit)
        };
        let backoff = || {
            ExponentialBackoffBuilder::new()
                .with_initial_delay(Duration::from_millis(200))
                .with_maximum_delay(Duration::from_secs(10))
                .clamp()
        };

        let mut storage_builder = Storage::builder()
            .with_credentials(credentials.clone())
            .with_retry_policy(retry())
            .with_backoff_policy(backoff());
        let mut control_builder = StorageControl::builder()
            .with_credentials(credentials)
            .with_retry_policy(retry())
            .with_backoff_policy(backoff());
        if let Some(endpoint) = &config.endpoint {
            storage_builder = storage_builder.with_endpoint(endpoint.clone());
            control_builder = control_builder.with_endpoint(endpoint.clone());
        }
        let storage = storage_builder.build().await.map_err(|e| {
            GcsError::InvalidConfig(format!("failed to initialize GCS client: {e}"))
        })?;
        let control = control_builder.build().await.map_err(|e| {
            GcsError::InvalidConfig(format!("failed to initialize GCS control client: {e}"))
        })?;

        Ok(Self {
            storage,
            control,
            bucket: config.bucket.clone(),
            bucket_resource: format!("projects/_/buckets/{}", config.bucket),
            prefix: config.prefix.clone(),
            limiter: Arc::new(Semaphore::new(config.max_concurrent_requests)),
            timeout: config.request_timeout,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// Object key under this client's prefix.
    pub fn key<'a>(&self, parts: impl IntoIterator<Item = &'a str>) -> String {
        join_key(self.prefix.as_deref(), parts)
    }

    /// Canonical location for a key in this bucket.
    pub fn uri(&self, key: &str) -> String {
        gs_uri(&self.bucket, key)
    }

    /// Resolve a persisted `gs://` location to a key in *this* bucket.
    /// Locations in another bucket are rejected rather than silently read
    /// with this installation's credentials.
    pub fn key_from_uri(&self, uri: &str) -> Result<String, GcsError> {
        match parse_gs_uri(uri) {
            Some((bucket, key)) if bucket == self.bucket => Ok(key),
            Some((bucket, _)) => Err(GcsError::InvalidConfig(format!(
                "stored location is in bucket '{bucket}', but this installation is configured for '{}'",
                self.bucket
            ))),
            None => Err(GcsError::Other(
                "stored location is not a gs:// URI".to_string(),
            )),
        }
    }

    async fn bounded<T, F>(&self, operation: &str, fut: F) -> Result<T, GcsError>
    where
        F: std::future::Future<Output = google_cloud_storage::Result<T>>,
    {
        let _permit = self
            .limiter
            .acquire()
            .await
            .map_err(|_| GcsError::Other("GCS client is shut down".to_string()))?;
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(GcsError::from_sdk(operation, &e)),
            Err(_) => Err(GcsError::Transient(format!(
                "GCS {operation} timed out after {}s",
                self.timeout.as_secs()
            ))),
        }
    }

    /// Upload `data` to `key`.
    ///
    /// `if_generation_match`: `Some(0)` creates only if absent, `Some(g)`
    /// replaces only generation `g`, `None` overwrites unconditionally (only
    /// safe for keys no other writer can share).
    pub async fn put_bytes(
        &self,
        key: &str,
        data: Bytes,
        content_type: Option<&str>,
        if_generation_match: Option<i64>,
        metadata: &[(&str, &str)],
    ) -> Result<Object, GcsError> {
        tracing::debug!(bucket = %self.bucket, key, size = data.len(), "GCS put object");
        let mut req = self
            .storage
            .write_object(&self.bucket_resource, key, data)
            .with_idempotency(true);
        if let Some(ct) = content_type {
            req = req.set_content_type(ct);
        }
        if let Some(generation) = if_generation_match {
            req = req.set_if_generation_match(generation);
        }
        if !metadata.is_empty() {
            req = req.set_metadata(metadata.iter().map(|(k, v)| (*k, *v)));
        }
        self.bounded("write object", req.send_unbuffered()).await
    }

    /// Stream a local file to `key` without reading it into memory.
    pub async fn put_file(
        &self,
        key: &str,
        path: &std::path::Path,
        content_type: Option<&str>,
        if_generation_match: Option<i64>,
    ) -> Result<Object, GcsError> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| GcsError::Other(format!("failed to open staged file: {e}")))?;
        tracing::debug!(bucket = %self.bucket, key, "GCS put object from file");
        let mut req = self
            .storage
            .write_object(&self.bucket_resource, key, file)
            .with_idempotency(true);
        if let Some(ct) = content_type {
            req = req.set_content_type(ct);
        }
        if let Some(generation) = if_generation_match {
            req = req.set_if_generation_match(generation);
        }
        self.bounded("write object", req.send_unbuffered()).await
    }

    /// Read a whole object (optionally a specific generation).
    pub async fn get_bytes(&self, key: &str, generation: Option<i64>) -> Result<Vec<u8>, GcsError> {
        let mut reader = self.open_read(key, generation).await?;
        let mut out =
            Vec::with_capacity(usize::try_from(reader.response.object().size).unwrap_or(0));
        let body = async {
            while let Some(chunk) = reader.response.next().await {
                let chunk: Bytes = chunk?;
                out.extend_from_slice(&chunk);
            }
            Ok::<_, google_cloud_storage::Error>(())
        };
        match tokio::time::timeout_at(reader.deadline, body).await {
            Ok(Ok(())) => Ok(out),
            Ok(Err(e)) => Err(GcsError::from_sdk("read object", &e)),
            Err(_) => Err(GcsError::Transient(format!(
                "GCS read object timed out after {}s",
                self.timeout.as_secs()
            ))),
        }
    }

    async fn open_read(&self, key: &str, generation: Option<i64>) -> Result<ActiveRead, GcsError> {
        tracing::debug!(bucket = %self.bucket, key, "GCS read object");
        let mut req = self.storage.read_object(&self.bucket_resource, key);
        if let Some(generation) = generation {
            req = req.set_generation(generation);
        }
        let permit = self
            .limiter
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| GcsError::Other("GCS client is shut down".to_string()))?;
        let deadline = Instant::now() + self.timeout;
        let response = tokio::time::timeout_at(deadline, req.send())
            .await
            .map_err(|_| GcsError::Transient("GCS read object timed out".to_string()))?
            .map_err(|e| GcsError::from_sdk("read object", &e))?;
        Ok(ActiveRead {
            response,
            deadline,
            permit,
        })
    }

    /// Open a streaming reader. At most a few chunks are buffered ahead of
    /// the consumer; dropping the reader cancels the download.
    pub async fn open_reader(
        &self,
        key: &str,
    ) -> Result<(Pin<Box<dyn AsyncRead + Send>>, i64), GcsError> {
        let read = self.open_read(key, None).await?;
        let size = read.response.object().size;
        Ok((read.into_reader(), size))
    }

    /// Object metadata, or `None` when it does not exist.
    pub async fn head(&self, key: &str) -> Result<Option<Object>, GcsError> {
        let req = self
            .control
            .get_object()
            .set_bucket(&self.bucket_resource)
            .set_object(key)
            .with_idempotency(true);
        match self.bounded("get object metadata", req.send()).await {
            Ok(obj) => Ok(Some(obj)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete `key`. With `generation`, only that exact generation is
    /// deleted, so a concurrent replacement is never removed. Returns false
    /// when nothing matched (already gone).
    pub async fn delete(&self, key: &str, generation: Option<i64>) -> Result<bool, GcsError> {
        tracing::debug!(bucket = %self.bucket, key, ?generation, "GCS delete object");
        // Repeating a delete is safe: a retry after a lost response sees
        // NotFound, which is reported as "already gone".
        let mut req = self
            .control
            .delete_object()
            .set_bucket(&self.bucket_resource)
            .set_object(key)
            .with_idempotency(true);
        if let Some(generation) = generation {
            req = req.set_if_generation_match(generation);
        }
        match self.bounded("delete object", req.send()).await {
            Ok(()) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            // The generation we meant to delete was already replaced; the
            // newer object is not ours to remove.
            Err(e) if generation.is_some() && e.is_precondition_failed() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Compose up to [`MAX_COMPOSE_SOURCES`] generation-pinned sources into
    /// `dest`. Each source carries an `ifGenerationMatch` precondition, so a
    /// concurrent replacement of a source fails the compose instead of
    /// changing the bytes being finalized.
    pub async fn compose(
        &self,
        dest: &str,
        sources: &[(String, i64)],
        content_type: Option<&str>,
        if_generation_match: Option<i64>,
        metadata: &[(&str, &str)],
    ) -> Result<Object, GcsError> {
        if sources.is_empty() || sources.len() > MAX_COMPOSE_SOURCES {
            return Err(GcsError::Other(format!(
                "compose needs 1..={MAX_COMPOSE_SOURCES} sources, got {}",
                sources.len()
            )));
        }
        let mut destination = Object::new()
            .set_bucket(&self.bucket_resource)
            .set_name(dest);
        if let Some(ct) = content_type {
            destination = destination.set_content_type(ct);
        }
        if !metadata.is_empty() {
            destination = destination.set_metadata(metadata.iter().map(|(k, v)| (*k, *v)));
        }
        let source_objects = sources.iter().map(|(name, generation)| {
            SourceObject::new()
                .set_name(name)
                .set_generation(*generation)
                .set_object_preconditions(
                    ObjectPreconditions::new().set_if_generation_match(*generation),
                )
        });
        // Sources are generation-pinned and every caller guards the
        // destination with a generation precondition, so a retry after a
        // lost response fails the precondition rather than composing twice.
        let mut req = self
            .control
            .compose_object()
            .set_destination(destination)
            .set_source_objects(source_objects)
            .with_idempotency(true);
        if let Some(generation) = if_generation_match {
            req = req.set_if_generation_match(generation);
        }
        tracing::debug!(bucket = %self.bucket, dest, sources = sources.len(), "GCS compose");
        self.bounded("compose objects", req.send()).await
    }

    /// List `(name, generation)` for every live object under `prefix`.
    /// Only used for cleaning up Hot-owned staging objects; never for
    /// authorization or user-visible listings.
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<(String, i64)>, GcsError> {
        let mut out = Vec::new();
        let mut page_token = String::new();
        loop {
            let req = self
                .control
                .list_objects()
                .set_parent(&self.bucket_resource)
                .set_prefix(prefix)
                .set_page_size(1000)
                .set_page_token(page_token.clone())
                .with_idempotency(true);
            let page = self.bounded("list objects", req.send()).await?;
            out.extend(page.objects.into_iter().map(|o| (o.name, o.generation)));
            if page.next_page_token.is_empty() {
                return Ok(out);
            }
            page_token = page.next_page_token;
        }
    }

    /// Best-effort delete of every object under `prefix` (generation
    /// pinned). Returns the number of failures.
    ///
    /// A large upload stages thousands of objects. Only a few deletes run at
    /// once so cleanup never queues ahead of every other request on the
    /// shared semaphore.
    pub async fn delete_prefix(&self, prefix: &str) -> Result<usize, GcsError> {
        use futures::StreamExt;
        const CLEANUP_CONCURRENCY: usize = 8;

        let objects = self.list_prefix(prefix).await?;
        let results: Vec<_> = futures::stream::iter(objects)
            .map(|(name, generation)| async move { self.delete(&name, Some(generation)).await })
            .buffer_unordered(CLEANUP_CONCURRENCY)
            .collect()
            .await;
        let failures = results.iter().filter(|r| r.is_err()).count();
        if failures > 0 {
            tracing::warn!(
                bucket = %self.bucket,
                failures,
                "GCS staging cleanup left objects behind; the bucket lifecycle rule will remove them"
            );
        }
        Ok(failures)
    }
}

/// Plan a staged composition: group `count` sources into levels of at most
/// [`MAX_COMPOSE_SOURCES`]. Returns the group sizes per intermediate level;
/// the final compose (not included) then takes at most 32 sources.
pub fn plan_compose_levels(count: usize) -> Vec<Vec<usize>> {
    let mut levels = Vec::new();
    let mut remaining = count;
    while remaining > MAX_COMPOSE_SOURCES {
        let groups: Vec<usize> = (0..remaining)
            .step_by(MAX_COMPOSE_SOURCES)
            .map(|start| MAX_COMPOSE_SOURCES.min(remaining - start))
            .collect();
        remaining = groups.len();
        levels.push(groups);
    }
    levels
}

// ============================================================================
// Streaming reader
// ============================================================================

/// A request remains active through its body, including backpressure. The
/// deadline covers headers and body together. Dropping a streaming reader
/// also aborts its producer while it is waiting on the network.
struct ActiveRead {
    response: google_cloud_storage::read_object::ReadObjectResponse,
    deadline: Instant,
    permit: OwnedSemaphorePermit,
}

impl ActiveRead {
    fn into_reader(self) -> Pin<Box<dyn AsyncRead + Send>> {
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CHUNKS);
        let Self {
            mut response,
            deadline,
            permit,
        } = self;
        let task = tokio::spawn(async move {
            let transfer = async {
                while let Some(chunk) = response.next().await {
                    let item = chunk.map_err(|e| {
                        std::io::Error::other(GcsError::from_sdk("read object", &e).to_string())
                    });
                    let failed = item.is_err();
                    if tx.send(item).await.is_err() || failed {
                        break;
                    }
                }
            };
            let timed_out = tokio::select! {
                _ = tx.closed() => false,
                result = tokio::time::timeout_at(deadline, transfer) => result.is_err(),
            };
            // A slow consumer must not retain the permit while we queue the
            // timeout error behind already-buffered chunks.
            drop(response);
            drop(permit);
            if timed_out {
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "GCS read object timed out",
                    )))
                    .await;
            }
        });
        let mut reader = ChannelReader::new(rx);
        reader.producer = Some(task);
        Box::pin(reader)
    }
}

/// `AsyncRead` over chunks produced by a download task. Errors from the
/// download surface as read errors, never as a silently truncated body.
struct ChannelReader {
    rx: mpsc::Receiver<std::io::Result<Bytes>>,
    current: Bytes,
    producer: Option<tokio::task::JoinHandle<()>>,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<std::io::Result<Bytes>>) -> Self {
        Self {
            rx,
            current: Bytes::new(),
            producer: None,
        }
    }
}

impl Drop for ChannelReader {
    fn drop(&mut self) {
        if let Some(producer) = &self.producer {
            producer.abort();
        }
    }
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.current.is_empty() {
                let n = self.current.len().min(buf.remaining());
                let chunk = self.current.split_to(n);
                buf.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => self.current = chunk,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real SDK response whose body blocks until explicitly released.
    struct PausedSource(Arc<tokio::sync::Notify>);

    struct BurstSource(std::vec::IntoIter<Bytes>);

    impl google_cloud_storage::streaming_source::StreamingSource for BurstSource {
        type Error = std::io::Error;
        async fn next(&mut self) -> Option<Result<Bytes, Self::Error>> {
            self.0.next().map(Ok)
        }
    }

    impl google_cloud_storage::streaming_source::StreamingSource for PausedSource {
        type Error = std::io::Error;
        async fn next(&mut self) -> Option<Result<Bytes, Self::Error>> {
            self.0.notified().await;
            None
        }
    }

    async fn paused_read(limiter: &Arc<Semaphore>, timeout: Duration) -> ActiveRead {
        let source = PausedSource(Arc::new(tokio::sync::Notify::new()));
        ActiveRead {
            response: google_cloud_storage::read_object::ReadObjectResponse::from_source(
                Default::default(),
                source,
            ),
            permit: limiter.clone().acquire_owned().await.unwrap(),
            deadline: Instant::now() + timeout,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn streaming_body_holds_permit_until_timeout_and_reports_error() {
        use tokio::io::AsyncReadExt;
        let limiter = Arc::new(Semaphore::new(1));
        let mut reader = paused_read(&limiter, Duration::from_secs(5))
            .await
            .into_reader();
        tokio::task::yield_now().await;
        assert!(
            limiter.clone().try_acquire_owned().is_err(),
            "body escaped its concurrency limit"
        );
        tokio::time::advance(Duration::from_secs(6)).await;
        let error = reader.read_to_end(&mut Vec::new()).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(limiter.clone().try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn dropping_reader_cancels_a_stalled_body_and_releases_permit() {
        let limiter = Arc::new(Semaphore::new(1));
        let reader = paused_read(&limiter, Duration::from_secs(300))
            .await
            .into_reader();
        tokio::task::yield_now().await;
        assert_eq!(limiter.available_permits(), 0);
        drop(reader);
        let permit = tokio::time::timeout(Duration::from_secs(1), limiter.acquire())
            .await
            .expect("dropping the reader must cancel even a stalled network read")
            .unwrap();
        drop(permit);
    }

    #[tokio::test(start_paused = true)]
    async fn backpressured_download_times_out_without_holding_the_permit() {
        use tokio::io::AsyncReadExt;
        let limiter = Arc::new(Semaphore::new(1));
        let chunks = vec![Bytes::from_static(b"x"); STREAM_CHANNEL_CHUNKS + 2];
        let read = ActiveRead {
            response: google_cloud_storage::read_object::ReadObjectResponse::from_source(
                Default::default(),
                BurstSource(chunks.into_iter()),
            ),
            permit: limiter.clone().acquire_owned().await.unwrap(),
            deadline: Instant::now() + Duration::from_secs(5),
        };
        let mut reader = read.into_reader();
        tokio::task::yield_now().await;
        assert_eq!(limiter.available_permits(), 0);
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            limiter.available_permits(),
            1,
            "slow consumers must release the expired request"
        );
        let mut received = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut received).await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(received.len(), STREAM_CHANNEL_CHUNKS);
    }
    use std::collections::HashMap;
    use tokio::io::AsyncReadExt;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn config_env_overrides_conf() {
        let conf = crate::val!({"file": {"gcs": {"bucket": "conf-bucket", "prefix": "conf"}}});
        let cfg = GcsConfig::resolve(
            &conf,
            "file",
            env_from(&[("HOT_FILE_GCS_BUCKET", "env-bucket")]),
        )
        .unwrap();
        assert_eq!(cfg.bucket, "env-bucket");
        assert_eq!(cfg.prefix.as_deref(), Some("conf"));
        assert_eq!(cfg.max_concurrent_requests, DEFAULT_MAX_CONCURRENT_REQUESTS);
    }

    #[test]
    fn config_roles_are_independent() {
        let conf = crate::val!({
            "file": {"gcs": {"bucket": "shared-bucket", "prefix": "/files/"}},
            "build": {"gcs": {"bucket": "shared-bucket", "prefix": "builds"}}
        });
        let file = GcsConfig::resolve(&conf, "file", env_from(&[])).unwrap();
        let build = GcsConfig::resolve(&conf, "build", env_from(&[])).unwrap();
        assert_eq!(file.prefix.as_deref(), Some("files"));
        assert_eq!(build.prefix.as_deref(), Some("builds"));
        assert_eq!(file.bucket, build.bucket);
    }

    #[test]
    fn config_rejects_missing_and_invalid_values() {
        let empty = crate::val!({});
        assert!(matches!(
            GcsConfig::resolve(&empty, "file", env_from(&[])),
            Err(GcsError::InvalidConfig(_))
        ));
        assert!(
            GcsConfig::resolve(
                &empty,
                "file",
                env_from(&[("HOT_FILE_GCS_BUCKET", "gs://my-bucket")])
            )
            .is_err()
        );
        assert!(
            GcsConfig::resolve(
                &empty,
                "build",
                env_from(&[
                    ("HOT_BUILD_GCS_BUCKET", "ok-bucket"),
                    ("HOT_BUILD_GCS_PREFIX", "a/../b")
                ])
            )
            .is_err()
        );
        assert!(
            GcsConfig::resolve(
                &empty,
                "file",
                env_from(&[
                    ("HOT_FILE_GCS_BUCKET", "ok-bucket"),
                    ("HOT_FILE_GCS_MAX_CONCURRENT_REQUESTS", "0")
                ])
            )
            .is_err()
        );
    }

    #[test]
    fn uri_round_trip() {
        assert_eq!(gs_uri("b", "p/k"), "gs://b/p/k");
        assert_eq!(
            parse_gs_uri("gs://b/p/k"),
            Some(("b".to_string(), "p/k".to_string()))
        );
        assert_eq!(parse_gs_uri("s3://b/k"), None);
        assert_eq!(parse_gs_uri("gs://b"), None);
        assert_eq!(parse_gs_uri("gs:///k"), None);
    }

    #[test]
    fn keys_join_prefix() {
        assert_eq!(
            join_key(Some("files"), ["org", "env", "a/b"]),
            "files/org/env/a/b"
        );
        assert_eq!(join_key(None, ["org", "x"]), "org/x");
    }

    #[test]
    fn compose_plan_stays_within_limits() {
        assert!(plan_compose_levels(1).is_empty());
        assert!(plan_compose_levels(32).is_empty());
        assert_eq!(plan_compose_levels(33), vec![vec![32, 1]]);
        let plan = plan_compose_levels(10_000);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].iter().sum::<usize>(), 10_000);
        assert_eq!(plan[1].iter().sum::<usize>(), plan[0].len());
        assert!(plan.iter().flatten().all(|g| *g <= MAX_COMPOSE_SOURCES));
        assert!(plan.last().unwrap().len() <= MAX_COMPOSE_SOURCES);
    }

    #[test]
    fn http_status_classification() {
        assert!(GcsError::from_http_status(404, String::new()).is_not_found());
        assert!(GcsError::from_http_status(412, String::new()).is_precondition_failed());
        assert!(matches!(
            GcsError::from_http_status(403, String::new()),
            GcsError::AccessDenied(_)
        ));
        assert!(matches!(
            GcsError::from_http_status(503, String::new()),
            GcsError::Transient(_)
        ));
    }

    #[tokio::test]
    async fn channel_reader_propagates_errors() {
        let (tx, rx) = mpsc::channel(4);
        tx.send(Ok(Bytes::from_static(b"hello "))).await.unwrap();
        tx.send(Ok(Bytes::from_static(b"world"))).await.unwrap();
        drop(tx);
        let mut out = String::new();
        ChannelReader::new(rx)
            .read_to_string(&mut out)
            .await
            .unwrap();
        assert_eq!(out, "hello world");

        let (tx, rx) = mpsc::channel(4);
        tx.send(Ok(Bytes::from_static(b"partial"))).await.unwrap();
        tx.send(Err(std::io::Error::other("boom"))).await.unwrap();
        drop(tx);
        let mut out = Vec::new();
        assert!(ChannelReader::new(rx).read_to_end(&mut out).await.is_err());
    }
}
