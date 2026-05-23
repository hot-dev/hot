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
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use hot::db::{build::Build, org::Org, project::Project, webhook::Webhook as DbWebhook};
use hot::permission::actions;
use hot::val::Val;
use once_cell::sync::OnceCell;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::ApiStateData;
use crate::auth::{AuthContext, authenticate_token};
use crate::domain_resolver::ResolvedDomain;
use crate::models::ApiErrorResponse;

use super::request::{
    RequestBody, build_call_event_data, build_request_val, hash_sensitive_request_fields,
};

/// The queue name that webhooks publish events to.
/// This MUST match the worker's event queue name ("hot:event").
const WEBHOOK_EVENT_QUEUE_NAME: &str = "hot:event";

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
        let queue_type_str = conf.get_str_or_default("queue.type", "memory");
        let queue_type = hot::queue::QueueType::from_str(&queue_type_str)
            .unwrap_or(hot::queue::QueueType::Memory);

        let redis_uri_str = conf.get_str_or_default("redis.uri", "");
        let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
            None
        } else {
            Some(redis_uri_str)
        };

        let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

        let serialization_str = conf.get_str_or_default("serialization.type", "zstd-json");
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

        tracing::info!(
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
pub async fn webhook_catch_all_handler(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    method: Method,
    resolved_domain: Option<Extension<ResolvedDomain>>,
    Path(full_path): Path<String>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
    body: Bytes,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
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
        method,
        path,
        headers,
        query,
        body,
        source,
    )
    .await
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
    method: Method,
    path: String,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
    body: Bytes,
    source: WebhookRouteSource,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
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
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Webhook endpoint")),
            ));
        }
    } else if normalized.len() == 12 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        ("/".to_string(), normalized.to_string())
    } else {
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
    let ctx = source.resolve(db, endpoint_path.clone()).await?;

    // Look up the webhook
    let endpoint = match DbWebhook::get_by_env_service_path_method(
        db,
        &ctx.env.env_id,
        &ctx.service,
        &ctx.endpoint_path,
        &method_str,
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

    // Validate the URL token matches this webhook's short ID
    if !hot::db::webhook::validate_token(&endpoint.webhook_id, &token) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Webhook endpoint")),
        ));
    }

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
        let authenticated = authenticate_token(db, token).await.map_err(|status| {
            (
                status,
                Json(ApiErrorResponse::new("unauthorized", "Invalid credentials")),
            )
        })?;

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

    // Build the unified HttpRequest value using the shared builder.
    // Includes method, url, headers, query, body, body-raw, ip, and auth (when authenticated).
    let raw_body_str = String::from_utf8_lossy(&body).to_string();
    let body_val: Val = serde_json::from_slice(&body).unwrap_or(Val::from(raw_body_str.clone()));

    let http_request = build_request_val(
        &method_str,
        &url_path,
        &headers,
        &query.0,
        Some(RequestBody {
            body: body_val,
            body_raw: raw_body_str,
        }),
        auth_result.as_ref(),
        &ctx.org.org_id,
    );

    // Execute the Hot function via the event queue
    let function_name = format!("{}/{}", endpoint.ns, endpoint.var);
    let event_id = Uuid::now_v7();
    let stream_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();

    // Fetch build and project info for execution context
    let (build_hash, project_id, project_name) =
        match Build::get_build(db, &endpoint.build_id).await {
            Ok(build) => {
                let project_name = Project::get_project(db, &build.project_id)
                    .await
                    .ok()
                    .map(|p| p.name);
                (Some(build.hash), Some(build.project_id), project_name)
            }
            Err(_) => (None, None, None),
        };

    // Get the default user for this env (for event attribution)
    let user_id = hot::db::User::get_default_user(db)
        .await
        .map(|u| u.user_id)
        .unwrap_or_else(|_| Uuid::nil());

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
        project_name,
        event_id: Some(event_id),
        origin_run_id: None,
        retry_attempt: 0,
        secret_keys: AHashSet::new(),
        secret_value_hashes: AHashSet::new(),
        access_id: None,
        agent_type: None,
    };

    // Pre-compute secret value hashes for targeted masking of sensitive headers
    // and the auth subtree in run logs.
    {
        let extra_secret_headers = endpoint.secret_headers();
        let sensitive_hashes = hash_sensitive_request_fields(&http_request, &extra_secret_headers);
        for h in sensitive_hashes {
            execution_context.secret_value_hashes.insert(h);
        }
    }

    // Build args: single argument (the HttpRequest).
    // The same HttpRequest is also passed as `caller` so the worker injects it
    // as the `hot.request` context variable.
    let args_val = Val::Vec(vec![http_request.clone()]);
    let event_data_val = build_call_event_data(&function_name, args_val, Some(http_request));
    let event_data_json =
        serde_json::to_value(event_data_val.to_hot_data_repr()).unwrap_or(serde_json::Value::Null);

    // Create call event for the worker queue
    let call_event = hot::lang::event::Event {
        event_id,
        env_id: ctx.env.env_id,
        stream_id,
        event_type: "hot:call".to_string(),
        event_data: event_data_val,
        event_time: chrono::Utc::now(),
        target_project_id: None,
        target_project_name: None,
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
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to create event: {}",
                e
            ))),
        ));
    }

    // Require pub/sub for webhook calls
    let pubsub = match stream_pubsub.as_ref() {
        Some(p) => p,
        None => {
            tracing::error!("Webhook handler requires stream pub/sub but none is configured");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(
                    "Stream pub/sub is not configured. Webhook calls require pub/sub for result delivery.",
                )),
            ));
        }
    };

    // Subscribe to pub/sub BEFORE enqueueing (prevents race condition)
    use hot::stream::StreamSubscriberFactory;
    let subscriber = pubsub.subscribe(stream_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to subscribe to stream: {}",
                e
            ))),
        )
    })?;

    // Enqueue the message
    use hot::queue::Queue;
    let queue = get_event_queue(conf).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e)),
        )
    })?;

    queue.enqueue(message).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to enqueue webhook call: {}",
                e
            ))),
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
    // Webhook timeout: 30 seconds (configurable, but Slack requires < 3s)
    let timeout_seconds = conf.get_int_or_default("webhook.timeout", 30) as u64;

    use hot::stream::StreamEvent;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_seconds);
    let mut subscriber = subscriber;

    enum Outcome {
        Stopped { result: Option<serde_json::Value> },
        Failed { error: Option<String> },
        Cancelled { reason: Option<String> },
    }
    let mut outcome: Option<Outcome> = None;
    let mut timed_out = false;

    loop {
        tokio::select! {
            event = async { subscriber.next().await } => {
                match event {
                    Some(StreamEvent::RunStop { result, .. }) => {
                        outcome = Some(Outcome::Stopped { result });
                        break;
                    }
                    Some(StreamEvent::RunFail { error, .. }) => {
                        outcome = Some(Outcome::Failed { error });
                        break;
                    }
                    Some(StreamEvent::RunCancel { reason, .. }) => {
                        outcome = Some(Outcome::Cancelled { reason });
                        break;
                    }
                    Some(_) => {
                        // Ignore other events (RunStart, StreamData, etc.)
                        continue;
                    }
                    None => {
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

    // Convert the outcome to an HTTP response
    match outcome {
        Some(Outcome::Stopped { result }) => build_webhook_response(result),
        Some(Outcome::Failed { error }) => {
            let error_text =
                error.unwrap_or_else(|| "Webhook function execution failed".to_string());
            tracing::warn!("Webhook function failed: {}", error_text);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Webhook function execution failed"})),
            )
                .into_response())
        }
        Some(Outcome::Cancelled { reason }) => {
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
fn build_webhook_response(
    result: Option<serde_json::Value>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    let result = match result {
        Some(r) => r,
        None => {
            return Ok((StatusCode::OK, Json(serde_json::json!(null))).into_response());
        }
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

    // Check if the result looks like an HttpResponse (has "status" field)
    if let serde_json::Value::Object(ref map) = unwrapped
        && map.contains_key("status")
    {
        let status_code = map
            .get("status")
            .and_then(|v| v.as_u64())
            .map(|s| s as u16)
            .unwrap_or(200);

        let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);

        let body = map.get("body").cloned().unwrap_or(serde_json::Value::Null);

        // Build response with custom headers if provided
        let mut response = Json(body).into_response();
        *response.status_mut() = status;

        if let Some(serde_json::Value::Object(headers)) = map.get("headers") {
            for (key, value) in headers {
                if let Some(v) = value.as_str()
                    && let (Ok(header_name), Ok(header_value)) = (
                        axum::http::HeaderName::from_str(key),
                        axum::http::HeaderValue::from_str(v),
                    )
                {
                    response.headers_mut().insert(header_name, header_value);
                }
            }
        }

        return Ok(response);
    }

    // Not an HttpResponse -- return as plain 200 JSON
    // Also unwrap $type/$val for other typed returns (clean JSON for the caller)
    if let serde_json::Value::Object(ref map) = result
        && map.contains_key("$type")
        && map.contains_key("$val")
    {
        return Ok((
            StatusCode::OK,
            Json(map.get("$val").cloned().unwrap_or(result)),
        )
            .into_response());
    }
    Ok((StatusCode::OK, Json(result)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

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
        let response = build_webhook_response(None).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!(null));
    }

    #[tokio::test]
    async fn test_build_response_plain_value_returns_200_json() {
        let result = serde_json::json!({"message": "hello"});
        let response = build_webhook_response(Some(result.clone())).unwrap();
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
        let response = build_webhook_response(Some(result)).unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({"created": true}));
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
        let response = build_webhook_response(Some(result)).unwrap();
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
        let response = build_webhook_response(Some(result)).unwrap();
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
        let response = build_webhook_response(Some(result)).unwrap();
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
        let response = build_webhook_response(Some(result)).unwrap();
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
        let response = build_webhook_response(Some(result)).unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_build_response_http_response_no_body() {
        let result = serde_json::json!({
            "status": 204
        });
        let response = build_webhook_response(Some(result)).unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!(null));
    }

    #[tokio::test]
    async fn test_build_response_string_value() {
        let result = serde_json::json!("plain text response");
        let response = build_webhook_response(Some(result)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!("plain text response"));
    }

    #[tokio::test]
    async fn test_build_response_array_value() {
        let result = serde_json::json!([1, 2, 3]);
        let response = build_webhook_response(Some(result.clone())).unwrap();
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
        let response = build_webhook_response(Some(result)).unwrap();
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
            Some(Extension(resolved)),
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
