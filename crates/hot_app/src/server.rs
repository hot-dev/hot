use crate::routes;
use crate::templates;
use axum::http::header;
use hot::db::{check_default_data_exists, create_db_pool, insert_default_data, run_migrations};
use hot::log;
use hot::stream::{StreamPubSub, StreamPubSubType};
use hot::val;
use hot::val::Val;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::watch;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::{self, TraceLayer};
use tracing::{debug, error, info, warn};

pub const DEFAULT_APP_HOST: &str = "localhost";
pub const DEFAULT_APP_PORT: u16 = 4680;
pub const ASSETS_URL_PREFIX: &str = "/assets/";

pub fn get_resolved_conf(conf: Val) -> Val {
    // Create app defaults
    let app_defaults = val!({
        "host": DEFAULT_APP_HOST,
        "port": DEFAULT_APP_PORT as i64
    });

    // Extract app-specific configuration from the full config
    let app_section = conf.get("app").unwrap_or(Val::map_empty());

    // Merge defaults with app-specific config (config overrides defaults)
    app_defaults.merge(&app_section)
}

pub async fn run(conf: Val) {
    run_with_stream_pubsub(conf, None).await
}

/// Run the APP server with an optional pre-created stream pub/sub instance.
/// This is used in `hot dev` mode to share the same pub/sub instance with the worker,
/// enabling real-time SSE updates when using in-memory pub/sub.
pub async fn run_with_stream_pubsub(conf: Val, shared_stream_pubsub: Option<Arc<StreamPubSub>>) {
    tracing::info!("Hot environment: {}", hot::env::get_env());

    // Validate session secret in production environments
    if !hot::env::is_local_dev() {
        match std::env::var("HOT_APP_SESSION_SECRET") {
            Ok(secret) if !secret.is_empty() => {
                info!("hot.dev: APP session secret configured");
            }
            _ => {
                error!(
                    "hot.dev: APP startup failed - HOT_APP_SESSION_SECRET environment variable is required in production"
                );
                panic!("HOT_APP_SESSION_SECRET environment variable is required in production");
            }
        }
    } else {
        match std::env::var("HOT_APP_SESSION_SECRET") {
            Ok(secret) if !secret.is_empty() => {
                info!("hot.dev: APP using configured session secret");
            }
            _ => {
                debug!(
                    "hot.dev: APP using fallback session secret (development mode only - set HOT_APP_SESSION_SECRET for production)"
                );
            }
        }
    }

    // Get resolved config with app defaults merged in
    let resolved_conf = get_resolved_conf(conf.clone());

    // Extract values from resolved config
    let host = resolved_conf.get_str_or_default("host", DEFAULT_APP_HOST);
    let port = resolved_conf.get_int_or_default("port", DEFAULT_APP_PORT as i64) as u16;
    let log_level_str = resolved_conf.get_str_or_default("log.level", "info");
    let log_level = log::string_to_level(&log_level_str);

    // Initialize the assets URL prefix for templates
    templates::init_assets_prefix(ASSETS_URL_PREFIX.to_string());

    // Initialize the build docs cache
    crate::build_cache::init_build_docs_cache(conf.clone());

    // Determine the assets directory using the resources system
    let assets_dir = match hot::resources::get_app_assets_path() {
        Ok(discovered_path) => {
            debug!(
                "hot.dev: Using discovered assets path: {}",
                discovered_path.display()
            );
            discovered_path.to_string_lossy().to_string()
        }
        Err(e) => {
            warn!(
                "hot.dev: Could not discover assets path ({}), using fallback: resources/app/assets",
                e
            );
            "resources/app/assets".to_string()
        }
    };

    // Check if the assets directory exists
    if !Path::new(&assets_dir).exists() {
        warn!(
            "hot.dev: Assets directory '{}' does not exist, creating it",
            assets_dir
        );
        if let Err(e) = std::fs::create_dir_all(&assets_dir) {
            warn!("hot.dev: Failed to create assets directory: {}", e);
        }
    }

    // Create database pool using db config from original full config
    let db_conf = hot::db::get_resolved_conf(conf.clone());

    // Run database migrations
    if let Err(e) = run_migrations(&db_conf).await {
        error!("hot.dev: Failed to run database migrations: {}", e);
        panic!("Failed to run database migrations: {}", e);
    }

    if let Err(e) = crate::database_bootstrap::database_bootstrap()
        .bootstrap(&db_conf)
        .await
    {
        error!("hot.dev: Failed to run database bootstrap: {}", e);
        panic!("Failed to run database bootstrap: {}", e);
    }

    let db = match create_db_pool(&db_conf).await {
        Ok(db) => Arc::new(db),
        Err(e) => {
            error!("hot.dev: Failed to create database pool: {}", e);
            panic!("Failed to create database pool: {}", e);
        }
    };

    // Check and insert default data if needed (local dev only)
    // In production, users must sign up through the normal flow
    if hot::env::is_local_dev() {
        match check_default_data_exists(&db).await {
            Ok((org_count, user_count)) => {
                if org_count == 0 || user_count == 0 {
                    info!(
                        "hot.dev: No default data found, inserting default organization and user"
                    );
                    if let Err(e) = insert_default_data(&db).await {
                        error!("hot.dev: Failed to insert default data: {}", e);
                    } else {
                        info!("hot.dev: Default data inserted successfully");
                    }
                } else {
                    debug!(
                        "hot.dev: Default data already exists (orgs: {}, users: {})",
                        org_count, user_count
                    );
                }
            }
            Err(e) => {
                error!("hot.dev: Failed to check default data: {}", e);
            }
        }
    }

    debug!("hot.dev: Database connection established");

    // Initialize stream pub/sub for real-time SSE updates (dashboard)
    // Use the shared instance if provided (for hot dev mode), otherwise create a new one
    let stream_pubsub: Option<Arc<StreamPubSub>> = if let Some(pubsub) = shared_stream_pubsub {
        debug!("hot.dev: APP using shared stream pub/sub instance");
        Some(pubsub)
    } else {
        let queue_type_str = conf.get_str_or_default("queue.type", "memory");
        let pubsub_type = match queue_type_str.as_str() {
            "redis" => StreamPubSubType::Redis,
            _ => StreamPubSubType::Memory,
        };

        let redis_uri_str = conf.get_str_or_default("redis.uri", "");
        let redis_uri = if redis_uri_str.is_empty() {
            None
        } else {
            Some(redis_uri_str)
        };

        let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

        match StreamPubSub::new(pubsub_type, redis_uri, redis_cluster) {
            Ok(pubsub) => {
                debug!(
                    "hot.dev: APP created stream pub/sub (type: {:?})",
                    pubsub_type
                );
                Some(Arc::new(pubsub))
            }
            Err(e) => {
                warn!(
                    "hot.dev: APP failed to create stream pub/sub: {}. Real-time updates will be unavailable.",
                    e
                );
                None
            }
        }
    };

    // Initialize the email queue for enqueuing app emails to the worker
    // This allows the app process to push emails to hot:email queue
    {
        let queue_type_str = conf.get_str_or_default("queue.type", "memory");
        let queue_type = match queue_type_str.as_str() {
            "redis" => hot::queue::QueueType::Redis,
            _ => hot::queue::QueueType::Memory,
        };
        let redis_uri_str = conf.get_str_or_default("redis.uri", "");
        let redis_uri = if redis_uri_str.is_empty() {
            None
        } else {
            Some(redis_uri_str)
        };
        let redis_cluster = conf.get_bool_or_default("redis.cluster", false);
        let serialization = hot::data::serialization::Serialization::default();

        match hot::queue::ProcessingQueue::<hot::data::msg::Message>::new_with_cluster(
            queue_type,
            "hot:email".to_string(),
            redis_uri,
            redis_cluster,
            serialization,
        ) {
            Ok(email_queue) => {
                hot::notification_queue::init_email_queue(std::sync::Arc::new(email_queue));
                debug!("hot.dev: APP initialized email queue for enqueuing");
            }
            Err(e) => {
                warn!(
                    "hot.dev: APP failed to create email queue: {}. App emails will be saved to DB only.",
                    e
                );
            }
        }
    }

    // Create shutdown signal channel for SSE handlers
    // When shutdown is triggered, we signal SSE handlers to close their connections
    // This prevents axum's graceful shutdown from hanging on open SSE connections
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Create our application with routes and request logging
    // Assets get a 1-year cache (assets have cache-busting via ?v= param, so safe to cache long)
    let assets_service = ServeDir::new(assets_dir);
    let cached_assets = tower::ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("public, max-age=31536000, immutable"),
        ))
        .service(assets_service);

    // Shared blob store so display pages can rehydrate spilled payloads.
    // None when blob.mode is "disabled".
    let blob_store = hot::blob::blob_store_from_conf(db.clone(), &conf).await;

    let app = routes::routes(db, conf, stream_pubsub, blob_store, shutdown_rx)
        .nest_service(ASSETS_URL_PREFIX, cached_assets)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(log_level))
                .on_request(trace::DefaultOnRequest::new().level(tracing::Level::DEBUG))
                .on_response(trace::DefaultOnResponse::new().level(tracing::Level::DEBUG)),
        );

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("hot.dev: APP listening on http://{}", addr);

    // Graceful shutdown handler
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            hot::signal::shutdown_signal().await;
            info!("hot.dev: APP received shutdown signal");
            let _ = shutdown_tx.send(true);
        })
        .await
        .unwrap();

    info!("hot.dev: APP shutdown complete");
}
