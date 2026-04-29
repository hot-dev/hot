pub mod access_log;
pub mod auth;
pub mod build_info;
pub mod domain_resolver;
pub mod handlers;
pub mod models;
pub mod rate_limit;
pub mod server;

use hot::db::DatabasePool;
use hot::storage::BuildStorage;
use hot::stream::StreamPubSub;
use hot::val::Val;
use std::sync::Arc;

/// Type alias for the API server state tuple (the actual state data, not the extractor)
/// Components: (database, build storage, config, optional stream pub/sub)
pub type ApiStateData = (
    Arc<DatabasePool>,
    Arc<Box<dyn BuildStorage>>,
    Arc<Val>,
    Option<Arc<StreamPubSub>>,
);
