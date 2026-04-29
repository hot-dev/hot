use axum::{Extension, Router, extract::DefaultBodyLimit, middleware, routing::get};
use hot::storage::build_storage_from_config;
use hot::stream::{StreamPubSub, StreamPubSubType};
use hot::val;
use hot::val::Val;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::trace::{self, TraceLayer};
use tracing::{debug, info};

use crate::access_log::access_log_middleware;
use crate::auth::api_key_auth_middleware;
use crate::domain_resolver::{DomainCache, domain_resolution_middleware};
use crate::handlers::{
    // File handlers
    abort_upload,
    activate_project,
    complete_upload,
    create_context_variable,
    create_domain,
    create_project,
    create_service_key,
    create_session,
    deactivate_project,
    delete_context_variable,
    delete_domain,
    delete_file,
    delete_project,
    deploy_build,
    download_build,
    download_file,
    get_build,
    get_deployed_build,
    get_domain,
    get_env_info,
    get_event,
    get_event_runs,
    get_file,
    get_live_build,
    get_org_usage,
    get_project,
    get_run,
    get_run_stats,
    get_service_key,
    initiate_upload,
    list_builds,
    list_builds_by_env,
    list_context_variables,
    list_domains,
    list_events,
    list_files,
    list_project_event_handlers,
    list_project_schedules,
    list_projects,
    list_runs,
    list_service_keys,
    list_sessions,
    mcp_handler,
    mcp_handler_domain,
    mcp_legacy_messages_handler,
    mcp_legacy_messages_handler_domain,
    mcp_sse_handler,
    mcp_sse_handler_domain,
    publish_event,
    revoke_all_service_keys,
    revoke_all_sessions,
    revoke_service_key,
    revoke_session,
    root_handler,
    status_handler,
    subscribe_to_env,
    subscribe_to_stream,
    subscribe_to_stream_post,
    subscribe_with_event,
    update_context_variable,
    update_project,
    update_service_key,
    upload_build,
    upload_file,
    upload_part_handler,
    verify_domain,
    webhook_catch_all_handler,
};
use crate::rate_limit::rate_limit_middleware;

pub const DEFAULT_API_HOST: &str = "localhost";
pub const DEFAULT_API_PORT: u16 = 4681;
const MIN_PUBLIC_CONCURRENCY_LIMIT: usize = 128;
const PUBLIC_CONCURRENCY_PER_CPU: usize = 32;

pub fn get_resolved_conf(conf: Val) -> Val {
    // Create API defaults
    let api_defaults = val!({
        "host": DEFAULT_API_HOST,
        "port": DEFAULT_API_PORT as i64
    });

    // Extract API-specific configuration from the full config
    let api_section = conf.get("api").unwrap_or(Val::map_empty());

    // Merge defaults with API-specific config (config overrides defaults)
    api_defaults.merge(&api_section)
}

fn default_public_concurrency_limit() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4);
    std::cmp::max(
        MIN_PUBLIC_CONCURRENCY_LIMIT,
        cpus.saturating_mul(PUBLIC_CONCURRENCY_PER_CPU),
    )
}

pub async fn run(conf: Val) {
    run_with_stream_pubsub(conf, None).await
}

/// Run the API server with an optional pre-created stream pub/sub
/// This allows sharing a single pub/sub instance with the worker in dev mode
pub async fn run_with_stream_pubsub(conf: Val, shared_stream_pubsub: Option<Arc<StreamPubSub>>) {
    // Get resolved config with API defaults merged in
    let resolved_conf = get_resolved_conf(conf.clone());

    // Extract values from resolved config
    let host = resolved_conf.get_str_or_default("host", DEFAULT_API_HOST);
    let port = resolved_conf.get_int_or_default("port", DEFAULT_API_PORT as i64) as u16;

    // Max HTTP body size: 300 MB to support 256 MB multipart upload parts.
    // Handler-level checks enforce the real per-route limits (build size, plan
    // file upload caps, etc.), so this is just the outer safety net.
    let max_body_size: usize = 300 * 1024 * 1024;
    let public_concurrency_limit = resolved_conf
        .get_int_or_default(
            "public-concurrency-limit",
            default_public_concurrency_limit() as i64,
        )
        .max(1) as usize;

    // Note: Log level is configured in the full config, not API section
    // TraceLayer is configured with tracing::Level::INFO directly below

    // Initialize database pool
    tracing::debug!("hot.dev: API verifying database connectivity");
    let db_conf = hot::db::get_resolved_conf(conf.clone());
    let db = match hot::db::create_db_pool(&db_conf).await {
        Ok(pool) => {
            // Test the database connection
            match hot::db::test_connection(&pool).await {
                Ok(_) => {
                    tracing::debug!("hot.dev: API successfully connected to database");
                    Arc::new(pool)
                }
                Err(e) => {
                    tracing::error!("hot.dev: API failed to verify database connection: {}", e);
                    panic!("Database connection test failed: {}", e);
                }
            }
        }
        Err(e) => {
            tracing::error!("hot.dev: API failed to create database pool: {}", e);
            panic!("Failed to create database pool: {}", e);
        }
    };

    // Initialize build storage
    let storage = match build_storage_from_config(&conf).await {
        Ok(storage) => Arc::new(storage),
        Err(e) => {
            panic!("Failed to initialize build storage: {}", e);
        }
    };

    // Initialize file storage
    let file_storage: Arc<Box<dyn hot::file_storage::FileStorage>> =
        match hot::file_storage::file_storage_from_config(&conf).await {
            Ok(storage) => Arc::new(storage),
            Err(e) => {
                panic!("Failed to initialize file storage: {}", e);
            }
        };

    // Use provided stream pub/sub or create new one
    let stream_pubsub: Option<Arc<StreamPubSub>> = if shared_stream_pubsub.is_some() {
        debug!("hot.dev: API using shared stream pub/sub");
        shared_stream_pubsub
    } else {
        // Initialize stream pub/sub for real-time SSE updates
        // Use the same backend type as the queue (Memory or Redis)
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
                    "hot.dev: API created stream pub/sub (type: {:?})",
                    pubsub_type
                );
                Some(Arc::new(pubsub))
            }
            Err(e) => {
                tracing::warn!(
                    "hot.dev: API failed to create stream pub/sub: {}. SSE will fall back to polling.",
                    e
                );
                None
            }
        }
    };

    // Create shared state tuple - include config for handlers to access limits
    let state = (
        db.clone(),
        storage.clone(),
        Arc::new(conf.clone()),
        stream_pubsub,
    );

    // Create routes that don't require authentication
    let public_routes = Router::new()
        .route("/", get(root_handler))
        .route("/status", get(status_handler))
        // Webhook endpoints — single catch-all handles both standard and custom domain routes:
        //   Standard:      /webhook/{org}/{env}/{service}/{path...}/{token}
        //   Custom domain: /webhook/{service}/{path...}/{token}
        // Per-endpoint auth is handled inside the handler based on meta.
        .route(
            "/webhook/{*full_path}",
            axum::routing::any(webhook_catch_all_handler),
        )
        // MCP (Model Context Protocol) — public route with per-tool auth
        // URL includes env_name for explicit environment targeting (like webhooks)
        // Streamable HTTP (POST) + legacy SSE (GET)
        .route(
            "/mcp/{org_slug}/{env_name}/{service}",
            axum::routing::post(mcp_handler).get(mcp_sse_handler),
        )
        // MCP legacy HTTP+SSE transport — message POST endpoint
        .route(
            "/mcp/{org_slug}/{env_name}/{service}/messages",
            axum::routing::post(mcp_legacy_messages_handler),
        )
        // Custom domain routes (org/env resolved from domain)
        .route(
            "/mcp/{service}",
            axum::routing::post(mcp_handler_domain).get(mcp_sse_handler_domain),
        )
        .route(
            "/mcp/{service}/messages",
            axum::routing::post(mcp_legacy_messages_handler_domain),
        )
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MB for webhooks/MCP
        .layer(GlobalConcurrencyLimitLayer::new(public_concurrency_limit));

    // Create API v1 routes that require authentication
    let api_v1_routes = Router::new()
        // Projects
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/{project_id_or_slug}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/activate",
            axum::routing::post(activate_project),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/deactivate",
            axum::routing::post(deactivate_project),
        )
        // Builds - env-scoped endpoint for all builds across projects
        .route("/v1/builds", get(list_builds_by_env))
        // Builds - project-scoped endpoints
        .route("/v1/projects/{project_id_or_slug}/builds", get(list_builds))
        .route(
            "/v1/projects/{project_id_or_slug}/builds",
            axum::routing::post(upload_build),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/builds/deployed",
            get(get_deployed_build),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/builds/live",
            get(get_live_build),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/builds/{build_id}",
            get(get_build),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/builds/{build_id}/download",
            get(download_build),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/builds/{build_id}/deploy",
            axum::routing::post(deploy_build),
        )
        // Context Variables
        .route(
            "/v1/projects/{project_id_or_slug}/context",
            get(list_context_variables).post(create_context_variable),
        )
        .route(
            "/v1/projects/{project_id_or_slug}/context/{key}",
            axum::routing::put(update_context_variable).delete(delete_context_variable),
        )
        // Runs
        .route("/v1/runs", get(list_runs))
        .route("/v1/runs/stats", get(get_run_stats))
        .route("/v1/runs/{run_id}", get(get_run))
        // Events
        .route(
            "/v1/events",
            axum::routing::post(publish_event).get(list_events),
        )
        .route("/v1/events/{event_id}", get(get_event))
        .route("/v1/events/{event_id}/runs", get(get_event_runs))
        // Event Handlers (read-only, project-scoped)
        .route(
            "/v1/projects/{project_id_or_slug}/event-handlers",
            get(list_project_event_handlers),
        )
        // Schedules (read-only, project-scoped)
        .route(
            "/v1/projects/{project_id_or_slug}/schedules",
            get(list_project_schedules),
        )
        // Sessions
        .route(
            "/v1/sessions",
            axum::routing::post(create_session)
                .get(list_sessions)
                .delete(revoke_all_sessions),
        )
        .route(
            "/v1/sessions/{session_id}",
            axum::routing::delete(revoke_session),
        )
        // Service Keys
        .route(
            "/v1/service-keys",
            axum::routing::post(create_service_key)
                .get(list_service_keys)
                .delete(revoke_all_service_keys),
        )
        .route(
            "/v1/service-keys/{service_key_id}",
            axum::routing::get(get_service_key)
                .patch(update_service_key)
                .delete(revoke_service_key),
        )
        // Custom Domains
        .route(
            "/v1/domains",
            axum::routing::post(create_domain).get(list_domains),
        )
        .route(
            "/v1/domains/{domain_id}",
            axum::routing::get(get_domain).delete(delete_domain),
        )
        .route(
            "/v1/domains/{domain_id}/verify",
            axum::routing::post(verify_domain),
        )
        // Environment info
        .route("/v1/env", get(get_env_info))
        // Environment SSE subscription (real-time dashboard updates)
        .route("/v1/env/subscribe", get(subscribe_to_env))
        // Organization usage and limits
        .route("/v1/org/usage", get(get_org_usage))
        // Stream subscription (SSE) — GET (classic) + POST (Streamable HTTP)
        .route(
            "/v1/streams/{stream_id}/subscribe",
            get(subscribe_to_stream).post(subscribe_to_stream_post),
        )
        // Atomic subscribe + publish (eliminates race conditions)
        .route(
            "/v1/streams/subscribe-with-event",
            axum::routing::post(subscribe_with_event),
        )
        // Files
        .route("/v1/files", get(list_files))
        .route("/v1/files/{file_id}", get(get_file).delete(delete_file))
        .route("/v1/files/{file_id}/download", get(download_file))
        // File upload (simple)
        .route("/v1/files/upload/{*path}", axum::routing::put(upload_file))
        // Multipart uploads
        .route("/v1/files/uploads", axum::routing::post(initiate_upload))
        .route(
            "/v1/files/uploads/{upload_id}/{part_number}",
            axum::routing::put(upload_part_handler),
        )
        .route(
            "/v1/files/uploads/{upload_id}/complete",
            axum::routing::post(complete_upload),
        )
        .route(
            "/v1/files/uploads/{upload_id}",
            axum::routing::delete(abort_upload),
        )
        .layer(DefaultBodyLimit::max(max_body_size))
        .route_layer(middleware::from_fn_with_state(
            db.clone(),
            access_log_middleware,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api_key_auth_middleware,
        ));

    // Domain resolution cache for custom domains (mcp.example.com, etc.)
    let domain_cache = DomainCache::new();

    // Combine all routes with middleware layers (outermost → innermost):
    //   Trace → BodyLimit → DomainResolution → Router → (auth → rate_limit → access_log → handler)
    let app = Router::new()
        .merge(public_routes)
        .merge(api_v1_routes)
        .layer(middleware::from_fn_with_state(
            (db.clone(), domain_cache),
            domain_resolution_middleware,
        ))
        .layer(Extension(file_storage))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    let path = request.uri().path();
                    let redacted = if path.starts_with("/webhook/") {
                        // Redact the last path segment (capability token) from trace spans
                        match path.rfind('/') {
                            Some(pos) => format!("{}/[redacted]", &path[..pos]),
                            None => path.to_string(),
                        }
                    } else {
                        path.to_string()
                    };
                    tracing::info_span!(
                        "http_request",
                        method = %request.method(),
                        uri = %redacted,
                    )
                })
                .on_request(trace::DefaultOnRequest::new().level(tracing::Level::DEBUG))
                .on_response(trace::DefaultOnResponse::new().level(tracing::Level::DEBUG)),
        )
        .with_state(state);

    debug!("hot.dev: API max body size: {} bytes", max_body_size);
    debug!(
        "hot.dev: API public concurrency limit: {}",
        public_concurrency_limit
    );

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("hot.dev: API listening on http://{}", addr);

    // Graceful shutdown handler
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        hot::signal::shutdown_signal().await;
        info!("hot.dev: API received shutdown signal");
    })
    .await
    .unwrap();

    info!("hot.dev: API shutdown complete");
}

pub async fn handler() -> &'static str {
    "hot.dev api server"
}
