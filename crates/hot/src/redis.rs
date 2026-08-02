use crate::val;
use crate::val::Val;
use std::sync::Once;

static INIT_RUSTLS: Once = Once::new();

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
pub fn standalone_async_config() -> ::redis::AsyncConnectionConfig {
    ::redis::AsyncConnectionConfig::new().set_response_timeout(None)
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
        // Install aws-lc-rs as the default crypto provider for Rustls
        // This is required when using rediss:// (Redis with TLS)
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
