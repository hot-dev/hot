//! Webhook endpoint handlers
//!
//! Exposes Hot functions as HTTP endpoints that external services (Slack, Stripe,
//! GitHub, etc.) can POST/GET to directly.
//!
//! Route: `ANY /webhook/{org_slug}/{service}/*path`
//!
//! Webhook endpoints are **public by default** -- external services call them
//! without a Hot API key. Per-endpoint auth is configured via the `auth` meta
//! field: `"none"` (default) or `"required"` (requires Bearer token -- API key,
//! service key, or session).

use ahash::{AHashMap, AHashSet};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
};
use hot::db::{build::Build, org::Org, project::Project, webhook::Webhook as DbWebhook};
use hot::permission::actions;
use hot::val::Val;
use once_cell::sync::OnceCell;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

use crate::ApiStateData;
use crate::auth::{AuthContext, authenticate_or_reject};
use crate::client_ip::{ClientIp, TrustedProxyPeer};
use crate::domain_resolver::ResolvedDomain;
use crate::models::ApiErrorResponse;
use crate::rate_limit::{self, PublicRateLimitMode};

use super::request::{
    RequestBody, bind_call_event_to_build, build_call_event_data, build_request_val,
    hash_sensitive_request_fields, redact_event_data_for_persistence,
};

/// The queue name that webhooks publish events to.
/// This MUST match the worker's event queue name ("hot:event").
const WEBHOOK_EVENT_QUEUE_NAME: &str = "hot:event";
static INVALID_WEBHOOK_TOKEN_TOTAL: AtomicU64 = AtomicU64::new(0);

fn record_invalid_webhook_token(reason: &'static str) {
    let total = INVALID_WEBHOOK_TOKEN_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(
        metric = "webhook_invalid_token_total",
        reason,
        total,
        "Rejected invalid webhook capability token"
    );
}

// ============================================================================
// Shared Event Queue (initialized once, reused across requests)
// ============================================================================

static WEBHOOK_EVENT_QUEUE: OnceCell<Arc<hot::queue::ProcessingQueue<hot::data::msg::Message>>> =
    OnceCell::new();

/// Get or initialize the shared event queue from config.
fn get_event_queue(
    conf: &Val,
) -> Result<&'static Arc<hot::queue::ProcessingQueue<hot::data::msg::Message>>, String> {
    WEBHOOK_EVENT_QUEUE.get_or_try_init(|| {
        let queue_type_str = conf.get_str_or_default("queue.type", "sqlite");
        let queue_type = hot::queue::QueueType::from_str(&queue_type_str)
            .map_err(|_| format!("Invalid queue.type configuration: '{}'", queue_type_str))?;

        let redis_uri_str = conf.get_str_or_default("redis.uri", "");
        let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
            None
        } else {
            Some(redis_uri_str)
        };

        let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

        let serialization_str = conf.get_str_or_default("serialization.type", "zstdjson");
        let serialization = hot::data::serialization::Serialization::from_str(&serialization_str)
            .unwrap_or_default();

        let queue = hot::queue::ProcessingQueue::<hot::data::msg::Message>::new_with_cluster(
            queue_type,
            WEBHOOK_EVENT_QUEUE_NAME.to_string(),
            redis_uri,
            redis_cluster,
            serialization,
        )
        .map_err(|e| format!("Failed to create event queue: {}", e))?;

        tracing::debug!(
            "Webhook: initialized shared event queue (type: {})",
            queue_type_str
        );
        Ok(Arc::new(queue))
    })
}

/// Resolved webhook path context.
struct WebhookPathContext {
    org: Org,
    env: hot::db::env::Env,
    service: String,
    endpoint_path: String,
}

/// Resolve org, env, and service from the webhook URL path.
///
/// Unlike MCP, webhooks are public by default and don't require an API key
/// for org/env resolution. Instead, org_slug and env_name come from the URL,
/// giving human-readable URLs and multi-environment support.
async fn resolve_webhook_path(
    db: &hot::db::DatabasePool,
    org_slug: &str,
    env_name: &str,
    service: String,
    endpoint_path: String,
) -> Result<WebhookPathContext, (StatusCode, Json<ApiErrorResponse>)> {
    // Look up org by slug
    let org = Org::get_org_by_slug(db, org_slug).await.map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Organization")),
        )
    })?;

    // Look up env by org + name
    let env = hot::db::Env::get_env_by_org_and_name(db, &org.org_id, env_name)
        .await
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::new(
                    "not_found",
                    format!(
                        "Environment '{}' not found for organization '{}'",
                        env_name, org_slug
                    ),
                )),
            )
        })?;

    Ok(WebhookPathContext {
        org,
        env,
        service,
        endpoint_path,
    })
}

/// Resolve webhook context from a custom domain's ResolvedDomain extension.
async fn resolve_webhook_path_from_domain(
    db: &hot::db::DatabasePool,
    resolved: &ResolvedDomain,
    service: String,
    endpoint_path: String,
) -> Result<WebhookPathContext, (StatusCode, Json<ApiErrorResponse>)> {
    let org = Org::get_org(db, &resolved.org_id).await.map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Organization")),
        )
    })?;

    let env = hot::db::Env::get_env(db, &resolved.env_id)
        .await
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Environment")),
            )
        })?;

    Ok(WebhookPathContext {
        org,
        env,
        service,
        endpoint_path,
    })
}

/// Unified webhook catch-all handler for `/webhook/{*full_path}`.
///
/// Parses the path segments to determine the routing source:
/// - Standard routes: `/webhook/{org}/{env}/{service}/{endpoint_path}/{token}` (4+ segments)
/// - Custom domain routes: `/webhook/{service}/{endpoint_path}/{token}` (2+ segments, requires ResolvedDomain)
///
/// This avoids Axum route conflicts between the two patterns (both use catch-all wildcards).
#[allow(clippy::too_many_arguments)]
pub async fn webhook_catch_all_handler(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    method: Method,
    client_ip: Option<Extension<ClientIp>>,
    trusted_proxy: Option<Extension<TrustedProxyPeer>>,
    blob_store: Option<Extension<Option<Arc<hot::blob::BlobStore>>>>,
    resolved_domain: Option<Extension<ResolvedDomain>>,
    OriginalUri(original_uri): OriginalUri,
    Path(full_path): Path<String>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
    body: Bytes,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    let trust_forwarded = trusted_proxy.map(|Extension(t)| t.0).unwrap_or(true);
    let original_url = build_original_url_with_trust(&headers, &original_uri, trust_forwarded);
    let blob_store = super::blob_store_from_ext(blob_store);
    let segments: Vec<&str> = full_path.trim_matches('/').splitn(4, '/').collect();

    let (source, path) = if let Some(Extension(resolved)) = resolved_domain {
        // Custom domain: /webhook/{service}/{path...}
        if segments.len() < 2 {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook endpoint")),
            ));
        }
        let service = segments[0].to_string();
        let path = segments[1..].join("/");
        (WebhookRouteSource::Domain { resolved, service }, path)
    } else {
        // Standard: /webhook/{org}/{env}/{service}/{path...}
        if segments.len() < 4 {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook endpoint")),
            ));
        }
        let org_slug = segments[0].to_string();
        let env_name = segments[1].to_string();
        let service = segments[2].to_string();
        let path = segments[3].to_string();
        (
            WebhookRouteSource::Path {
                org_slug,
                env_name,
                service,
            },
            path,
        )
    };

    webhook_handler_inner(
        &db,
        &conf,
        &stream_pubsub,
        blob_store.as_ref(),
        method,
        path,
        original_url,
        client_ip.map(|extension| extension.0),
        headers,
        query,
        body,
        source,
    )
    .await
}

fn validate_authority_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.contains(['?', '#', '/', '@', '\\'])
        || trimmed
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return None;
    }
    let authority = axum::http::uri::Authority::from_str(trimmed).ok()?;
    Some(authority.as_str().to_string())
}

/// Reconstruct the URL the caller originally requested: scheme and host from
/// the forwarding headers set by Hot's edge (falling back to the Host header),
/// plus the original path and query verbatim — token segment included, no
/// re-encoding. Providers that sign the URL they call (Twilio, HubSpot) hash
/// this exact string, so any normalization here breaks their signatures.
/// Returns None when no host can be determined.
#[cfg(test)]
fn build_original_url(headers: &HeaderMap, original_uri: &Uri) -> Option<String> {
    build_original_url_with_trust(headers, original_uri, true)
}

fn build_original_url_with_trust(
    headers: &HeaderMap,
    original_uri: &Uri,
    trust_forwarded: bool,
) -> Option<String> {
    use super::request::first_forwarded_value;

    let scheme = if trust_forwarded {
        first_forwarded_value(headers, "x-forwarded-proto")
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "http" | "https"))
    } else {
        None
    }
    .or_else(|| {
        original_uri
            .scheme_str()
            .map(str::to_ascii_lowercase)
            .filter(|s| matches!(s.as_str(), "http" | "https"))
    })
    .unwrap_or_else(|| "https".to_string());

    let host = if trust_forwarded {
        first_forwarded_value(headers, "x-forwarded-host").and_then(|h| validate_authority_host(&h))
    } else {
        None
    }
    .or_else(|| first_forwarded_value(headers, "host").and_then(|h| validate_authority_host(&h)))
    .or_else(|| {
        original_uri
            .authority()
            .and_then(|a| validate_authority_host(a.as_str()))
    })?;

    // Providers sign the URL as configured, which omits the default port —
    // but proxies sometimes inject it into Host/X-Forwarded-Host. Strip it
    // so the reconstruction matches the signed string; non-default ports are
    // kept (a URL configured with one carries it).
    let default_port = match scheme.as_str() {
        "https" => Some(":443"),
        "http" => Some(":80"),
        _ => None,
    };
    let host = match default_port {
        Some(suffix) => host.strip_suffix(suffix).unwrap_or(&host).to_string(),
        None => host,
    };

    let path_and_query = original_uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    Some(format!("{}://{}{}", scheme, host, path_and_query))
}

enum WebhookRouteSource {
    Path {
        org_slug: String,
        env_name: String,
        service: String,
    },
    Domain {
        resolved: ResolvedDomain,
        service: String,
    },
}

impl WebhookRouteSource {
    async fn resolve(
        self,
        db: &hot::db::DatabasePool,
        endpoint_path: String,
    ) -> Result<WebhookPathContext, (StatusCode, Json<ApiErrorResponse>)> {
        match self {
            WebhookRouteSource::Path {
                org_slug,
                env_name,
                service,
            } => resolve_webhook_path(db, &org_slug, &env_name, service, endpoint_path).await,
            WebhookRouteSource::Domain { resolved, service } => {
                resolve_webhook_path_from_domain(db, &resolved, service, endpoint_path).await
            }
        }
    }

    fn service(&self) -> &str {
        match self {
            WebhookRouteSource::Path { service, .. }
            | WebhookRouteSource::Domain { service, .. } => service,
        }
    }

    fn url_path(&self, endpoint_path: &str) -> String {
        match self {
            WebhookRouteSource::Path {
                org_slug,
                env_name,
                service,
            } => format!(
                "/webhook/{}/{}/{}{}",
                org_slug, env_name, service, endpoint_path
            ),
            WebhookRouteSource::Domain { service, .. } => {
                format!("/webhook/{}{}", service, endpoint_path)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn webhook_handler_inner(
    db: &Arc<hot::db::DatabasePool>,
    conf: &Arc<Val>,
    stream_pubsub: &Option<Arc<hot::stream::StreamPubSub>>,
    blob_store: Option<&Arc<hot::blob::BlobStore>>,
    method: Method,
    path: String,
    original_url: Option<String>,
    client_ip: Option<ClientIp>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
    body: Bytes,
    source: WebhookRouteSource,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    let on_custom_domain = matches!(source, WebhookRouteSource::Domain { .. });
    let normalized = path.trim_end_matches('/');
    let (endpoint_path, token) = if let Some((prefix, last)) = normalized.rsplit_once('/') {
        if last.len() == 12 && last.chars().all(|c| c.is_ascii_hexdigit()) {
            let ep = if prefix.is_empty() {
                "/".to_string()
            } else if prefix.starts_with('/') {
                prefix.to_string()
            } else {
                format!("/{}", prefix)
            };
            (ep, last.to_string())
        } else {
            record_invalid_webhook_token("malformed");
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook endpoint")),
            ));
        }
    } else if normalized.len() == 12 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        ("/".to_string(), normalized.to_string())
    } else {
        record_invalid_webhook_token("malformed");
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Webhook endpoint")),
        ));
    };

    let method_str = method.as_str().to_uppercase();

    tracing::debug!(
        "Webhook request: {} {}{}",
        method_str,
        source.service(),
        endpoint_path
    );

    let url_path = source.url_path(&endpoint_path);
    let routing_db = db.with_scope(hot::db::DatabaseScope::Bootstrap(
        hot::db::BootstrapOperation::Routing,
    ));
    let ctx = source.resolve(&routing_db, endpoint_path.clone()).await?;
    let scope = crate::auth::runtime_database_scope(db, ctx.env.env_id)
        .await
        .map_err(|status| {
            (
                status,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Environment is not admitting requests",
                )),
            )
        })?;
    let scoped_db = Arc::new(db.with_scope(scope));
    let db = &scoped_db;

    // Apply the org RPS limit before the endpoint query so random paths and
    // invalid tokens cannot turn the webhook table into an unbounded oracle.
    if let Err(exceeded) = rate_limit::check_org_rate_limit(
        db,
        &ctx.org.org_id,
        PublicRateLimitMode::from_conf(conf),
        "webhook",
    )
    .await
    {
        return Ok(rate_limit::rate_limit_response(exceeded));
    }

    // Look up the webhook
    let endpoint = match DbWebhook::get_by_env_service_path_method(
        db,
        &ctx.env.env_id,
        &ctx.service,
        &ctx.endpoint_path,
        &method_str,
        &token,
    )
    .await
    {
        Ok(e) => e,
        Err(hot::db::webhook::WebhookError::NotFound) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook endpoint")),
            ));
        }
        Err(hot::db::webhook::WebhookError::InvalidToken) => {
            record_invalid_webhook_token("mismatch");
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook endpoint")),
            ));
        }
        Err(e) => {
            tracing::error!("Failed to look up webhook endpoint: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to look up webhook endpoint: {}",
                    e
                ))),
            ));
        }
    };

    // If auth_mode is "required", validate the Authorization header.
    // Uses the shared authenticate_token() path for all credential types
    // (API keys, service keys, and sessions).
    // Auth context is preserved for inclusion in the HttpRequest value.
    let auth_result: Option<(AuthContext, hot::db::api_key::ApiKey)> = if endpoint.auth_mode()
        == "required"
    {
        let auth_header = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let token = auth_header.strip_prefix("Bearer ").unwrap_or("");
        if token.is_empty() {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ApiErrorResponse::new(
                    "unauthorized",
                    "Webhook endpoint requires authentication. Provide Authorization: Bearer <token>",
                )),
            ));
        }

        // Authenticate via the shared path (handles API keys, sessions, service keys)
        let authenticated = authenticate_or_reject(db, token).await?;

        let auth_ctx = authenticated.auth_ctx;

        // SECURITY: Verify the credential belongs to this environment.
        // Prevents cross-env authorization bypass when two environments define
        // the same webhook service/path and permission strings overlap.
        if auth_ctx.env_id() != ctx.env.env_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credentials do not belong to this environment",
                )),
            ));
        }

        // Permission check: all credential types use unified permissions
        {
            let webhook_resource = format!("webhook:{}{}", ctx.service, endpoint_path);
            if !auth_ctx.has_permission(&webhook_resource, actions::EXECUTE)
                && !auth_ctx.has_permission(&format!("webhook:{}/*", ctx.service), actions::EXECUTE)
                && !auth_ctx.has_permission("webhook:*", actions::EXECUTE)
            {
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(ApiErrorResponse::new(
                        "forbidden",
                        "Credential does not have execute access to this webhook endpoint",
                    )),
                ));
            }
        }

        Some((auth_ctx, authenticated.api_key))
    } else {
        None
    };

    let _inflight_guard =
        match rate_limit::check_public_org_inflight(db, conf, &ctx.org.org_id, "webhook").await {
            Ok(guard) => guard,
            Err(exceeded) => return Ok(rate_limit::rate_limit_response(exceeded)),
        };

    // Build the unified HttpRequest value using the shared builder.
    // Includes method, url, headers, query, body, body-raw, ip, and auth (when authenticated).
    // One UTF-8 validation: valid bodies use the borrowed str directly; for
    // non-UTF-8 payloads body-raw is lossy, so carry the verbatim bytes too
    // so body-signing webhook verifiers hash what the provider actually sent.
    let (raw_body_str, body_bytes) = match std::str::from_utf8(&body) {
        Ok(s) => (s.to_string(), None),
        Err(_) => (
            String::from_utf8_lossy(&body).to_string(),
            Some(body.to_vec()),
        ),
    };
    let body_val: Val =
        serde_json::from_slice(&body).unwrap_or_else(|_| Val::from(raw_body_str.clone()));

    let http_request = build_request_val(
        &method_str,
        &url_path,
        original_url.as_deref(),
        &headers,
        client_ip.as_ref(),
        &query.0,
        Some(RequestBody {
            body: body_val,
            body_raw: raw_body_str,
            body_bytes,
        }),
        auth_result.as_ref(),
        &ctx.org.org_id,
    );

    // Execute the Hot function via the event queue
    let function_name = format!("{}/{}", endpoint.ns, endpoint.var);
    let event_id = Uuid::now_v7();
    let stream_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();

    // Resolve the selected deployment completely before persisting or queuing
    // work. Child organizations keep their member directory at the root, so an
    // environment-scoped query cannot choose a "default" user under RLS.
    let build = Build::get_build(db, &endpoint.build_id)
        .await
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook deployment")),
            )
        })?;
    // A deployment this runtime can't run, made with another major version
    // of Hot, is unavailable until the project is redeployed. A sender that
    // retries delivers the request once it is.
    if build.runtime_version_error_message().is_some() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::new(
                "redeploy_required",
                "This webhook is unavailable until its project is redeployed for this version of Hot",
            )),
        ));
    }
    let project = Project::get_project(db, &build.project_id)
        .await
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook deployment")),
            )
        })?;
    if project.env_id != ctx.env.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Webhook deployment")),
        ));
    }
    // Authenticated calls retain their credential's creator; public capability
    // calls belong to the deployment creator. Neither selects an unrelated
    // global user or invents a UUID when the member directory is not visible.
    let user_id = auth_result
        .as_ref()
        .map(|(_, key)| key.created_by_user_id)
        .unwrap_or(build.created_by_user_id);
    let build_hash = Some(build.hash);
    let project_id = Some(build.project_id);
    let project_name = Some(project.name);

    let mut execution_context = hot::lang::event::ExecutionContext {
        run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Call.as_id(),
        env_id: Some(ctx.env.env_id),
        env_name: Some(ctx.env.name.clone()),
        user_id: Some(user_id),
        org_id: Some(ctx.org.org_id),
        org_slug: Some(ctx.org.slug.clone()),
        build_id: Some(endpoint.build_id),
        build_hash,
        project_id,
        project_name: project_name.clone(),
        event_id: Some(event_id),
        origin_run_id: None,
        retry_attempt: 0,
        max_retries: 0,
        retry_delay_ms: 0,
        secret_keys: AHashSet::new(),
        secret_value_hashes: AHashSet::new(),
        secret_values: Default::default(),
        access_id: None,
        agent_type: None,
        event_handler_fn: None,
        queue_timing: None,
        // Enqueue-only producer context; the worker stamps deadline_at at admission
        deadline_at: None,
        trace_calls: true,
    };

    // Pre-compute secret value hashes for targeted masking of sensitive headers
    // and the auth subtree in run logs.
    let extra_secret_headers = endpoint.secret_headers();
    {
        let sensitive_hashes = hash_sensitive_request_fields(&http_request, &extra_secret_headers);
        for h in sensitive_hashes {
            execution_context.secret_value_hashes.insert(h);
        }
    }

    // Build args: single argument (the HttpRequest).
    // The same HttpRequest is also passed as `caller` so the worker injects it
    // as the `hot.request` context variable.
    let args_val = Val::vec(vec![http_request.clone()]);
    let mut event_data_val = build_call_event_data(&function_name, args_val, Some(http_request));
    bind_call_event_to_build(&mut event_data_val, endpoint.build_id);
    // The persisted copy must not retain webhook capability tokens, sensitive
    // HTTP headers, or decrypted service-key metadata: store a redacted copy
    // while keeping raw values only in the transient call_event queue payload.
    let persisted_event_data = redact_event_data_for_persistence(
        &event_data_val,
        auth_result.is_some(),
        &extra_secret_headers,
    );
    let event_data_json = serde_json::to_value(persisted_event_data.to_hot_data_repr())
        .unwrap_or(serde_json::Value::Null);

    // Create call event for the worker queue
    let call_event = hot::lang::event::Event {
        event_id,
        env_id: ctx.env.env_id,
        stream_id,
        event_type: "hot:call".to_string(),
        event_data: event_data_val,
        event_time: chrono::Utc::now(),
        target_project_id: project_id,
        target_project_name: project_name.clone(),
    };

    let event_message = hot::lang::event::EventMessage {
        id: event_id,
        head: AHashMap::from_iter([
            ("env_id".to_string(), ctx.env.env_id.to_string()),
            ("event_type".to_string(), "hot:call".to_string()),
            ("function".to_string(), function_name.clone()),
        ]),
        body: hot::lang::event::EventMessageBody {
            event: call_event,
            execution_context,
        },
    };

    let message: hot::data::msg::Message = event_message.into();

    // Require pub/sub and event queue BEFORE inserting the event row so
    // misconfigured or unavailable infrastructure does not leave orphan rows.
    let pubsub = match stream_pubsub.as_ref() {
        Some(p) => p,
        None => {
            tracing::error!("Webhook handler requires stream pub/sub but none is configured");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(
                    "Webhook delivery is temporarily unavailable",
                )),
            ));
        }
    };

    use hot::queue::Queue;
    let queue = get_event_queue(conf).map_err(|e| {
        tracing::error!("Webhook failed to initialize event queue: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(
                "Webhook delivery is temporarily unavailable",
            )),
        )
    })?;

    // Subscribe to pub/sub BEFORE inserting/enqueueing (prevents race condition)
    use hot::stream::StreamSubscriberFactory;
    let subscriber = pubsub
        .subscribe_in_env(ctx.env.env_id, stream_id)
        .await
        .map_err(|e| {
            tracing::error!(%stream_id, "Webhook failed to subscribe to stream: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(
                    "Webhook delivery is temporarily unavailable",
                )),
            )
        })?;

    // Insert event into database (security: prevents spoofed messages)
    if let Err(e) = hot::db::Event::insert_event(
        db,
        &event_id,
        &ctx.env.env_id,
        &stream_id,
        "hot:call",
        &event_data_json,
        chrono::Utc::now(),
        &user_id,
        None,
    )
    .await
    {
        tracing::error!(%event_id, "Webhook failed to persist event: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error("Internal server error")),
        ));
    }

    queue.enqueue(message).await.map_err(|e| {
        tracing::error!(%event_id, "Webhook failed to enqueue call: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error("Internal server error")),
        )
    })?;

    tracing::info!(
        "Webhook call: {} ({}) queued with run_id {} [org={}, service={}, path={}]",
        endpoint.name,
        function_name,
        run_id,
        ctx.org.slug,
        ctx.service,
        ctx.endpoint_path
    );

    // Wait for the result synchronously (no SSE streaming for webhooks)
    // Webhook timeout: 30 seconds (configurable, clamped to 1..=300s to prevent wrap/panic)
    let timeout_seconds = conf.get_int_or_default("webhook.timeout", 30).clamp(1, 300) as u64;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_seconds);
    let (outcome, timed_out) = wait_for_webhook_outcome(
        db,
        blob_store,
        ctx.env.env_id,
        event_id,
        subscriber,
        deadline,
    )
    .await;

    // Authorization was checked before durable acceptance. Return this
    // execution's outcome so providers do not replay committed side effects.
    // Convert the outcome to an HTTP response
    match outcome {
        Some(WebhookOutcome::Stopped { result }) => {
            build_webhook_response(result, on_custom_domain)
        }
        Some(WebhookOutcome::Failed { error }) => {
            let error_text =
                error.unwrap_or_else(|| "Webhook function execution failed".to_string());
            tracing::warn!("Webhook function failed: {}", error_text);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Webhook function execution failed"})),
            )
                .into_response())
        }
        Some(WebhookOutcome::Cancelled { reason }) => {
            if let Some(ref r) = reason {
                tracing::warn!("Webhook execution cancelled: {}", r);
            }
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Execution cancelled"})),
            )
                .into_response())
        }
        None if timed_out => {
            tracing::warn!(
                "Webhook call timed out after {}s for run_id {}",
                timeout_seconds,
                run_id
            );
            Ok((
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": format!("Webhook function timed out after {}s", timeout_seconds)})),
            )
                .into_response())
        }
        None => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Stream subscription closed unexpectedly"})),
        )
            .into_response()),
    }
}

/// Build an HTTP response from the Hot function's return value.
///
/// If the return value is an HttpResponse-shaped map (with status, and optionally headers/body),
/// use those fields. Otherwise, wrap the raw result as a 200 JSON response.
///
/// On the shared API host, which is same-site with the app, a tenant's
/// response can't set cookies, and a browser renders it sandboxed: no
/// scripts, no form posts, no content sniffing. A custom domain is the
/// tenant's own site, so its headers pass through unchanged.
fn shared_host_response_headers(response: &mut axum::response::Response) {
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("sandbox"),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
}

fn is_blocked_framing_header(name: &axum::http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
    )
}

fn is_blocked_shared_host_header(name: &axum::http::HeaderName) -> bool {
    let s = name.as_str();
    matches!(
        s,
        "set-cookie"
            | "content-security-policy"
            | "content-security-policy-report-only"
            | "x-content-type-options"
            | "clear-site-data"
            | "strict-transport-security"
            | "refresh"
    ) || s.starts_with("access-control-")
        || s.starts_with("cross-origin-")
}

fn build_webhook_response(
    result: Option<serde_json::Value>,
    on_custom_domain: bool,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    // A value that is not an `HttpResponse` is a 200 JSON response. On the
    // shared host it carries the same sandbox headers as any other response.
    let plain = |value: serde_json::Value| {
        let mut response = (StatusCode::OK, Json(value)).into_response();
        if !on_custom_domain {
            shared_host_response_headers(&mut response);
        }
        response
    };

    let result = match result {
        Some(r) => r,
        None => return Ok(plain(serde_json::Value::Null)),
    };

    // Unwrap typed values: Hot types serialize as {"$type": "...", "$val": {...}}
    // For HttpResponse, the actual fields (status, body, headers) are inside $val
    let unwrapped = if let serde_json::Value::Object(ref map) = result {
        if let (Some(type_val), Some(val)) = (map.get("$type"), map.get("$val")) {
            let type_str = type_val.as_str().unwrap_or("");
            if type_str == "HttpResponse" || type_str.ends_with("/HttpResponse") {
                // Use the inner $val which contains {status, body, headers}
                val.clone()
            } else {
                result.clone()
            }
        } else {
            result.clone()
        }
    } else {
        result.clone()
    };

    // A typed `HttpResponse` is always a response. An untyped map is one only
    // when its `status` is a final HTTP status code (200..=599): `{status: "ok"}`
    // and informational 1xx statuses are never emitted as webhook responses.
    let typed_response = matches!(
        &result,
        serde_json::Value::Object(map)
            if map.contains_key("$val")
                && map.get("$type").and_then(|t| t.as_str()).is_some_and(|t| {
                    t == "HttpResponse" || t.ends_with("/HttpResponse")
                })
    );
    if let serde_json::Value::Object(ref map) = unwrapped
        && (typed_response
            || map
                .get("status")
                .and_then(|status| status.as_u64())
                .is_some_and(|status| (200..=599).contains(&status)))
    {
        let status_code = map
            .get("status")
            .and_then(|v| v.as_u64())
            .filter(|s| (200..=599).contains(s))
            .map(|s| s as u16)
            .unwrap_or(if typed_response && map.contains_key("status") {
                500
            } else {
                200
            });

        let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);

        // RFC 9110: 204 No Content and 304 Not Modified must not carry a body.
        let mut response = if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED
        {
            (status, axum::body::Body::empty()).into_response()
        } else {
            let body = map.get("body").cloned().unwrap_or(serde_json::Value::Null);
            let mut resp = Json(body).into_response();
            *resp.status_mut() = status;
            resp
        };

        if let Some(serde_json::Value::Object(headers)) = map.get("headers") {
            for (key, value) in headers {
                if let Some(v) = value.as_str()
                    && let (Ok(header_name), Ok(header_value)) = (
                        axum::http::HeaderName::from_str(key),
                        axum::http::HeaderValue::from_str(v),
                    )
                {
                    if is_blocked_framing_header(&header_name) {
                        continue;
                    }
                    if !on_custom_domain && is_blocked_shared_host_header(&header_name) {
                        continue;
                    }
                    response.headers_mut().insert(header_name, header_value);
                }
            }
        }
        if !on_custom_domain {
            shared_host_response_headers(&mut response);
        }

        return Ok(response);
    }

    // Not an HttpResponse -- return as plain 200 JSON
    // Also unwrap $type/$val for other typed returns (clean JSON for the caller)
    if let serde_json::Value::Object(ref map) = result
        && map.contains_key("$type")
        && map.contains_key("$val")
    {
        return Ok(plain(map.get("$val").cloned().unwrap_or(result)));
    }
    Ok(plain(result))
}

#[derive(Debug)]
enum WebhookOutcome {
    Stopped { result: Option<serde_json::Value> },
    Failed { error: Option<String> },
    Cancelled { reason: Option<String> },
}

async fn wait_for_webhook_outcome(
    db: &hot::db::DatabasePool,
    blob_store: Option<&Arc<hot::blob::BlobStore>>,
    env_id: Uuid,
    event_id: Uuid,
    mut subscriber: Box<dyn hot::stream::StreamSubscriber>,
    deadline: tokio::time::Instant,
) -> (Option<WebhookOutcome>, bool) {
    use hot::stream::{StreamEvent, StreamNext};

    let mut outcome: Option<WebhookOutcome> = None;
    let mut timed_out = false;

    loop {
        tokio::select! {
            event = async { subscriber.next().await } => {
                match event {
                    StreamNext::Event(StreamEvent::RunStop {
                        run_id: completed_run,
                        event_id: completed_event_id,
                        result,
                        ..
                    }) if completed_event_id == Some(event_id) => {
                        outcome = Some(match super::resolve_stream_result(db, blob_store, env_id, completed_run, result).await {
                            Ok(result) => WebhookOutcome::Stopped { result },
                            Err(error) => WebhookOutcome::Failed { error: Some(error) },
                        });
                        break;
                    }
                    StreamNext::Event(StreamEvent::RunFail {
                        event_id: failed_event_id,
                        error,
                        ..
                    }) if failed_event_id == Some(event_id) => {
                        outcome = Some(WebhookOutcome::Failed { error });
                        break;
                    }
                    StreamNext::Event(StreamEvent::RunCancel {
                        event_id: cancelled_event_id,
                        reason,
                        ..
                    }) if cancelled_event_id == Some(event_id) => {
                        outcome = Some(WebhookOutcome::Cancelled { reason });
                        break;
                    }
                    StreamNext::Event(_) => {
                        // Ignore non-terminal events and terminal events from
                        // child events emitted on the same stream.
                        continue;
                    }
                    StreamNext::Idle => {}
                    StreamNext::Closed => {
                        break;
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                timed_out = true;
                break;
            }
        }
    }

    (outcome, timed_out)
}

#[cfg(test)]
pub(crate) mod postgres_lifecycle;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn test_wait_for_webhook_outcome_ignores_child_event_terminals_on_same_stream() {
        use hot::stream::{
            StreamEvent, StreamPublisher, StreamSubscriberFactory, mem::MemStreamPubSub,
        };

        let db = hot::db::test_db().await;
        let pubsub = MemStreamPubSub::new();
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let webhook_event_id = Uuid::now_v7();
        let child_event_id = Uuid::now_v7();

        let subscriber = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();

        // Child runs sharing `stream_id` finish/fail/cancel before the webhook itself finishes.
        pubsub
            .publish(StreamEvent::RunStop {
                run_id: Uuid::now_v7(),
                env_id,
                stream_id,
                event_id: Some(child_event_id),
                result: Some(serde_json::json!({"hijack": "stop"})),
            })
            .await
            .unwrap();
        pubsub
            .publish(StreamEvent::RunFail {
                run_id: Uuid::now_v7(),
                env_id,
                stream_id,
                event_id: Some(child_event_id),
                error: Some("child failed".to_string()),
            })
            .await
            .unwrap();
        pubsub
            .publish(StreamEvent::RunCancel {
                run_id: Uuid::now_v7(),
                env_id,
                stream_id,
                event_id: Some(child_event_id),
                reason: Some("child cancelled".to_string()),
            })
            .await
            .unwrap();
        // Now the webhook's own event completes.
        pubsub
            .publish(StreamEvent::RunStop {
                run_id: Uuid::now_v7(),
                env_id,
                stream_id,
                event_id: Some(webhook_event_id),
                result: Some(serde_json::json!({"ok": true})),
            })
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        let (outcome, timed_out) =
            wait_for_webhook_outcome(&db, None, env_id, webhook_event_id, subscriber, deadline)
                .await;
        assert!(!timed_out);
        match outcome {
            Some(WebhookOutcome::Stopped { result }) => {
                assert_eq!(result, Some(serde_json::json!({"ok": true})));
            }
            other => panic!("expected webhook Stopped outcome, got {other:?}"),
        }
    }

    #[test]
    fn test_event_queue_name_matches_worker() {
        assert_eq!(
            WEBHOOK_EVENT_QUEUE_NAME, "hot:event",
            "Webhooks must publish to 'hot:event' queue where the worker consumes from"
        );
    }

    // ========================================================================
    // build_webhook_response tests
    // ========================================================================

    #[tokio::test]
    async fn test_build_response_none_returns_null_200() {
        let response = build_webhook_response(None, false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!(null));
    }

    #[tokio::test]
    async fn test_build_response_plain_value_returns_200_json() {
        let result = serde_json::json!({"message": "hello"});
        let response = build_webhook_response(Some(result.clone()), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, result);
    }

    #[tokio::test]
    async fn test_build_response_http_response_with_status() {
        // Flat format (no $type/$val wrapper) — e.g., plain map with status
        let result = serde_json::json!({
            "status": 201,
            "headers": {},
            "body": {"created": true}
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"created": true}));
    }

    #[tokio::test]
    async fn test_a_status_key_that_is_not_a_status_code_stays_data() {
        async fn body_of(response: axum::response::Response) -> serde_json::Value {
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        // Data with a `status` field is returned whole.
        let data = serde_json::json!({"status": "ok", "count": 3});
        let response = build_webhook_response(Some(data.clone()), true).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, data);

        // On the shared host a plain value is sandboxed like any response.
        let response = build_webhook_response(Some(data.clone()), false).unwrap();
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_SECURITY_POLICY],
            "sandbox"
        );
        assert_eq!(
            response.headers()[axum::http::header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let response = build_webhook_response(Some(data.clone()), true).unwrap();
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::CONTENT_SECURITY_POLICY)
        );

        let out_of_range = serde_json::json!({"status": 7, "count": 3});
        let response = build_webhook_response(Some(out_of_range.clone()), true).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, out_of_range);

        // An untyped map with a status code is still a response.
        let untyped = serde_json::json!({"status": 201, "body": {"id": 1}});
        let response = build_webhook_response(Some(untyped), true).unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(body_of(response).await, serde_json::json!({"id": 1}));

        // So is a typed HttpResponse.
        let typed = serde_json::json!({
            "$type": "::hot::http/HttpResponse",
            "$val": {"status": 404, "body": "missing"}
        });
        let response = build_webhook_response(Some(typed), true).unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_of(response).await, serde_json::json!("missing"));
    }

    #[test]
    fn test_shared_host_responses_set_no_cookies_and_render_sandboxed() {
        let result = serde_json::json!({
            "status": 200,
            "headers": {
                "Content-Type": "text/html",
                "Set-Cookie": "session=attacker; Domain=hot.dev",
                "Content-Security-Policy": "script-src *",
                "X-Custom": "kept"
            },
            "body": "<form method=post></form>"
        });
        let shared = build_webhook_response(Some(result.clone()), false).unwrap();
        let headers = shared.headers();
        assert!(headers.get(axum::http::header::SET_COOKIE).is_none());
        assert_eq!(
            headers[axum::http::header::CONTENT_SECURITY_POLICY],
            "sandbox"
        );
        assert_eq!(
            headers[axum::http::header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(headers["x-custom"], "kept");

        let custom = build_webhook_response(Some(result), true).unwrap();
        let headers = custom.headers();
        assert_eq!(
            headers[axum::http::header::SET_COOKIE],
            "session=attacker; Domain=hot.dev"
        );
        assert_eq!(
            headers[axum::http::header::CONTENT_SECURITY_POLICY],
            "script-src *"
        );
    }

    #[tokio::test]
    async fn test_build_response_typed_http_response() {
        // Hot typed format: {"$type": "::hot::http/HttpResponse", "$val": {...}}
        let result = serde_json::json!({
            "$type": "::hot::http/HttpResponse",
            "$val": {
                "status": 201,
                "headers": {},
                "body": {"created": true}
            }
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"created": true}));
    }

    #[tokio::test]
    async fn test_build_response_short_typed_http_response() {
        // Short $type format (without namespace)
        let result = serde_json::json!({
            "$type": "HttpResponse",
            "$val": {
                "status": 200,
                "body": "ok"
            }
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!("ok"));
    }

    #[tokio::test]
    async fn test_build_response_http_response_with_custom_headers() {
        let result = serde_json::json!({
            "$type": "::hot::http/HttpResponse",
            "$val": {
                "status": 200,
                "headers": {
                    "x-custom-header": "custom-value",
                    "x-request-id": "abc123"
                },
                "body": "ok"
            }
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-custom-header").unwrap(),
            "custom-value"
        );
        assert_eq!(response.headers().get("x-request-id").unwrap(), "abc123");
    }

    #[tokio::test]
    async fn test_build_response_typed_non_http_response_unwrapped() {
        // A typed value that's NOT HttpResponse should be unwrapped from $val
        let result = serde_json::json!({
            "$type": "::myapp/MyResult",
            "$val": {"data": "hello"}
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"data": "hello"}));
    }

    #[tokio::test]
    async fn test_build_response_http_response_404() {
        let result = serde_json::json!({
            "status": 404,
            "body": {"error": "not found"}
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_build_response_http_response_no_body() {
        let result = serde_json::json!({
            "status": 204
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(
            body.is_empty(),
            "RFC 9110: 204 No Content must have empty body"
        );

        let ok_no_body = serde_json::json!({
            "status": 200
        });
        let response = build_webhook_response(Some(ok_no_body), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!(null));
    }

    #[tokio::test]
    async fn test_build_response_string_value() {
        let result = serde_json::json!("plain text response");
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!("plain text response"));
    }

    #[tokio::test]
    async fn test_build_response_array_value() {
        let result = serde_json::json!([1, 2, 3]);
        let response = build_webhook_response(Some(result.clone()), false).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, result);
    }

    #[tokio::test]
    async fn test_build_response_status_only_map_treated_as_http_response() {
        // A map with "status" key (but no $type) is still treated as an HttpResponse
        let result = serde_json::json!({
            "status": 202,
            "body": "accepted"
        });
        let response = build_webhook_response(Some(result), false).unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    // ========================================================================
    // Token extraction / validation tests
    // ========================================================================

    #[test]
    fn test_uuid_short_produces_12_hex_chars() {
        let id = uuid::Uuid::now_v7();
        let token = hot::db::webhook::uuid_short(&id);
        assert_eq!(token.len(), 12);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_validate_token_correct() {
        let id = uuid::Uuid::now_v7();
        let token = hot::db::webhook::uuid_short(&id);
        assert!(hot::db::webhook::validate_token(&id, &token));
    }

    #[test]
    fn test_validate_token_wrong() {
        let id = uuid::Uuid::now_v7();
        assert!(!hot::db::webhook::validate_token(&id, "000000000000"));
    }

    /// Helper: simulates the path-splitting logic from webhook_handler
    fn split_path_and_token(raw_path: &str) -> Option<(String, String)> {
        let normalized = raw_path.trim_end_matches('/');
        if let Some((prefix, last)) = normalized.rsplit_once('/') {
            if last.len() == 12 && last.chars().all(|c| c.is_ascii_hexdigit()) {
                let ep = if prefix.is_empty() {
                    "/".to_string()
                } else if prefix.starts_with('/') {
                    prefix.to_string()
                } else {
                    format!("/{}", prefix)
                };
                return Some((ep, last.to_string()));
            }
            None
        } else if normalized.len() == 12 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(("/".to_string(), normalized.to_string()))
        } else {
            None
        }
    }

    #[test]
    fn test_split_path_root_with_token() {
        // URL: /webhook/org/env/svc/abcdef123456  -> path="/", token="abcdef123456"
        let result = split_path_and_token("abcdef123456");
        assert_eq!(result, Some(("/".to_string(), "abcdef123456".to_string())));
    }

    #[test]
    fn test_split_path_with_subpath_and_token() {
        // URL: /webhook/org/env/svc/hook/abcdef123456
        let result = split_path_and_token("hook/abcdef123456");
        assert_eq!(
            result,
            Some(("/hook".to_string(), "abcdef123456".to_string()))
        );
    }

    #[test]
    fn test_split_path_deep_subpath_and_token() {
        // URL: /webhook/org/env/svc/a/b/c/abcdef123456
        let result = split_path_and_token("a/b/c/abcdef123456");
        assert_eq!(
            result,
            Some(("/a/b/c".to_string(), "abcdef123456".to_string()))
        );
    }

    #[test]
    fn test_split_path_trailing_slash_ignored() {
        let result = split_path_and_token("hook/abcdef123456/");
        assert_eq!(
            result,
            Some(("/hook".to_string(), "abcdef123456".to_string()))
        );
    }

    #[test]
    fn test_split_path_no_token_returns_none() {
        // No 12-char hex token at the end
        let result = split_path_and_token("hook/events");
        assert_eq!(result, None);
    }

    #[test]
    fn test_split_path_short_token_returns_none() {
        // Token too short (11 chars)
        let result = split_path_and_token("hook/abcdef12345");
        assert_eq!(result, None);
    }

    // ========================================================================
    // build_original_url tests
    // ========================================================================

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn test_original_url_from_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "api.hot.dev".parse().unwrap());
        headers.insert("host", "internal:8080".parse().unwrap());
        let url = build_original_url(
            &headers,
            &uri("/webhook/acme/prod/twilio/sms/abcdef123456?bodySHA256=aa"),
        );
        assert_eq!(
            url.as_deref(),
            Some("https://api.hot.dev/webhook/acme/prod/twilio/sms/abcdef123456?bodySHA256=aa")
        );
    }

    #[test]
    fn test_original_url_falls_back_to_host_header() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "hooks.example.com".parse().unwrap());
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456"));
        assert_eq!(
            url.as_deref(),
            Some("https://hooks.example.com/webhook/svc/abcdef123456")
        );
    }

    #[test]
    fn test_original_url_none_without_host() {
        let headers = HeaderMap::new();
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456"));
        assert_eq!(url, None);
    }

    #[test]
    fn test_original_url_takes_first_forwarded_value() {
        // Multiple proxies append comma-separated values; the first is the client-facing one.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https, http".parse().unwrap());
        headers.insert("x-forwarded-host", "api.hot.dev, edge-2".parse().unwrap());
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456"));
        assert_eq!(
            url.as_deref(),
            Some("https://api.hot.dev/webhook/svc/abcdef123456")
        );
    }

    #[test]
    fn test_original_url_strips_default_port() {
        // Proxies sometimes inject the default port; providers sign without it.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "api.hot.dev:443".parse().unwrap());
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456"));
        assert_eq!(
            url.as_deref(),
            Some("https://api.hot.dev/webhook/svc/abcdef123456")
        );

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        headers.insert("host", "localhost:80".parse().unwrap());
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456"));
        assert_eq!(
            url.as_deref(),
            Some("http://localhost/webhook/svc/abcdef123456")
        );
    }

    #[test]
    fn test_original_url_keeps_non_default_port() {
        // A URL configured with an explicit non-default port carries it in
        // the signed string, so reconstruction must keep it.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        headers.insert("host", "localhost:8080".parse().unwrap());
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456"));
        assert_eq!(
            url.as_deref(),
            Some("http://localhost:8080/webhook/svc/abcdef123456")
        );
    }

    #[test]
    fn test_original_url_preserves_query_verbatim() {
        // Percent-encoding and parameter order must survive untouched — the
        // provider signed this exact string.
        let mut headers = HeaderMap::new();
        headers.insert("host", "api.hot.dev".parse().unwrap());
        let url = build_original_url(&headers, &uri("/webhook/svc/abcdef123456?b=2&a=%2Fx%20y"));
        assert_eq!(
            url.as_deref(),
            Some("https://api.hot.dev/webhook/svc/abcdef123456?b=2&a=%2Fx%20y")
        );
    }

    #[test]
    fn test_original_url_rejects_malformed_and_untrusted_forwarding_headers() {
        let mut spoofed = HeaderMap::new();
        spoofed.insert("host", "api.hot.dev".parse().unwrap());
        spoofed.insert("x-forwarded-host", "evil.com?".parse().unwrap());
        spoofed.insert("x-forwarded-proto", "javascript".parse().unwrap());
        assert_eq!(
            build_original_url_with_trust(&spoofed, &uri("/webhook/svc/abcdef123456"), true),
            Some("https://api.hot.dev/webhook/svc/abcdef123456".to_string())
        );

        let mut untrusted_peer = HeaderMap::new();
        untrusted_peer.insert("host", "api.hot.dev".parse().unwrap());
        untrusted_peer.insert("x-forwarded-host", "attacker.example".parse().unwrap());
        untrusted_peer.insert("x-forwarded-proto", "http".parse().unwrap());
        assert_eq!(
            build_original_url_with_trust(
                &untrusted_peer,
                &uri("/webhook/svc/abcdef123456"),
                false
            ),
            Some("https://api.hot.dev/webhook/svc/abcdef123456".to_string())
        );
    }

    #[tokio::test]
    async fn test_webhook_response_blocks_framing_shared_headers_1xx_and_204_body() {
        let resp_204 = build_webhook_response(
            Some(serde_json::json!({
                "status": 204,
                "headers": {
                    "Content-Length": "999",
                    "Transfer-Encoding": "chunked",
                    "Clear-Site-Data": "\"cookies\"",
                    "Strict-Transport-Security": "max-age=0",
                    "Access-Control-Allow-Origin": "*",
                    "X-Safe": "ok"
                },
                "body": {"should": "not appear"}
            })),
            false,
        )
        .unwrap();
        assert_eq!(resp_204.status(), StatusCode::NO_CONTENT);
        assert!(resp_204.headers().get("clear-site-data").is_none());
        assert!(
            resp_204
                .headers()
                .get("strict-transport-security")
                .is_none()
        );
        assert!(
            resp_204
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        assert!(resp_204.headers().get("transfer-encoding").is_none());
        assert_eq!(resp_204.headers()["x-safe"], "ok");
        let bytes = axum::body::to_bytes(resp_204.into_body(), 1024)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "204 must have an empty body");

        let resp_101 = build_webhook_response(
            Some(serde_json::json!({
                "$type": "::hot::http/HttpResponse",
                "$val": {"status": 101, "body": "switching"}
            })),
            true,
        )
        .unwrap();
        assert_eq!(resp_101.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_split_path_long_token_returns_none() {
        // Token too long (13 chars)
        let result = split_path_and_token("hook/abcdef1234567");
        assert_eq!(result, None);
    }

    #[test]
    fn test_split_path_non_hex_token_returns_none() {
        // Contains non-hex chars
        let result = split_path_and_token("hook/abcdefghijkl");
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_webhook_catch_all_returns_404_without_domain_and_too_few_segments() {
        let db_conf = hot::val!({ "uri": "sqlite::memory:", "schema": "hot" });
        let db = hot::db::create_db_pool(&db_conf).await.unwrap();
        let conf = hot::val!({});
        let storage: Arc<Box<dyn hot::storage::BuildStorage>> =
            Arc::new(Box::new(MockBuildStorage));
        let state: crate::ApiStateData = (Arc::new(db), storage, Arc::new(conf), None);

        // Without ResolvedDomain, needs 4+ segments; 2 segments should 404
        let result = webhook_catch_all_handler(
            State(state),
            Method::POST,
            None,
            None,
            None,
            None,
            OriginalUri(uri("/webhook/svc/abcdef123456")),
            Path("svc/abcdef123456".to_string()),
            HeaderMap::new(),
            Query(std::collections::HashMap::new()),
            Bytes::new(),
        )
        .await;

        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_webhook_catch_all_domain_route_dispatches_correctly() {
        let db_conf = hot::val!({ "uri": "sqlite::memory:", "schema": "hot" });
        let db = hot::db::create_db_pool(&db_conf).await.unwrap();
        let conf = hot::val!({});
        let storage: Arc<Box<dyn hot::storage::BuildStorage>> =
            Arc::new(Box::new(MockBuildStorage));
        let state: crate::ApiStateData = (Arc::new(db), storage, Arc::new(conf), None);

        let resolved = ResolvedDomain {
            domain: "hooks.example.com".to_string(),
            env_id: Uuid::now_v7(),
            org_id: Uuid::now_v7(),
        };

        // With ResolvedDomain, 2 segments should dispatch (will 404 on DB lookup, not segment parse)
        let result = webhook_catch_all_handler(
            State(state),
            Method::POST,
            None,
            None,
            None,
            Some(Extension(resolved)),
            OriginalUri(uri("/webhook/svc/hook/abcdef123456")),
            Path("svc/hook/abcdef123456".to_string()),
            HeaderMap::new(),
            Query(std::collections::HashMap::new()),
            Bytes::new(),
        )
        .await;

        // Should fail on org/env lookup (not on segment parsing)
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[derive(Debug, Clone)]
    struct MockBuildStorage;

    #[async_trait::async_trait]
    impl hot::storage::BuildStorage for MockBuildStorage {
        async fn store_build(
            &self,
            _: &Uuid,
            _: &Uuid,
            _: &Uuid,
            _: Vec<u8>,
        ) -> Result<String, String> {
            Ok("mock://build".to_string())
        }
        async fn retrieve_build(&self, _: &Uuid, _: &Uuid, _: &Uuid) -> Result<Vec<u8>, String> {
            Ok(vec![])
        }
        async fn exists(&self, _: &Uuid, _: &Uuid, _: &Uuid) -> Result<bool, String> {
            Ok(false)
        }
        async fn delete_build(&self, _: &Uuid, _: &Uuid, _: &Uuid) -> Result<(), String> {
            Ok(())
        }
        fn build_path(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> String {
            format!("mock://{}/{}/{}", org_id, env_id, build_id)
        }
        fn storage_type(&self) -> &str {
            "mock"
        }
    }
}
