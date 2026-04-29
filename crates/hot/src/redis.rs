use crate::val;
use crate::val::Val;
use std::sync::Once;

static INIT_RUSTLS: Once = Once::new();

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
