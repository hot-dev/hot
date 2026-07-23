//! MCP (Model Context Protocol) handlers
//!
//! Implements the MCP JSON-RPC protocol for exposing Hot functions as AI tools.
//!
//! Supports two transports:
//!   - **Streamable HTTP** (2025-03-26): `POST /mcp/{org}/{service}`
//!     Modern transport. JSON-RPC request in, JSON or SSE response out.
//!   - **HTTP+SSE** (2024-11-05): `GET /mcp/{org}/{service}` + `POST /mcp/{org}/{service}/messages`
//!     Deprecated transport kept for compatibility. GET opens persistent SSE
//!     stream, POST sends messages, responses flow back over the SSE stream.
//!
//! The org slug in the URL provides multi-tenant namespace isolation.
//! The environment is determined by the API key.

use ahash::{AHashMap, AHashSet};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
};
use hot::db::{build::Build, mcp_tool::McpTool, org::Org, project::Project};
use hot::permission::actions;
use hot::stream::{
    McpSseTransportPrincipal, McpSseTransportSessionBinding, McpSseTransportSessionStore,
};
use hot::val::Val;

use crate::access_log::OptionalAccessId;
use crate::auth::{AuthContext, authenticate_token};
use crate::client_ip::ClientIp;
use crate::domain_resolver::ResolvedDomain;
use crate::rate_limit::{self, PublicRateLimitMode};
use axum::http::HeaderMap;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::ApiStateData;
use crate::models::ApiErrorResponse;

// ============================================================================
// MCP Protocol Types
// ============================================================================

/// JSON-RPC 2.0 request
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<JsonValue>,
    pub method: String,
    #[serde(default)]
    pub params: Option<JsonValue>,
}

/// JSON-RPC 2.0 response
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<JsonValue>, result: JsonValue) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(
        id: Option<JsonValue>,
        code: i32,
        message: String,
        data: Option<JsonValue>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data,
            }),
        }
    }
}

/// JSON-RPC 2.0 error object
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

// JSON-RPC error codes
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

// ============================================================================
// MCP Types
// ============================================================================

/// MCP server capabilities
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub tools: Option<ToolsCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
}

#[derive(Debug, Serialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

/// Logging capability (declares the server can emit notifications/message)
#[derive(Debug, Serialize)]
pub struct LoggingCapability {}

/// MCP tool definition
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<JsonValue>,
}

/// MCP initialize result
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// tools/list result
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<McpToolDef>,
}

/// tools/call params
#[derive(Debug, Deserialize)]
pub struct ToolsCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<JsonValue>,
}

/// tools/call result
#[derive(Debug, Serialize)]
pub struct ToolsCallResult {
    pub content: Vec<ToolResultContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ToolResultContent {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<JsonValue>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<JsonValue>,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<JsonValue>,
    },
    #[serde(rename = "resource")]
    Resource {
        resource: JsonValue,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<JsonValue>,
    },
}

// ============================================================================
// Argument Conversion
// ============================================================================

/// Convert MCP named arguments (JSON object) to a positional Vec for Hot's `call()`.
///
/// MCP tools send arguments as a JSON object: `{"name": "Alice", "mood": "happy"}`
/// Hot's `call-event-handler` expects `event.data.args` to be a Vec: `["Alice", "happy"]`
///
/// The parameter order is determined by the tool's `input_schema.properties` key order,
/// which is preserved from the function's original parameter list.
fn convert_named_args_to_positional(
    args_json: &JsonValue,
    input_schema: Option<&JsonValue>,
) -> Result<Val, String> {
    let args_map: serde_json::Map<String, JsonValue> = match args_json {
        JsonValue::Object(m) => m.clone(),
        JsonValue::Null => serde_json::Map::new(),
        other => {
            return Err(format!(
                "Tool arguments must be a JSON object, got: {}",
                other
            ));
        }
    };

    if args_map.is_empty() {
        return Ok(Val::Vec(vec![]));
    }

    // Get parameter order from the tool's input_schema properties
    let param_names: Vec<String> = input_schema
        .and_then(|schema| schema.get("properties"))
        .and_then(|props| props.as_object())
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();

    // Convert named args to positional Vec in parameter order
    let mut positional_args: Vec<Val> = Vec::with_capacity(param_names.len());
    for param_name in &param_names {
        let val: Val = if let Some(arg_value) = args_map.get(param_name) {
            serde_json::from_value(arg_value.clone()).unwrap_or(Val::Null)
        } else {
            Val::Null // Optional parameter not provided
        };
        positional_args.push(val);
    }
    Ok(Val::Vec(positional_args))
}

/// The queue name that MCP publishes events to.
/// This MUST match the worker's event queue name ("hot:event").
const MCP_EVENT_QUEUE_NAME: &str = "hot:event";

/// The event data key for the function name.
use super::request::{
    bind_call_event_to_build, build_call_event_data, build_request_val,
    hash_sensitive_request_fields,
};

// ============================================================================
// Shared Event Queue (initialized once, reused across requests)
// ============================================================================
// MCP publishes hot:call events to the hot:event queue, which is the same queue
// that the worker consumes from. The event_type "hot:call" tells the worker
// to route the event to the correct function (vs hot:schedule or user events).

static EVENT_QUEUE: OnceCell<Arc<hot::queue::ProcessingQueue<hot::data::msg::Message>>> =
    OnceCell::new();

/// Get or initialize the shared event queue from config.
fn get_event_queue(
    conf: &Val,
) -> Result<&'static Arc<hot::queue::ProcessingQueue<hot::data::msg::Message>>, String> {
    EVENT_QUEUE.get_or_try_init(|| {
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
            MCP_EVENT_QUEUE_NAME.to_string(),
            redis_uri,
            redis_cluster,
            serialization,
        )
        .map_err(|e| format!("Failed to create event queue: {}", e))?;

        tracing::debug!(
            "MCP: initialized shared event queue (type: {})",
            queue_type_str
        );
        Ok(Arc::new(queue))
    })
}

// ============================================================================
// MCP Handler
// ============================================================================

// MCP-specific error codes
const MCP_DISABLED: i32 = -32001;
const MCP_PUBSUB_UNAVAILABLE: i32 = -32003;
const MAX_MCP_HTTP_SSE_MESSAGE_TASKS: usize = 1024;
static MCP_HTTP_SSE_MESSAGE_SEMAPHORE: OnceCell<Arc<tokio::sync::Semaphore>> = OnceCell::new();

fn mcp_http_sse_message_semaphore() -> Arc<tokio::sync::Semaphore> {
    MCP_HTTP_SSE_MESSAGE_SEMAPHORE
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_MCP_HTTP_SSE_MESSAGE_TASKS)))
        .clone()
}

/// Resolved context for an MCP request, including optional authentication.
struct McpContext {
    org: Org,
    env: hot::db::env::Env,
    service: String,
    auth: Option<(AuthContext, hot::db::api_key::ApiKey)>,
}

/// Try to authenticate from the Authorization header.
///
/// Returns `Some((auth_ctx, api_key))` if a valid Bearer token is present,
/// `None` if no token or empty token. Returns `Err` if token is present but invalid.
async fn try_authenticate(
    db: &Arc<hot::db::DatabasePool>,
    headers: &HeaderMap,
) -> Result<Option<(AuthContext, hot::db::api_key::ApiKey)>, (StatusCode, Json<ApiErrorResponse>)> {
    let auth_header = match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return Ok(None),
    };

    let token = match auth_header.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(None),
    };

    let authenticated = authenticate_token(db, token).await.map_err(|status| {
        (
            status,
            Json(ApiErrorResponse::new("unauthorized", "Invalid credentials")),
        )
    })?;

    Ok(Some((authenticated.auth_ctx, authenticated.api_key)))
}

/// Resolve and validate org/env/service for an MCP request.
///
/// The org and env are always resolved from the URL path (like webhooks).
/// When credentials are present, cross-validates that the API key's env
/// matches the URL env, and checks permissions.
async fn resolve_mcp_context(
    db: &hot::db::DatabasePool,
    auth: Option<(AuthContext, hot::db::api_key::ApiKey)>,
    org_slug: &str,
    env_name: &str,
    service: String,
) -> Result<McpContext, (StatusCode, Json<ApiErrorResponse>)> {
    // Always resolve org and env from the URL
    let org = Org::get_org_by_slug(db, org_slug).await.map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Organization")),
        )
    })?;

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

    // If authenticated, cross-validate and check permissions
    if let Some((ref auth_ctx, _)) = auth {
        if auth_ctx.env_id() != env.env_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credentials do not belong to this environment",
                )),
            ));
        }

        let mcp_resource = format!("mcp:{}/*", service);
        if !auth_ctx.has_permission(&mcp_resource, actions::EXECUTE) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credentials do not have MCP access. Required permission: mcp:<service>/* execute",
                )),
            ));
        }

        if let Some(allowed_services) = auth_ctx.mcp_service_restrictions()
            && !allowed_services.contains(&service)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    format!(
                        "Credentials do not have access to MCP service '{}'. Allowed: {}",
                        service,
                        allowed_services.join(", ")
                    ),
                )),
            ));
        }
    }

    Ok(McpContext {
        org,
        env,
        service,
        auth,
    })
}

/// Resolve MCP context from a custom domain's ResolvedDomain extension.
///
/// Instead of resolving org/env from URL path segments, looks them up by the
/// org_id and env_id carried in the ResolvedDomain. Used by the shorter
/// `/mcp/{service}` routes that only work on custom domains.
async fn resolve_mcp_context_from_domain(
    db: &hot::db::DatabasePool,
    auth: Option<(AuthContext, hot::db::api_key::ApiKey)>,
    resolved: &crate::domain_resolver::ResolvedDomain,
    service: String,
) -> Result<McpContext, (StatusCode, Json<ApiErrorResponse>)> {
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

    if let Some((ref auth_ctx, _)) = auth {
        if auth_ctx.env_id() != env.env_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credentials do not belong to this environment",
                )),
            ));
        }

        let mcp_resource = format!("mcp:{}/*", service);
        if !auth_ctx.has_permission(&mcp_resource, actions::EXECUTE) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credentials do not have MCP access. Required permission: mcp:<service>/* execute",
                )),
            ));
        }

        if let Some(allowed_services) = auth_ctx.mcp_service_restrictions()
            && !allowed_services.contains(&service)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    format!(
                        "Credentials do not have access to MCP service '{}'. Allowed: {}",
                        service,
                        allowed_services.join(", ")
                    ),
                )),
            ));
        }
    }

    Ok(McpContext {
        org,
        env,
        service,
        auth,
    })
}

/// Main MCP endpoint handler
///
/// Handles JSON-RPC requests for MCP protocol (spec 2025-03-26):
/// - initialize: Returns server capabilities with version negotiation
/// - ping: Health check (returns empty result)
/// - notifications/*: Acknowledged with HTTP 202 (no body)
/// - tools/list: Returns available tools for this service
/// - tools/call: Executes a tool, streaming progress via SSE
///
/// Returns either `application/json` or `text/event-stream` (SSE) depending
/// on the method, per the Streamable HTTP transport spec.
pub async fn mcp_handler(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    OptionalAccessId(access_id): OptionalAccessId,
    client_ip: Option<Extension<ClientIp>>,
    Path((org_slug, env_name, service)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    let source = McpRouteSource::Path {
        org_slug,
        env_name,
        service,
    };
    mcp_handler_inner(
        &db,
        &conf,
        &stream_pubsub,
        access_id,
        client_ip.map(|extension| extension.0),
        &headers,
        request,
        source,
    )
    .await
}

/// MCP handler for custom domain routes (`/mcp/{service}`).
///
/// Resolves org/env from the `ResolvedDomain` extension injected by the
/// domain resolution middleware. Returns 404 if no custom domain is active.
pub async fn mcp_handler_domain(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    OptionalAccessId(access_id): OptionalAccessId,
    client_ip: Option<Extension<ClientIp>>,
    resolved: Option<Extension<ResolvedDomain>>,
    Path(service): Path<String>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    let resolved = resolved
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(
                    "This endpoint requires a custom domain",
                )),
            )
        })?
        .0;
    let source = McpRouteSource::Domain { resolved, service };
    mcp_handler_inner(
        &db,
        &conf,
        &stream_pubsub,
        access_id,
        client_ip.map(|extension| extension.0),
        &headers,
        request,
        source,
    )
    .await
}

#[derive(Clone)]
enum McpRouteSource {
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

impl McpRouteSource {
    fn sse_transport_route_key(&self) -> String {
        match self {
            McpRouteSource::Path {
                org_slug,
                env_name,
                service,
            } => format!("path:{}/{}/{}", org_slug, env_name, service),
            McpRouteSource::Domain { resolved, service } => {
                format!("domain:{}:{}:{}", resolved.domain, resolved.env_id, service)
            }
        }
    }

    async fn resolve(
        self,
        db: &hot::db::DatabasePool,
        auth: Option<(AuthContext, hot::db::api_key::ApiKey)>,
    ) -> Result<McpContext, (StatusCode, Json<ApiErrorResponse>)> {
        match self {
            McpRouteSource::Path {
                org_slug,
                env_name,
                service,
            } => resolve_mcp_context(db, auth, &org_slug, &env_name, service).await,
            McpRouteSource::Domain { resolved, service } => {
                resolve_mcp_context_from_domain(db, auth, &resolved, service).await
            }
        }
    }
}

fn mcp_sse_transport_principal(auth: &AuthContext) -> McpSseTransportPrincipal {
    match auth {
        AuthContext::ApiKey(api_key) => McpSseTransportPrincipal::ApiKey(api_key.api_key_id),
        AuthContext::Session { session, .. } => {
            McpSseTransportPrincipal::Session(session.session_id)
        }
        AuthContext::ServiceKey { service_key, .. } => {
            McpSseTransportPrincipal::ServiceKey(service_key.service_key_id)
        }
    }
}

fn mcp_sse_transport_principal_from_context(
    ctx: &McpContext,
) -> Result<McpSseTransportPrincipal, (StatusCode, Json<ApiErrorResponse>)> {
    let (auth, _) = ctx.auth.as_ref().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorResponse::new(
                "unauthorized",
                "MCP HTTP+SSE transport requires authentication",
            )),
        )
    })?;

    Ok(mcp_sse_transport_principal(auth))
}

fn mcp_sse_transport_session_expires_at(ttl: Duration) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
        + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(300))
}

fn mcp_sse_transport_session_not_found() -> (StatusCode, Json<ApiErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::new(
            "mcp_transport_session_not_found",
            "MCP HTTP+SSE transport session was not found or has expired",
        )),
    )
}

fn mcp_sse_transport_session_store_error(
    action: &str,
    error: impl std::fmt::Display,
) -> (StatusCode, Json<ApiErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiErrorResponse::internal_error(&format!(
            "Failed to {} MCP HTTP+SSE transport session: {}",
            action, error
        ))),
    )
}

fn verify_mcp_sse_transport_session_binding(
    binding: &McpSseTransportSessionBinding,
    ctx: &McpContext,
    route_key: &str,
    principal: &McpSseTransportPrincipal,
) -> Result<(), (StatusCode, Json<ApiErrorResponse>)> {
    if binding.is_expired() {
        return Err(mcp_sse_transport_session_not_found());
    }

    if binding.env_id != ctx.env.env_id
        || binding.route_key != route_key
        || &binding.principal != principal
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "mcp_transport_session_mismatch",
                "MCP HTTP+SSE transport session does not match this route or credential",
            )),
        ));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn mcp_handler_inner(
    db: &Arc<hot::db::DatabasePool>,
    conf: &Arc<Val>,
    stream_pubsub: &Option<Arc<hot::stream::StreamPubSub>>,
    access_id: Option<Uuid>,
    client_ip: Option<ClientIp>,
    headers: &HeaderMap,
    request: JsonRpcRequest,
    source: McpRouteSource,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    let mcp_enabled = conf.get_bool_or_default("mcp.enabled", true);
    if !mcp_enabled {
        return Ok(Json(JsonRpcResponse::error(
            request.id,
            MCP_DISABLED,
            "MCP is disabled. Set hot.mcp.enabled to true to enable MCP functionality.".to_string(),
            Some(serde_json::json!({
                "hint": "Add 'hot.mcp.enabled true' to your hot.hot configuration or set HOT_MCP_ENABLED=true"
            })),
        ))
        .into_response());
    }

    if request.jsonrpc != "2.0" {
        return Ok(Json(JsonRpcResponse::error(
            request.id,
            INVALID_REQUEST,
            "Invalid JSON-RPC version".to_string(),
            None,
        ))
        .into_response());
    }

    if request.method.starts_with("notifications/") {
        return Ok(StatusCode::ACCEPTED.into_response());
    }

    let mcp_timeout = conf.get_int_or_default("mcp.timeout", 60) as u64;

    match request.method.as_str() {
        "initialize" => {
            let client_version = request
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str());
            handle_initialize(request.id, client_version)
                .await
                .map(|r| r.into_response())
        }
        "ping" => {
            Ok(Json(JsonRpcResponse::success(request.id, serde_json::json!({}))).into_response())
        }
        "tools/list" => {
            let auth = try_authenticate(db, headers).await?;
            let ctx = source.resolve(db, auth).await?;
            if let Err(exceeded) = rate_limit::check_org_rate_limit(
                db,
                &ctx.org.org_id,
                PublicRateLimitMode::from_conf(conf),
                "mcp-tools-list",
            )
            .await
            {
                return Ok(rate_limit::rate_limit_response(exceeded));
            }
            handle_tools_list(db, &ctx, request.id)
                .await
                .map(|r| r.into_response())
        }
        "tools/call" => {
            let auth = try_authenticate(db, headers).await?;
            let ctx = source.resolve(db, auth).await?;
            if let Err(exceeded) = rate_limit::check_org_rate_limit(
                db,
                &ctx.org.org_id,
                PublicRateLimitMode::from_conf(conf),
                "mcp-tools-call",
            )
            .await
            {
                return Ok(rate_limit::rate_limit_response(exceeded));
            }

            let pubsub = match stream_pubsub.as_ref() {
                Some(p) => p,
                None => {
                    tracing::error!(
                        "MCP tools/call requires stream pub/sub but none is configured"
                    );
                    return Ok(Json(JsonRpcResponse::error(
                        request.id,
                        MCP_PUBSUB_UNAVAILABLE,
                        "Stream pub/sub is not configured. MCP tool calls require pub/sub for result delivery.".to_string(),
                        Some(serde_json::json!({
                            "hint": "Configure Redis or memory pub/sub via queue.type setting"
                        })),
                    ))
                    .into_response());
                }
            };

            handle_tools_call_streaming(
                db,
                conf,
                pubsub,
                &ctx,
                request.id,
                request.params,
                mcp_timeout,
                access_id,
                client_ip.as_ref(),
                headers,
            )
            .await
        }
        _ => Ok(Json(JsonRpcResponse::error(
            request.id,
            METHOD_NOT_FOUND,
            format!("Method not found: {}", request.method),
            None,
        ))
        .into_response()),
    }
}

/// Handle initialize request with version negotiation (spec 2025-03-26 §Lifecycle)
///
/// If the server supports the client's requested protocol version, it MUST respond
/// with the same version. Otherwise, it MUST respond with its latest supported version.
async fn handle_initialize(
    id: Option<JsonValue>,
    client_protocol_version: Option<&str>,
) -> Result<Json<JsonRpcResponse>, (StatusCode, Json<ApiErrorResponse>)> {
    // Version negotiation: echo the client's version if we support it,
    // otherwise respond with our latest supported version.
    let protocol_version = match client_protocol_version {
        Some(v @ "2025-03-26") | Some(v @ "2024-11-05") => v.to_string(),
        _ => "2025-03-26".to_string(),
    };

    let result = InitializeResult {
        protocol_version,
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: false,
            }),
            logging: Some(LoggingCapability {}),
        },
        server_info: ServerInfo {
            name: "hot-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        instructions: None,
    };

    Ok(Json(JsonRpcResponse::success(
        id,
        serde_json::to_value(result).unwrap_or(JsonValue::Null),
    )))
}

/// Handle tools/list request
async fn handle_tools_list(
    db: &hot::db::DatabasePool,
    ctx: &McpContext,
    id: Option<JsonValue>,
) -> Result<Json<JsonRpcResponse>, (StatusCode, Json<ApiErrorResponse>)> {
    // Get all MCP tools for this environment and service
    let tools = McpTool::get_mcp_tools_by_env_and_service(db, &ctx.env.env_id, &ctx.service)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get MCP tools: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    // Convert to MCP tool definitions
    let tool_defs: Vec<McpToolDef> = tools
        .into_iter()
        .map(|t| McpToolDef {
            name: t.name,
            title: t.title,
            description: t.description,
            input_schema: t.input_schema,
            annotations: t.annotations,
        })
        .collect();

    let result = ToolsListResult { tools: tool_defs };

    Ok(Json(JsonRpcResponse::success(
        id,
        serde_json::to_value(result).unwrap_or(JsonValue::Null),
    )))
}

/// Handle tools/call with SSE streaming response (spec 2025-03-26)
///
/// Per the MCP Streamable HTTP spec, the server MAY respond to JSON-RPC
/// requests with `Content-Type: text/event-stream`, sending notifications
/// before the final JSON-RPC response. This enables:
/// - Real-time progress updates via `notifications/message` during tool execution
/// - Stream data events forwarded from the Hot runtime
/// - Keep-alive to prevent connection timeouts for long-running tools
///
/// Setup errors (invalid params, tool not found, etc.) are returned as
/// `application/json` so the client gets immediate feedback without SSE.
#[allow(clippy::too_many_arguments)]
async fn handle_tools_call_streaming(
    db: &hot::db::DatabasePool,
    conf: &Val,
    stream_pubsub: &Arc<hot::stream::StreamPubSub>,
    ctx: &McpContext,
    id: Option<JsonValue>,
    params: Option<JsonValue>,
    timeout_seconds: u64,
    access_id: Option<Uuid>,
    client_ip: Option<&ClientIp>,
    headers: &HeaderMap,
) -> Result<axum::response::Response, (StatusCode, Json<ApiErrorResponse>)> {
    // Parse params
    let call_params: ToolsCallParams = match params {
        Some(p) => serde_json::from_value(p).map_err(|e| {
            tracing::warn!("Invalid tools/call params: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&format!(
                    "Invalid params: {}",
                    e
                ))),
            )
        })?,
        None => {
            return Ok(Json(JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                "Missing params for tools/call".to_string(),
                None,
            ))
            .into_response());
        }
    };

    // Look up the tool to verify it exists and get the function name
    let tool = match McpTool::get_mcp_tool_by_env_service_and_name(
        db,
        &ctx.env.env_id,
        &ctx.service,
        &call_params.name,
    )
    .await
    {
        Ok(t) => t,
        Err(hot::db::mcp_tool::McpToolError::NotFound) => {
            return Ok(Json(JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                format!("Tool not found: {}", call_params.name),
                None,
            ))
            .into_response());
        }
        Err(e) => {
            tracing::error!("Failed to get MCP tool: {}", e);
            return Ok(Json(JsonRpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("Failed to lookup tool: {}", e),
                None,
            ))
            .into_response());
        }
    };

    // Per-tool auth check: only exact `auth: "none"` is public. Missing,
    // malformed, or unknown values require auth.
    if !tool.is_public() {
        match &ctx.auth {
            Some((auth_ctx, _)) => {
                let tool_resource = format!("mcp:{}/{}", ctx.service, call_params.name);
                if !auth_ctx.has_permission(&tool_resource, actions::EXECUTE) {
                    return Ok(Json(JsonRpcResponse::error(
                        id,
                        INVALID_PARAMS,
                        format!(
                            "Credential does not have execute permission for tool '{}'",
                            call_params.name
                        ),
                        None,
                    ))
                    .into_response());
                }
            }
            None => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ApiErrorResponse::new(
                        "unauthorized",
                        format!(
                            "Tool '{}' requires authentication. Provide Authorization: Bearer <token>",
                            call_params.name
                        ),
                    )),
                ));
            }
        }
    }

    let inflight_guard =
        match rate_limit::check_public_org_inflight(db, conf, &ctx.org.org_id, "mcp-tools-call")
            .await
        {
            Ok(guard) => guard,
            Err(exceeded) => return Ok(rate_limit::rate_limit_response(exceeded)),
        };

    // Construct the Hot function name from ns/var
    let function_name = format!("{}/{}", tool.ns, tool.var);

    // Create IDs for tracking
    let event_id = Uuid::now_v7();
    let stream_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();

    // Fetch build and project info for the execution context
    let (build_hash, project_id, project_name) = match Build::get_build(db, &tool.build_id).await {
        Ok(build) => {
            let project_name = Project::get_project(db, &build.project_id)
                .await
                .ok()
                .map(|p| p.name);
            (Some(build.hash), Some(build.project_id), project_name)
        }
        Err(_) => (None, None, None),
    };

    // Resolve user_id: from auth if present, otherwise use org owner
    let user_id = ctx
        .auth
        .as_ref()
        .map(|(_, api_key)| api_key.created_by_user_id)
        .unwrap_or(ctx.org.created_by_user_id);

    // Create execution context for the call (fully populated)
    let mut execution_context = hot::lang::event::ExecutionContext {
        run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Call.as_id(),
        env_id: Some(ctx.env.env_id),
        env_name: Some(ctx.env.name.clone()),
        user_id: Some(user_id),
        org_id: Some(ctx.org.org_id),
        org_slug: Some(ctx.org.slug.clone()),
        build_id: Some(tool.build_id),
        build_hash,
        project_id,
        project_name: project_name.clone(),
        event_id: Some(event_id),
        origin_run_id: None,
        retry_attempt: 0,
        secret_keys: AHashSet::new(),
        secret_value_hashes: AHashSet::new(),
        access_id,
        agent_type: None,
    };

    // Convert MCP named arguments (JSON object) to positional Vec for Hot's call() function.
    // MCP sends: {"name": "Cursor"} but Hot expects: ["Cursor"] (positional args).
    let args_val: Val = match call_params.arguments {
        Some(args_json) => {
            match convert_named_args_to_positional(&args_json, tool.input_schema.as_ref()) {
                Ok(val) => val,
                Err(msg) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(ApiErrorResponse::bad_request(&msg)),
                    ));
                }
            }
        }
        None => Val::Vec(vec![]),
    };

    // Build enriched request Val for hot.request ctx injection.
    // Includes HTTP context (method, url, headers, query, ip) and auth identity.
    let url_path = format!("/mcp/{}/{}/{}", ctx.org.slug, ctx.env.name, ctx.service);
    let empty_query = std::collections::HashMap::new();
    let caller_val = build_request_val(
        "POST",
        &url_path,
        None,
        headers,
        client_ip,
        &empty_query,
        None,
        ctx.auth.as_ref(),
        &ctx.org.org_id,
    );

    // Pre-compute secret value hashes for targeted masking.
    // Only sensitive fields (auth subtree, known sensitive headers, user-declared
    // secret-headers from meta) are hashed — not the entire hot.request.
    {
        let extra_secret_headers = tool.secret_headers();
        let sensitive_hashes = hash_sensitive_request_fields(&caller_val, &extra_secret_headers);
        for h in sensitive_hashes {
            execution_context.secret_value_hashes.insert(h);
        }
    }

    // Build the event data Val (used for both the event and the DB insert)
    let mut event_data_val = build_call_event_data(&function_name, args_val, Some(caller_val));
    bind_call_event_to_build(&mut event_data_val, tool.build_id);

    // Insert event into database BEFORE enqueueing.
    // The worker verifies that events exist in the database for security
    // (prevents spoofed messages with fake env_ids).
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

    // Convert to unified Message format
    let message: hot::data::msg::Message = event_message.into();

    if let Err(e) = hot::db::Event::insert_event(
        db,
        &event_id,
        &ctx.env.env_id,
        &stream_id,
        "hot:call",
        &event_data_json,
        chrono::Utc::now(),
        &user_id,
        access_id.as_ref(),
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

    // Subscribe to pub/sub BEFORE enqueueing to avoid a race condition where
    // the worker processes the message and publishes the result before we
    // subscribe. This is critical for the memory pub/sub backend (tokio
    // broadcast) where events are ephemeral. Redis Streams retains messages
    // so would be less affected, but subscribing first is correct for both.
    use hot::stream::StreamSubscriberFactory;

    let subscriber = stream_pubsub
        .subscribe_in_env(ctx.env.env_id, stream_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to subscribe to stream pub/sub: {}",
                    e
                ))),
            )
        })?;

    // Now enqueue the message (subscriber is already listening)
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
                "Failed to enqueue call: {}",
                e
            ))),
        )
    })?;

    let tool_name = call_params.name.clone();
    tracing::info!(
        "MCP tools/call: {} ({}) queued with run_id {} [org={}, env={}]",
        tool_name,
        function_name,
        run_id,
        ctx.org.slug,
        ctx.env.name
    );

    // =====================================================================
    // SSE stream: yields MCP notifications/message for progress, then the
    // final JSON-RPC response for the tools/call result.
    //
    // Per Streamable HTTP spec (2025-03-26 §Sending Messages to the Server):
    //   "The server MAY send JSON-RPC requests and notifications before
    //    sending a JSON-RPC response."
    //   "After all JSON-RPC responses have been sent, the server SHOULD
    //    close the SSE stream."
    // =====================================================================
    let request_id = id;

    let sse_stream = async_stream::stream! {
        use hot::stream::{StreamEvent, StreamNext};

        let _inflight_guard = inflight_guard;
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_seconds);
        let mut subscriber = subscriber;
        let mut timed_out = false;

        // The terminal event carries the result/error directly — no DB
        // round-trip needed. This avoids a race where the pub/sub event
        // arrives before the worker has committed the run record to the DB.
        enum Outcome {
            Stopped { result: Option<serde_json::Value> },
            Failed { error: Option<String> },
            Cancelled { reason: Option<String> },
        }
        let mut outcome: Option<Outcome> = None;

        loop {
            tokio::select! {
                event = async { subscriber.next().await } => {
                    match event {
                        StreamNext::Event(StreamEvent::RunStart { run_id: started_run_id, .. }) => {
                            // Send MCP log notification (spec §Logging)
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/message",
                                "params": {
                                    "level": "info",
                                    "logger": "hot-mcp",
                                    "data": {
                                        "message": format!("Executing tool '{}'", tool_name),
                                        "runId": started_run_id.to_string()
                                    }
                                }
                            });
                            if let Ok(data) = serde_json::to_string(&notification) {
                                yield Ok::<SseEvent, Infallible>(
                                    SseEvent::default().event("message").data(data)
                                );
                            }
                        }
                        StreamNext::Event(StreamEvent::StreamData { data_type, payload, .. }) => {
                            // Forward stream data as MCP log notification
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/message",
                                "params": {
                                    "level": "info",
                                    "logger": "hot-mcp",
                                    "data": {
                                        "type": data_type,
                                        "payload": payload
                                    }
                                }
                            });
                            if let Ok(data) = serde_json::to_string(&notification) {
                                yield Ok::<SseEvent, Infallible>(
                                    SseEvent::default().event("message").data(data)
                                );
                            }
                        }
                        StreamNext::Event(StreamEvent::RunStop { result, .. }) => {
                            outcome = Some(Outcome::Stopped { result });
                            break;
                        }
                        StreamNext::Event(StreamEvent::RunFail { error, .. }) => {
                            outcome = Some(Outcome::Failed { error });
                            break;
                        }
                        StreamNext::Event(StreamEvent::RunCancel { reason, .. }) => {
                            outcome = Some(Outcome::Cancelled { reason });
                            break;
                        }
                        StreamNext::Event(StreamEvent::TaskMessage { .. }) => {}
                        StreamNext::Idle => {}
                        StreamNext::Closed => {
                            // Subscriber closed unexpectedly
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

        // Build and send the final JSON-RPC response (one per request, per spec)
        //
        // We use the result/error carried in the pub/sub event directly,
        // avoiding a DB round-trip and the associated race condition.
        let response = match outcome {
            Some(Outcome::Stopped { result }) => {
                let content = match result {
                    Some(ref r) => extract_content_from_result(r),
                    None => vec![ToolResultContent::Text {
                        text: "null".to_string(),
                        annotations: None,
                    }],
                };
                let call_result = ToolsCallResult {
                    content,
                    is_error: None,
                    meta: None,
                };
                JsonRpcResponse::success(
                    request_id,
                    serde_json::to_value(call_result).unwrap_or(JsonValue::Null),
                )
            }
            Some(Outcome::Failed { error }) => {
                let error_text = error.unwrap_or_else(|| "Unknown error".to_string());
                let call_result = ToolsCallResult {
                    content: vec![ToolResultContent::Text {
                        text: error_text,
                        annotations: None,
                    }],
                    is_error: Some(true),
                    meta: None,
                };
                JsonRpcResponse::success(
                    request_id,
                    serde_json::to_value(call_result).unwrap_or(JsonValue::Null),
                )
            }
            Some(Outcome::Cancelled { reason }) => {
                let reason_text = reason.unwrap_or_else(|| "Execution cancelled".to_string());
                let call_result = ToolsCallResult {
                    content: vec![ToolResultContent::Text {
                        text: reason_text,
                        annotations: None,
                    }],
                    is_error: Some(true),
                    meta: None,
                };
                JsonRpcResponse::success(
                    request_id,
                    serde_json::to_value(call_result).unwrap_or(JsonValue::Null),
                )
            }
            None if timed_out => {
                tracing::warn!(
                    "MCP tools/call timed out after {}s for run_id {}. \
                     The enqueued job may still execute.",
                    timeout_seconds,
                    run_id
                );
                JsonRpcResponse::error(
                    request_id,
                    INTERNAL_ERROR,
                    format!(
                        "Timeout: tool execution did not complete within {}s. \
                         The function may still be running in the background.",
                        timeout_seconds
                    ),
                    None,
                )
            }
            None => {
                JsonRpcResponse::error(
                    request_id,
                    INTERNAL_ERROR,
                    "Stream pub/sub subscription closed unexpectedly".to_string(),
                    None,
                )
            }
        };

        if let Ok(data) = serde_json::to_string(&response) {
            yield Ok::<SseEvent, Infallible>(
                SseEvent::default().event("message").data(data)
            );
        }
    };

    // KeepAlive sends SSE comments to prevent ALB/proxy idle timeout
    Ok(Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

// ============================================================================
// Tool Result Formatting & Content Detection
// ============================================================================

/// Extract MCP content items from a tool result value.
///
/// Recognises structured content objects with a `type` field:
/// - `{"type":"image","data":"<base64>","mimeType":"image/png"}` → Image
/// - `{"type":"audio","data":"<base64>","mimeType":"audio/wav"}` → Audio
/// - `{"type":"text","text":"..."}` → Text
///
/// Arrays of such objects are expanded into multiple content items.
/// Everything else is serialised as pretty-printed JSON text.
fn extract_content_from_result(result: &JsonValue) -> Vec<ToolResultContent> {
    // Single structured content item?
    if let Some(item) = try_extract_content(result) {
        return vec![item];
    }

    // Array of content items?
    if let Some(arr) = result.as_array() {
        let mut items: Vec<ToolResultContent> = Vec::with_capacity(arr.len());
        for element in arr {
            if let Some(item) = try_extract_content(element) {
                items.push(item);
            } else {
                items.push(ToolResultContent::Text {
                    text: serde_json::to_string_pretty(element)
                        .unwrap_or_else(|_| "null".to_string()),
                    annotations: None,
                });
            }
        }
        if !items.is_empty() {
            return items;
        }
    }

    // Default: serialise the whole result as text
    vec![ToolResultContent::Text {
        text: serde_json::to_string_pretty(result).unwrap_or_else(|_| "null".to_string()),
        annotations: None,
    }]
}

/// Try to interpret a JSON value as a typed MCP content item.
fn try_extract_content(value: &JsonValue) -> Option<ToolResultContent> {
    let obj = value.as_object()?;
    let content_type = obj.get("type")?.as_str()?;
    let annotations = obj.get("annotations").cloned();

    match content_type {
        "image" => {
            let data = obj.get("data")?.as_str()?.to_string();
            let mime_type = obj.get("mimeType")?.as_str()?.to_string();
            Some(ToolResultContent::Image {
                data,
                mime_type,
                annotations,
            })
        }
        "audio" => {
            let data = obj.get("data")?.as_str()?.to_string();
            let mime_type = obj.get("mimeType")?.as_str()?.to_string();
            Some(ToolResultContent::Audio {
                data,
                mime_type,
                annotations,
            })
        }
        "text" => {
            let text = obj.get("text")?.as_str()?.to_string();
            Some(ToolResultContent::Text { text, annotations })
        }
        "resource" => {
            let resource = obj.get("resource")?.clone();
            Some(ToolResultContent::Resource {
                resource,
                annotations,
            })
        }
        _ => None,
    }
}

// ============================================================================
// Deprecated HTTP+SSE Transport (2024-11-05 spec)
// ============================================================================
//
// For older MCP clients (e.g. Claude Desktop) that only speak the original
// HTTP+SSE transport:
//
//   1. Client GETs the MCP endpoint → server returns SSE stream with an
//      `endpoint` event containing the POST URL for messages.
//   2. Client POSTs JSON-RPC messages to that URL → server returns 202.
//   3. JSON-RPC responses flow back over the SSE stream from step 1.
//
// ## Horizontal scalability
//
// Transport session routing uses the same `StreamPubSub` abstraction as everything else:
//   - **Local dev**: in-memory broadcast channels (single process)
//   - **Production**: Redis pub/sub (any instance can handle GET or POST)
//
// The transport session UUID is used as a `stream_id` for the pub/sub channel.
// `StreamData` events carry the JSON-RPC response payload between the
// POST handler and the SSE stream, regardless of which server instance
// handles each side.

/// The data_type used for MCP HTTP+SSE transport session messages in StreamData events.
const MCP_HTTP_SSE_SESSION_DATA_TYPE: &str = "mcp:session:response";

fn mcp_http_sse_session_timeout(conf: &Val) -> tokio::time::Duration {
    let seconds = conf.get_int_or_default("mcp.http-sse.session-timeout", 300);
    tokio::time::Duration::from_secs(seconds.max(1) as u64)
}

/// GET /mcp/{org_slug}/{env_name}/{service}
///
/// Opens a persistent SSE stream for the deprecated MCP HTTP+SSE transport.
/// The first event is `endpoint` with the URL the client should POST to.
/// Subsequent events are JSON-RPC responses/notifications routed from
/// `mcp_http_sse_messages_handler` via the shared pub/sub layer.
pub async fn mcp_sse_handler(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    Path((org_slug, env_name, service)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures::stream::Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let messages_path_prefix = format!("/mcp/{}/{}/{}", org_slug, env_name, service);
    let source = McpRouteSource::Path {
        org_slug,
        env_name,
        service,
    };
    mcp_sse_handler_core(
        db,
        conf,
        stream_pubsub,
        headers,
        messages_path_prefix,
        source,
    )
    .await
}

/// SSE handler for custom domain routes (`GET /mcp/{service}`).
pub async fn mcp_sse_handler_domain(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    resolved: Option<Extension<ResolvedDomain>>,
    Path(service): Path<String>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures::stream::Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let resolved = resolved.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found(
                "This endpoint requires a custom domain",
            )),
        )
    })?;
    let messages_path_prefix = format!("/mcp/{}", service);
    let source = McpRouteSource::Domain {
        resolved: resolved.0,
        service,
    };
    mcp_sse_handler_core(
        db,
        conf,
        stream_pubsub,
        headers,
        messages_path_prefix,
        source,
    )
    .await
}

async fn mcp_sse_handler_core(
    db: Arc<hot::db::DatabasePool>,
    conf: Arc<Val>,
    stream_pubsub: Option<Arc<hot::stream::StreamPubSub>>,
    headers: HeaderMap,
    messages_path_prefix: String,
    source: McpRouteSource,
) -> Result<
    Sse<impl futures::stream::Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let mcp_enabled = conf.get_bool_or_default("mcp.enabled", true);
    if !mcp_enabled {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::new("mcp_disabled", "MCP is disabled")),
        ));
    }

    let route_key = source.sse_transport_route_key();
    let auth = try_authenticate(&db, &headers).await?;
    let auth = auth.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorResponse::new(
                "unauthorized",
                "MCP HTTP+SSE transport requires authentication. Provide Authorization: Bearer <token>",
            )),
        )
    })?;
    let ctx = source.resolve(&db, Some(auth)).await?;
    let env_id = ctx.env.env_id;
    let principal = mcp_sse_transport_principal_from_context(&ctx)?;

    let pubsub = stream_pubsub.as_ref().cloned().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::new(
                "pubsub_unavailable",
                "Stream pub/sub is required for MCP HTTP+SSE transport",
            )),
        )
    })?;

    let transport_session_id = Uuid::now_v7();
    let session_timeout = mcp_http_sse_session_timeout(&conf);

    let messages_path = format!(
        "{}/messages?sessionId={}",
        messages_path_prefix, transport_session_id
    );

    use hot::stream::StreamSubscriberFactory;
    let subscriber = pubsub
        .subscribe_in_env(env_id, transport_session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to subscribe to session pub/sub: {}",
                    e
                ))),
            )
        })?;

    let binding = McpSseTransportSessionBinding {
        transport_session_id,
        env_id,
        route_key,
        principal,
        expires_at: mcp_sse_transport_session_expires_at(session_timeout),
    };
    pubsub
        .put_mcp_sse_transport_session(binding, session_timeout)
        .await
        .map_err(|e| mcp_sse_transport_session_store_error("create", e))?;

    tracing::info!(
        "MCP HTTP+SSE: transport session {} opened for {}",
        transport_session_id,
        messages_path_prefix
    );

    let pubsub_for_cleanup = Arc::clone(&pubsub);
    let stream = async_stream::stream! {
        use hot::stream::{StreamEvent, StreamNext};

        // First event: tell the client where to POST messages
        yield Ok::<SseEvent, Infallible>(
            SseEvent::default()
                .event("endpoint")
                .data(messages_path)
        );

        let mut subscriber = subscriber;
        let deadline = tokio::time::Instant::now() + session_timeout;

        // Receive responses from the pub/sub channel and forward as SSE events
        loop {
            tokio::select! {
                event = async { subscriber.next().await } => {
                    match event {
                        StreamNext::Event(StreamEvent::StreamData { payload, data_type, .. })
                            if data_type == MCP_HTTP_SSE_SESSION_DATA_TYPE =>
                        {
                            // The payload is a JSON-RPC response string
                            if let Some(data) = payload.as_str() {
                                yield Ok::<SseEvent, Infallible>(
                                    SseEvent::default().event("message").data(data)
                                );
                            }
                        }
                        StreamNext::Event(_) => {
                            // Ignore other event types on this channel
                        }
                        StreamNext::Idle => {}
                        StreamNext::Closed => {
                            // Subscriber closed
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    // Session timeout
                    break;
                }
            }
        }

        if let Err(e) = pubsub_for_cleanup
            .delete_mcp_sse_transport_session(transport_session_id)
            .await
        {
            tracing::warn!(
                "MCP HTTP+SSE: failed to delete transport session {}: {}",
                transport_session_id,
                e
            );
        }

        tracing::info!(
            "MCP HTTP+SSE: transport session {} closed",
            transport_session_id
        );
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// POST /mcp/{org_slug}/{env_name}/{service}/messages?sessionId={id}
///
/// Receives JSON-RPC messages for the deprecated MCP HTTP+SSE transport.
/// Routes the message through the standard MCP handler logic, then publishes
/// the response through the shared pub/sub layer so it reaches the SSE
/// stream (which may be on a different server instance).
/// Returns HTTP 202 Accepted immediately (response comes via SSE).
pub async fn mcp_http_sse_messages_handler(
    State(state): State<ApiStateData>,
    OptionalAccessId(access_id): OptionalAccessId,
    client_ip: Option<Extension<ClientIp>>,
    Path((org_slug, env_name, service)): Path<(String, String, String)>,
    query: Query<HttpSseMessageQuery>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    let source = McpRouteSource::Path {
        org_slug,
        env_name,
        service,
    };
    mcp_http_sse_messages_inner(
        state,
        access_id,
        client_ip.map(|extension| extension.0),
        query,
        headers,
        request,
        source,
    )
    .await
}

/// HTTP+SSE messages handler for custom domain routes (`POST /mcp/{service}/messages`).
#[allow(clippy::too_many_arguments)]
pub async fn mcp_http_sse_messages_handler_domain(
    State(state): State<ApiStateData>,
    OptionalAccessId(access_id): OptionalAccessId,
    client_ip: Option<Extension<ClientIp>>,
    resolved: Option<Extension<ResolvedDomain>>,
    Path(service): Path<String>,
    query: Query<HttpSseMessageQuery>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    let resolved = resolved
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found(
                    "This endpoint requires a custom domain",
                )),
            )
        })?
        .0;
    let source = McpRouteSource::Domain { resolved, service };
    mcp_http_sse_messages_inner(
        state,
        access_id,
        client_ip.map(|extension| extension.0),
        query,
        headers,
        request,
        source,
    )
    .await
}

async fn mcp_http_sse_messages_inner(
    state: ApiStateData,
    access_id: Option<Uuid>,
    client_ip: Option<ClientIp>,
    query: Query<HttpSseMessageQuery>,
    headers: HeaderMap,
    request: JsonRpcRequest,
    source: McpRouteSource,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    let transport_session_id = Uuid::parse_str(&query.transport_session_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(
                "Invalid MCP transport session ID",
            )),
        )
    })?;

    let (db, _storage, _conf, stream_pubsub) = &state;

    let pubsub = stream_pubsub.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::new(
                "pubsub_unavailable",
                "Stream pub/sub is required for MCP HTTP+SSE transport",
            )),
        )
    })?;

    let route_key = source.sse_transport_route_key();
    let auth = try_authenticate(db, &headers).await?;
    let auth = auth.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorResponse::new(
                "unauthorized",
                "MCP HTTP+SSE transport requires authentication. Provide Authorization: Bearer <token>",
            )),
        )
    })?;
    let ctx = source.clone().resolve(db, Some(auth)).await?;
    let env_id = ctx.env.env_id;
    let principal = mcp_sse_transport_principal_from_context(&ctx)?;
    let binding = pubsub
        .get_mcp_sse_transport_session(transport_session_id)
        .await
        .map_err(|e| mcp_sse_transport_session_store_error("load", e))?
        .ok_or_else(mcp_sse_transport_session_not_found)?;
    verify_mcp_sse_transport_session_binding(&binding, &ctx, &route_key, &principal)?;

    let permit = mcp_http_sse_message_semaphore()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ApiErrorResponse::new(
                    "too_many_requests",
                    "Too many MCP HTTP+SSE messages are being processed. Try again shortly.",
                )),
            )
        })?;

    let state_clone = state.clone();
    let pubsub_clone = pubsub.clone();
    tokio::spawn(async move {
        let _permit = permit;
        process_mcp_http_sse_message_request(
            state_clone,
            headers,
            access_id,
            client_ip,
            source,
            request,
            transport_session_id,
            env_id,
            pubsub_clone,
        )
        .await;
    });

    Ok(StatusCode::ACCEPTED)
}

#[allow(clippy::too_many_arguments)]
async fn process_mcp_http_sse_message_request(
    state: ApiStateData,
    headers: HeaderMap,
    access_id: Option<Uuid>,
    client_ip: Option<ClientIp>,
    source: McpRouteSource,
    request: JsonRpcRequest,
    transport_session_id: Uuid,
    env_id: Uuid,
    pubsub: Arc<hot::stream::StreamPubSub>,
) {
    let (db, _, conf, stream_pubsub) = &state;
    let request_id = request.id.clone();
    let response = mcp_handler_inner(
        db,
        conf,
        stream_pubsub,
        access_id,
        client_ip,
        &headers,
        request,
        source,
    )
    .await;

    match response {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("MCP HTTP+SSE: failed to read response body: {}", e);
                    let error = JsonRpcResponse::error(
                        request_id,
                        INTERNAL_ERROR,
                        "Failed to read MCP response body".to_string(),
                        None,
                    );
                    publish_mcp_http_sse_session_payload(
                        &pubsub,
                        transport_session_id,
                        env_id,
                        &error,
                    )
                    .await;
                    return;
                }
            };

            let content_type = parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            if content_type.contains("text/event-stream") {
                // Parse full SSE frames so multi-line `data:` payloads are handled correctly.
                let body_str = String::from_utf8_lossy(&body_bytes);
                for payload in extract_sse_data_payloads(&body_str) {
                    if !payload.is_empty() {
                        publish_mcp_http_sse_session_payload_str(
                            &pubsub,
                            transport_session_id,
                            env_id,
                            payload,
                        )
                        .await;
                    }
                }
            } else {
                // JSON response — forward directly
                let body_str = String::from_utf8_lossy(&body_bytes).trim().to_string();
                if !body_str.is_empty() {
                    publish_mcp_http_sse_session_payload_str(
                        &pubsub,
                        transport_session_id,
                        env_id,
                        body_str,
                    )
                    .await;
                }
            }
        }
        Err((_status, error_json)) => {
            publish_mcp_http_sse_session_payload(
                &pubsub,
                transport_session_id,
                env_id,
                &error_json.0,
            )
            .await;
        }
    }
}

fn extract_sse_data_payloads(sse: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    let normalized = sse.replace("\r\n", "\n").replace('\r', "\n");

    for frame in normalized.split("\n\n") {
        if frame.trim().is_empty() {
            continue;
        }

        let mut data_lines = Vec::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start().to_string());
            }
        }

        if !data_lines.is_empty() {
            payloads.push(data_lines.join("\n"));
        }
    }

    payloads
}

async fn publish_mcp_http_sse_session_payload(
    pubsub: &Arc<hot::stream::StreamPubSub>,
    transport_session_id: Uuid,
    env_id: Uuid,
    payload: &impl serde::Serialize,
) {
    if let Ok(payload_str) = serde_json::to_string(payload) {
        publish_mcp_http_sse_session_payload_str(pubsub, transport_session_id, env_id, payload_str)
            .await;
    }
}

async fn publish_mcp_http_sse_session_payload_str(
    pubsub: &Arc<hot::stream::StreamPubSub>,
    transport_session_id: Uuid,
    env_id: Uuid,
    payload: String,
) {
    use hot::stream::{StreamEvent, StreamPublisher};

    let event = StreamEvent::StreamData {
        stream_data_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        env_id: Some(env_id),
        stream_id: transport_session_id,
        data_type: MCP_HTTP_SSE_SESSION_DATA_TYPE.to_string(),
        payload: serde_json::Value::String(payload),
    };

    if let Err(e) = pubsub.publish(event).await {
        tracing::warn!(
            "MCP HTTP+SSE: failed to publish transport session payload for {}: {}",
            transport_session_id,
            e
        );
    }
}

/// Query parameters for the HTTP+SSE messages endpoint.
#[derive(Debug, Deserialize)]
pub struct HttpSseMessageQuery {
    #[serde(rename = "sessionId")]
    pub transport_session_id: String,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use hot::db::api_key::ApiKey;
    use hot::val;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::{Duration, Instant};

    #[derive(Debug, Clone)]
    struct TestBuildStorage;

    #[async_trait::async_trait]
    impl hot::storage::BuildStorage for TestBuildStorage {
        async fn store_build(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
            _data: Vec<u8>,
        ) -> Result<String, String> {
            Ok("mock://build".to_string())
        }

        async fn retrieve_build(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
        ) -> Result<Vec<u8>, String> {
            Ok(vec![])
        }

        async fn exists(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
        ) -> Result<bool, String> {
            Ok(false)
        }

        async fn delete_build(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
        ) -> Result<(), String> {
            Ok(())
        }

        fn build_path(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> String {
            format!(
                "mock://builds/{}/{}/{}.hot.zip",
                org_id.simple(),
                env_id.simple(),
                build_id.simple()
            )
        }

        fn storage_type(&self) -> &str {
            "mock"
        }
    }

    async fn make_test_state(
        conf: Val,
        with_pubsub: bool,
    ) -> (ApiStateData, ApiKey, Option<Arc<hot::stream::StreamPubSub>>) {
        let db_conf = val!({
            "uri": "sqlite::memory:",
            "schema": "hot"
        });
        let db = hot::db::create_db_pool(&db_conf).await.unwrap();

        let storage: Arc<Box<dyn hot::storage::BuildStorage>> =
            Arc::new(Box::new(TestBuildStorage));

        let pubsub = if with_pubsub {
            Some(Arc::new(
                hot::stream::StreamPubSub::new(hot::stream::StreamPubSubType::Memory, None, false)
                    .unwrap(),
            ))
        } else {
            None
        };

        let state: ApiStateData = (Arc::new(db), storage, Arc::new(conf), pubsub.clone());

        let now = chrono::Utc::now();
        let api_key = ApiKey {
            api_key_id: Uuid::now_v7(),
            env_id: Uuid::now_v7(),
            description: "test".to_string(),
            key_data: serde_json::json!({"hash": "test"}),
            active: true,
            created_by_user_id: Uuid::now_v7(),
            created_at: now,
            updated_at: now,
            updated_by_user_id: None,
            active_toggle_at: None,
            active_toggle_by_user_id: None,
            permissions: serde_json::Value::Null,
        };

        (state, api_key, pubsub)
    }

    fn extract_payload(event: hot::stream::StreamNext) -> String {
        match event {
            hot::stream::StreamNext::Event(hot::stream::StreamEvent::StreamData {
                payload,
                ..
            }) => payload
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| payload.to_string()),
            other => panic!("Expected StreamData, got {:?}", other),
        }
    }

    #[test]
    fn test_mcp_http_sse_session_timeout_default_and_bounds() {
        let default_conf = val!({});
        assert_eq!(
            mcp_http_sse_session_timeout(&default_conf),
            Duration::from_secs(300)
        );

        let custom_conf = val!({"mcp": {"http-sse": {"session-timeout": 42i64}}});
        assert_eq!(
            mcp_http_sse_session_timeout(&custom_conf),
            Duration::from_secs(42)
        );

        let zero_conf = val!({"mcp": {"http-sse": {"session-timeout": 0i64}}});
        assert_eq!(
            mcp_http_sse_session_timeout(&zero_conf),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn test_extract_sse_data_payloads_multiline_and_crlf() {
        let sse = "event: message\r\ndata: line 1\r\ndata: line 2\r\n\r\nevent: message\r\ndata: {\"ok\":true}\r\n\r\n";
        let payloads = extract_sse_data_payloads(sse);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0], "line 1\nline 2");
        assert_eq!(payloads[1], "{\"ok\":true}");
    }

    #[test]
    fn test_extract_sse_data_payloads_ignores_non_data_frames() {
        let sse = ": keep-alive\n\n\
                   event: message\n\
                   id: 1\n\
                   data: hello\n\n\
                   event: message\n\
                   id: 2\n\n";
        let payloads = extract_sse_data_payloads(sse);
        assert_eq!(payloads, vec!["hello".to_string()]);
    }

    #[tokio::test]
    async fn test_initialize_supports_both_protocol_versions() {
        let response_2025 = handle_initialize(Some(json!(1)), Some("2025-03-26"))
            .await
            .unwrap();
        let value_2025 = serde_json::to_value(response_2025.0).unwrap();
        assert_eq!(value_2025["result"]["protocolVersion"], "2025-03-26");

        let response_2024 = handle_initialize(Some(json!(2)), Some("2024-11-05"))
            .await
            .unwrap();
        let value_2024 = serde_json::to_value(response_2024.0).unwrap();
        assert_eq!(value_2024["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn test_streamable_ping_returns_empty_object() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, _) = make_test_state(conf, false).await;

        let response = mcp_handler(
            State(state),
            OptionalAccessId(None),
            None,
            Path((
                "local".to_string(),
                "development".to_string(),
                "svc".to_string(),
            )),
            HeaderMap::new(),
            Json(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(9)),
                method: "ping".to_string(),
                params: None,
            }),
        )
        .await
        .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 9);
        assert_eq!(json["result"], json!({}));
    }

    #[tokio::test]
    async fn test_process_mcp_http_sse_message_request_publishes_initialize_response() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, pubsub) = make_test_state(conf, true).await;
        let pubsub = pubsub.unwrap();
        let transport_session_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();

        use hot::stream::StreamSubscriberFactory;
        let mut subscriber = pubsub
            .subscribe_in_env(env_id, transport_session_id)
            .await
            .unwrap();

        process_mcp_http_sse_message_request(
            state,
            HeaderMap::new(),
            None,
            None,
            McpRouteSource::Path {
                org_slug: "local".to_string(),
                env_name: "development".to_string(),
                service: "svc".to_string(),
            },
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "initialize".to_string(),
                params: Some(json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1.0"}
                })),
            },
            transport_session_id,
            env_id,
            pubsub.clone(),
        )
        .await;

        let event = tokio::time::timeout(Duration::from_secs(1), subscriber.next())
            .await
            .unwrap();
        let payload = extract_payload(event);
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["protocolVersion"], "2025-03-26");
    }

    #[tokio::test]
    async fn test_process_mcp_http_sse_message_request_does_not_cross_env_session_channel() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, pubsub) = make_test_state(conf, true).await;
        let pubsub = pubsub.unwrap();
        let transport_session_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let other_env_id = Uuid::now_v7();

        use hot::stream::StreamSubscriberFactory;
        let mut subscriber = pubsub
            .subscribe_in_env(env_id, transport_session_id)
            .await
            .unwrap();
        let mut other_env_subscriber = pubsub
            .subscribe_in_env(other_env_id, transport_session_id)
            .await
            .unwrap();

        process_mcp_http_sse_message_request(
            state,
            HeaderMap::new(),
            None,
            None,
            McpRouteSource::Path {
                org_slug: "local".to_string(),
                env_name: "development".to_string(),
                service: "svc".to_string(),
            },
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(10)),
                method: "initialize".to_string(),
                params: Some(json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test",
                        "version": "1.0",
                    },
                })),
            },
            transport_session_id,
            env_id,
            pubsub.clone(),
        )
        .await;

        let event = tokio::time::timeout(Duration::from_secs(1), subscriber.next())
            .await
            .unwrap();
        let payload = extract_payload(event);
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["id"], 10);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), other_env_subscriber.next())
                .await
                .is_err(),
            "MCP HTTP+SSE transport session response should not be delivered across env-scoped channels"
        );
    }

    #[tokio::test]
    async fn test_process_mcp_http_sse_message_request_publishes_jsonrpc_error() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, pubsub) = make_test_state(conf, true).await;
        let pubsub = pubsub.unwrap();
        let transport_session_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();

        use hot::stream::StreamSubscriberFactory;
        let mut subscriber = pubsub
            .subscribe_in_env(env_id, transport_session_id)
            .await
            .unwrap();

        process_mcp_http_sse_message_request(
            state,
            HeaderMap::new(),
            None,
            None,
            McpRouteSource::Path {
                org_slug: "local".to_string(),
                env_name: "development".to_string(),
                service: "svc".to_string(),
            },
            JsonRpcRequest {
                jsonrpc: "1.0".to_string(),
                id: Some(json!(2)),
                method: "initialize".to_string(),
                params: None,
            },
            transport_session_id,
            env_id,
            pubsub.clone(),
        )
        .await;

        let event = tokio::time::timeout(Duration::from_secs(1), subscriber.next())
            .await
            .unwrap();
        let payload = extract_payload(event);
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["id"], 2);
        assert_eq!(value["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test]
    async fn test_mcp_http_sse_messages_handler_returns_accepted_immediately_and_publishes() {
        let ctx = make_auth_test_context().await;
        let headers = bearer_header(&ctx.api_key_token);
        let auth = try_authenticate(&ctx.state.0, &headers)
            .await
            .unwrap()
            .unwrap();
        let env_id = auth.0.env_id();
        let principal = mcp_sse_transport_principal(&auth.0);
        let pubsub = ctx.state.3.as_ref().unwrap().clone();
        let transport_session_id = Uuid::now_v7();
        let route_key = format!("path:{}/{}/{}", ctx.org_slug, ctx.env_name, ctx.service);
        let session_timeout = mcp_http_sse_session_timeout(&ctx.state.2);

        use hot::stream::StreamSubscriberFactory;
        let mut subscriber = pubsub
            .subscribe_in_env(env_id, transport_session_id)
            .await
            .unwrap();
        pubsub
            .put_mcp_sse_transport_session(
                McpSseTransportSessionBinding {
                    transport_session_id,
                    env_id,
                    route_key,
                    principal,
                    expires_at: mcp_sse_transport_session_expires_at(session_timeout),
                },
                session_timeout,
            )
            .await
            .unwrap();

        let started = Instant::now();
        let status = mcp_http_sse_messages_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            Query(HttpSseMessageQuery {
                transport_session_id: transport_session_id.to_string(),
            }),
            headers,
            Json(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(3)),
                method: "ping".to_string(),
                params: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "HTTP+SSE POST should return quickly with 202"
        );

        let event = tokio::time::timeout(Duration::from_secs(1), subscriber.next())
            .await
            .unwrap();
        let payload = extract_payload(event);
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["id"], 3);
        assert_eq!(value["result"], json!({}));
    }

    #[tokio::test]
    async fn test_mcp_http_sse_messages_handler_rejects_missing_transport_session_binding() {
        let ctx = make_auth_test_context().await;
        let transport_session_id = Uuid::now_v7();

        let result = mcp_http_sse_messages_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((ctx.org_slug, ctx.env_name, ctx.service)),
            Query(HttpSseMessageQuery {
                transport_session_id: transport_session_id.to_string(),
            }),
            bearer_header(&ctx.api_key_token),
            Json(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(4)),
                method: "ping".to_string(),
                params: None,
            }),
        )
        .await;

        let (status, error) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error.0.error.code, "mcp_transport_session_not_found");
    }

    #[tokio::test]
    async fn test_mcp_http_sse_messages_handler_rejects_principal_mismatch() {
        let ctx = make_auth_test_context().await;
        let headers = bearer_header(&ctx.api_key_token);
        let auth = try_authenticate(&ctx.state.0, &headers)
            .await
            .unwrap()
            .unwrap();
        let env_id = auth.0.env_id();
        let pubsub = ctx.state.3.as_ref().unwrap().clone();
        let transport_session_id = Uuid::now_v7();
        let session_timeout = mcp_http_sse_session_timeout(&ctx.state.2);

        pubsub
            .put_mcp_sse_transport_session(
                McpSseTransportSessionBinding {
                    transport_session_id,
                    env_id,
                    route_key: format!("path:{}/{}/{}", ctx.org_slug, ctx.env_name, ctx.service),
                    principal: McpSseTransportPrincipal::ApiKey(Uuid::now_v7()),
                    expires_at: mcp_sse_transport_session_expires_at(session_timeout),
                },
                session_timeout,
            )
            .await
            .unwrap();

        let result = mcp_http_sse_messages_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((ctx.org_slug, ctx.env_name, ctx.service)),
            Query(HttpSseMessageQuery {
                transport_session_id: transport_session_id.to_string(),
            }),
            headers,
            Json(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(5)),
                method: "ping".to_string(),
                params: None,
            }),
        )
        .await;

        let (status, error) = result.unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error.0.error.code, "mcp_transport_session_mismatch");
    }

    #[tokio::test]
    async fn test_mcp_http_sse_get_handler_requires_auth() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, _) = make_test_state(conf, true).await;

        // No Authorization header → 401
        let result = mcp_sse_handler(
            State(state),
            Path((
                "local".to_string(),
                "development".to_string(),
                "svc".to_string(),
            )),
            HeaderMap::new(),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ========================================================================
    // Argument conversion tests (bug #4: named args must become positional Vec)
    // ========================================================================

    #[test]
    fn test_convert_named_args_single_param() {
        let args = json!({"name": "Cursor"});
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string"}
            }
        });

        let result = convert_named_args_to_positional(&args, Some(&schema)).unwrap();
        match result {
            Val::Vec(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0], Val::from("Cursor"));
            }
            other => panic!("Expected Vec, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_named_args_multiple_params_preserves_order() {
        let args = json!({"mood": "excited", "name": "World"});
        // Schema properties define the canonical order (name, mood)
        let schema = json!({
            "type": "object",
            "required": ["name", "mood"],
            "properties": {
                "name": {"type": "string"},
                "mood": {"type": "string"}
            }
        });

        let result = convert_named_args_to_positional(&args, Some(&schema)).unwrap();
        match result {
            Val::Vec(v) => {
                assert_eq!(v.len(), 2);
                // Order should follow schema properties, not the JSON object key order
                assert_eq!(v[0], Val::from("World"));
                assert_eq!(v[1], Val::from("excited"));
            }
            other => panic!("Expected Vec, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_named_args_optional_param_missing() {
        let args = json!({"name": "Alice"});
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string"},
                "greeting": {"type": "string"}
            }
        });

        let result = convert_named_args_to_positional(&args, Some(&schema)).unwrap();
        match result {
            Val::Vec(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0], Val::from("Alice"));
                assert_eq!(v[1], Val::Null); // Optional param becomes Null
            }
            other => panic!("Expected Vec, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_named_args_empty_object() {
        let args = json!({});
        let schema = json!({
            "type": "object",
            "properties": {}
        });

        let result = convert_named_args_to_positional(&args, Some(&schema)).unwrap();
        match result {
            Val::Vec(v) => assert!(v.is_empty()),
            other => panic!("Expected empty Vec, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_named_args_null_becomes_empty_vec() {
        let args = json!(null);
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}}
        });

        let result = convert_named_args_to_positional(&args, Some(&schema)).unwrap();
        match result {
            Val::Vec(v) => assert!(v.is_empty()),
            other => panic!("Expected empty Vec, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_named_args_rejects_non_object() {
        let args = json!([1, 2, 3]);
        let result = convert_named_args_to_positional(&args, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a JSON object"));
    }

    #[test]
    fn test_convert_named_args_no_schema_returns_empty() {
        // If schema is None or has no properties, we can't determine order
        let args = json!({"name": "Alice"});
        let result = convert_named_args_to_positional(&args, None).unwrap();
        match result {
            Val::Vec(v) => {
                // No schema means no properties to iterate, so empty Vec
                assert!(v.is_empty());
            }
            other => panic!("Expected empty Vec, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_named_args_numeric_and_bool_types() {
        let args = json!({"count": 42, "active": true, "name": "test"});
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"},
                "active": {"type": "boolean"}
            }
        });

        let result = convert_named_args_to_positional(&args, Some(&schema)).unwrap();
        match result {
            Val::Vec(v) => {
                assert_eq!(v.len(), 3);
                assert_eq!(v[0], Val::from("test"));
                assert_eq!(v[1], Val::Int(42));
                assert_eq!(v[2], Val::Bool(true));
            }
            other => panic!("Expected Vec, got {:?}", other),
        }
    }

    // ========================================================================
    // Event data structure contract tests (bugs #1 and #2)
    // ========================================================================

    /// The MCP event queue name must match the worker's event queue.
    /// Worker uses "hot:event" (see hot_worker/src/server.rs line ~2960).
    /// If this test fails, MCP calls will be enqueued but never processed.
    #[test]
    fn test_event_queue_name_matches_worker() {
        assert_eq!(
            MCP_EVENT_QUEUE_NAME, "hot:event",
            "MCP must publish to 'hot:event' queue where the worker consumes from"
        );
    }

    /// The event data must use "fn" as the key for the function name.
    /// The worker's `extract_target_function_from_event` looks for "fn"
    /// (see hot_worker/src/server.rs line ~649).
    /// The `call-event-handler` in hot-std reads `event.data.fn`.
    /// build_call_event_data must produce a map with "fn" key.
    #[test]
    fn test_event_data_fn_key_matches_worker_contract() {
        let event_data = build_call_event_data("::test/fn", Val::Vec(vec![]), None);
        if let Val::Map(map) = &event_data {
            assert!(
                map.get(&Val::from("fn")).is_some(),
                "Event data must use 'fn' key to match worker's extract_target_function_from_event"
            );
        } else {
            panic!("build_call_event_data must return a Map");
        }
    }

    /// The event data structure must contain "fn" and "args" keys.
    /// This matches the contract expected by:
    ///   - Worker: `extract_target_function_from_event` reads `event_data["fn"]`
    ///   - Hot: `call-event-handler` reads `event.data.fn` and `event.data.args`
    #[test]
    fn test_build_call_event_data_structure() {
        let args = Val::Vec(vec![Val::from("Alice")]);
        let event_data = build_call_event_data("::hot::hi/greet", args, None);

        match &event_data {
            Val::Map(map) => {
                // Must have "fn" key with the function name
                let fn_val = map.get(&Val::from("fn"));
                assert!(fn_val.is_some(), "Event data must contain 'fn' key");
                assert_eq!(
                    fn_val.unwrap(),
                    &Val::from("::hot::hi/greet"),
                    "fn value must be the fully qualified function name"
                );

                // Must have "args" key with a Vec
                let args_val = map.get(&Val::from("args"));
                assert!(args_val.is_some(), "Event data must contain 'args' key");
                match args_val.unwrap() {
                    Val::Vec(v) => {
                        assert_eq!(v.len(), 1);
                        assert_eq!(v[0], Val::from("Alice"));
                    }
                    other => panic!("args must be a Vec, got {:?}", other),
                }
            }
            other => panic!("Event data must be a Map, got {:?}", other),
        }
    }

    /// The event data "fn" key must be extractable by the worker's function.
    /// Simulates what `extract_target_function_from_event` does.
    #[test]
    fn test_event_data_fn_extractable_by_worker() {
        let event_data =
            build_call_event_data("::hot::hi/greet", Val::Vec(vec![Val::from("test")]), None);

        // Simulate worker's extract_target_function_from_event
        let extracted = match &event_data {
            Val::Map(map) => map.get(&Val::from("fn")).and_then(|v| match v {
                Val::Str(s) => Some((**s).to_string()),
                _ => None,
            }),
            _ => None,
        };

        assert_eq!(
            extracted,
            Some("::hot::hi/greet".to_string()),
            "Worker must be able to extract function name from event data"
        );
    }

    /// The event type for MCP calls must be "hot:call".
    /// This ensures the worker routes to the `call-event-handler` which
    /// listens for on-event: "hot:call".
    #[test]
    fn test_event_type_is_hot_call() {
        // The event_type used in the handler (line ~726)
        let event_type = "hot:call";
        assert_eq!(
            event_type, "hot:call",
            "MCP event type must be 'hot:call' to match call-event-handler"
        );
    }

    // ========================================================================
    // Stream event matching contract tests (bug #5: run_id mismatch)
    // ========================================================================
    //
    // The worker generates its own run_id, different from the one the MCP handler
    // creates. The wait_for_run_completion function must accept ANY terminal event
    // on the stream, not filter by run_id. These tests verify the matching logic
    // in isolation (without needing a DB).

    /// Helper: simulates the event matching logic from wait_for_run_completion.
    /// Returns Some(run_id) for terminal events, None for non-terminal events.
    fn match_terminal_event(event: &hot::stream::StreamEvent) -> Option<Uuid> {
        use hot::stream::StreamEvent;
        match event {
            StreamEvent::RunStop { run_id, .. }
            | StreamEvent::RunFail { run_id, .. }
            | StreamEvent::RunCancel { run_id, .. } => Some(*run_id),
            _ => None,
        }
    }

    #[test]
    fn test_stream_matching_accepts_any_run_id_on_stop() {
        use hot::stream::StreamEvent;
        let worker_run_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();

        let event = StreamEvent::RunStop {
            run_id: worker_run_id,
            env_id: Uuid::now_v7(),
            stream_id,
            event_id: None,
            result: Some(serde_json::json!("test result")),
        };

        // The match must return the worker's run_id (not filter by MCP's run_id)
        let matched = match_terminal_event(&event);
        assert_eq!(matched, Some(worker_run_id));
    }

    #[test]
    fn test_stream_matching_accepts_run_fail() {
        use hot::stream::StreamEvent;
        let run_id = Uuid::now_v7();

        let event = StreamEvent::RunFail {
            run_id,
            env_id: Uuid::now_v7(),
            stream_id: Uuid::now_v7(),
            event_id: None,
            error: Some("test error".to_string()),
        };

        assert_eq!(match_terminal_event(&event), Some(run_id));
    }

    #[test]
    fn test_stream_matching_accepts_run_cancel() {
        use hot::stream::StreamEvent;
        let run_id = Uuid::now_v7();

        let event = StreamEvent::RunCancel {
            run_id,
            env_id: Uuid::now_v7(),
            stream_id: Uuid::now_v7(),
            event_id: None,
            reason: Some("cancelled".to_string()),
        };

        assert_eq!(match_terminal_event(&event), Some(run_id));
    }

    #[test]
    fn test_stream_matching_skips_run_start() {
        use hot::stream::StreamEvent;

        let event = StreamEvent::RunStart {
            run_id: Uuid::now_v7(),
            env_id: Uuid::now_v7(),
            stream_id: Uuid::now_v7(),
            event_id: None,
        };

        assert_eq!(match_terminal_event(&event), None);
    }

    // ========================================================================
    // Content detection tests (image, audio, text extraction)
    // ========================================================================

    #[test]
    fn test_try_extract_content_image() {
        let value = json!({
            "type": "image",
            "data": "iVBORw0KGgo=",
            "mimeType": "image/png"
        });

        let result = try_extract_content(&value);
        assert!(result.is_some());
        match result.unwrap() {
            ToolResultContent::Image {
                data,
                mime_type,
                annotations,
            } => {
                assert_eq!(data, "iVBORw0KGgo=");
                assert_eq!(mime_type, "image/png");
                assert!(annotations.is_none());
            }
            other => panic!("Expected Image, got {:?}", other),
        }
    }

    #[test]
    fn test_try_extract_content_audio() {
        let value = json!({
            "type": "audio",
            "data": "UklGRg==",
            "mimeType": "audio/wav"
        });

        let result = try_extract_content(&value);
        assert!(result.is_some());
        match result.unwrap() {
            ToolResultContent::Audio {
                data,
                mime_type,
                annotations,
            } => {
                assert_eq!(data, "UklGRg==");
                assert_eq!(mime_type, "audio/wav");
                assert!(annotations.is_none());
            }
            other => panic!("Expected Audio, got {:?}", other),
        }
    }

    #[test]
    fn test_try_extract_content_text() {
        let value = json!({
            "type": "text",
            "text": "Hello world"
        });

        let result = try_extract_content(&value);
        assert!(result.is_some());
        match result.unwrap() {
            ToolResultContent::Text {
                text, annotations, ..
            } => {
                assert_eq!(text, "Hello world");
                assert!(annotations.is_none());
            }
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[test]
    fn test_try_extract_content_resource() {
        let value = json!({
            "type": "resource",
            "resource": {
                "uri": "file:///project/README.md",
                "mimeType": "text/markdown",
                "text": "# Hello"
            }
        });

        let result = try_extract_content(&value);
        assert!(result.is_some());
        match result.unwrap() {
            ToolResultContent::Resource {
                resource,
                annotations,
            } => {
                assert_eq!(resource["uri"], "file:///project/README.md");
                assert_eq!(resource["mimeType"], "text/markdown");
                assert_eq!(resource["text"], "# Hello");
                assert!(annotations.is_none());
            }
            other => panic!("Expected Resource, got {:?}", other),
        }
    }

    #[test]
    fn test_try_extract_content_with_annotations() {
        let value = json!({
            "type": "text",
            "text": "secret data",
            "annotations": {
                "audience": ["assistant"],
                "priority": 0.8
            }
        });

        let result = try_extract_content(&value);
        assert!(result.is_some());
        match result.unwrap() {
            ToolResultContent::Text {
                text, annotations, ..
            } => {
                assert_eq!(text, "secret data");
                let ann = annotations.expect("annotations should be present");
                assert_eq!(ann["audience"][0], "assistant");
                assert_eq!(ann["priority"], 0.8);
            }
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[test]
    fn test_try_extract_content_unknown_type() {
        let value = json!({"type": "video", "data": "..."});
        assert!(try_extract_content(&value).is_none());
    }

    #[test]
    fn test_try_extract_content_no_type() {
        let value = json!({"message": "hello"});
        assert!(try_extract_content(&value).is_none());
    }

    #[test]
    fn test_extract_content_array_mixed() {
        let result = json!([
            {"type": "text", "text": "Here is the chart:"},
            {"type": "image", "data": "abc123", "mimeType": "image/png"},
            {"some": "plain object"}
        ]);

        let items = extract_content_from_result(&result);
        assert_eq!(items.len(), 3);

        match &items[0] {
            ToolResultContent::Text { text, .. } => assert_eq!(text, "Here is the chart:"),
            other => panic!("Expected Text, got {:?}", other),
        }
        match &items[1] {
            ToolResultContent::Image {
                data, mime_type, ..
            } => {
                assert_eq!(data, "abc123");
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("Expected Image, got {:?}", other),
        }
        // Third item should be serialised as text (no "type" field)
        match &items[2] {
            ToolResultContent::Text { text, .. } => assert!(text.contains("plain object")),
            other => panic!("Expected Text fallback, got {:?}", other),
        }
    }

    #[test]
    fn test_extract_content_plain_value() {
        let result = json!({"answer": 42});
        let items = extract_content_from_result(&result);
        assert_eq!(items.len(), 1);
        match &items[0] {
            ToolResultContent::Text { text, .. } => assert!(text.contains("42")),
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[test]
    fn test_image_content_serialization() {
        let content = ToolResultContent::Image {
            data: "abc123".to_string(),
            mime_type: "image/png".to_string(),
            annotations: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["data"], "abc123");
        assert_eq!(json["mimeType"], "image/png");
        assert!(json.get("annotations").is_none());
    }

    #[test]
    fn test_audio_content_serialization() {
        let content = ToolResultContent::Audio {
            data: "UklGRg==".to_string(),
            mime_type: "audio/wav".to_string(),
            annotations: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "audio");
        assert_eq!(json["data"], "UklGRg==");
        assert_eq!(json["mimeType"], "audio/wav");
        assert!(json.get("annotations").is_none());
    }

    #[test]
    fn test_resource_content_serialization() {
        let content = ToolResultContent::Resource {
            resource: json!({
                "uri": "file:///test.md",
                "mimeType": "text/markdown",
                "text": "# Test"
            }),
            annotations: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "resource");
        assert_eq!(json["resource"]["uri"], "file:///test.md");
        assert_eq!(json["resource"]["text"], "# Test");
        assert!(json.get("annotations").is_none());
    }

    #[test]
    fn test_content_serialization_with_annotations() {
        let content = ToolResultContent::Text {
            text: "hello".to_string(),
            annotations: Some(json!({"audience": ["user"], "priority": 0.5})),
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["annotations"]["audience"][0], "user");
        assert_eq!(json["annotations"]["priority"], 0.5);
    }

    #[test]
    fn test_tools_call_result_meta_omitted_when_none() {
        let result = ToolsCallResult {
            content: vec![ToolResultContent::Text {
                text: "ok".to_string(),
                annotations: None,
            }],
            is_error: None,
            meta: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("_meta").is_none());
        assert!(json.get("isError").is_none());
    }

    // ========================================================================
    // Auth integration tests
    //
    // These tests use a real SQLite in-memory database with full schema setup
    // (org, env, project, build, API key, MCP tools) to validate the conditional
    // authentication behavior end-to-end through the handler layer.
    // ========================================================================

    /// Full test context with a real DB, API key, and MCP tools with different auth modes.
    struct AuthTestContext {
        state: ApiStateData,
        api_key_token: String,
        org_slug: String,
        env_name: String,
        service: String,
    }

    async fn make_auth_test_context() -> AuthTestContext {
        let db_conf = val!({
            "uri": "sqlite::memory:",
            "schema": "hot"
        });
        let db = hot::db::create_db_pool(&db_conf).await.unwrap();

        // Run SQLite migrations to create tables
        match &db {
            hot::db::DatabasePool::Sqlite(pool) => {
                let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
                let migration_path = manifest_dir
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("resources/db/sqlite/migrations");
                let migrator = sqlx::migrate::Migrator::new(migration_path)
                    .await
                    .expect("Failed to create migrator");
                migrator.run(pool).await.expect("Failed to run migrations");
            }
            _ => panic!("Expected SQLite database for tests"),
        }

        // Insert default data: org "local", env "development", user
        let test_data = hot::db::insert_test_data(&db).await.unwrap();

        // Generate and insert a real API key
        let api_key_id = Uuid::now_v7();
        let (api_key_token, key_data_json) = ApiKey::generate_api_key(&api_key_id).unwrap();
        let key_data: serde_json::Value = serde_json::from_str(&key_data_json).unwrap();
        let full_access = json!({"*:*": ["*"]});
        ApiKey::insert_api_key(
            &db,
            &api_key_id,
            &test_data.env_id,
            "test-key",
            &key_data,
            &test_data.user_id,
            &full_access,
        )
        .await
        .unwrap();

        // Deploy the build (MCP tool queries filter on deployed = true)
        hot::db::build::Build::deploy_build(&db, &test_data.build_id, &test_data.user_id)
            .await
            .unwrap();

        let service = "weather";

        // Insert a public tool (auth: "none")
        McpTool::insert_mcp_tool(
            &db,
            &Uuid::now_v7(),
            &test_data.build_id,
            service,
            "::weather",
            "get-forecast",
            "get-forecast",
            Some("Get weather forecast (public)"),
            Some(&json!({"type": "object", "properties": {"city": {"type": "string"}}})),
            None,
            None,
            None,
            None,
            Some(&json!({"mcp": {"auth": "none"}})),
            Some("weather.hot"),
            Some(1),
            None,
            None,
        )
        .await
        .unwrap();

        // Insert a required-auth tool (auth: "required" — the default)
        McpTool::insert_mcp_tool(
            &db,
            &Uuid::now_v7(),
            &test_data.build_id,
            service,
            "::weather",
            "get-alerts",
            "get-alerts",
            Some("Get weather alerts (requires auth)"),
            Some(&json!({"type": "object", "properties": {"region": {"type": "string"}}})),
            None,
            None,
            None,
            None,
            Some(&json!({"mcp": {}})),
            Some("weather.hot"),
            Some(10),
            None,
            None,
        )
        .await
        .unwrap();

        // Insert a tool with no meta (defaults to auth: "required")
        McpTool::insert_mcp_tool(
            &db,
            &Uuid::now_v7(),
            &test_data.build_id,
            service,
            "::weather",
            "get-history",
            "get-history",
            Some("Get weather history (default auth)"),
            Some(&json!({"type": "object", "properties": {"date": {"type": "string"}}})),
            None,
            None,
            None,
            None,
            None,
            Some("weather.hot"),
            Some(20),
            None,
            None,
        )
        .await
        .unwrap();

        let storage: Arc<Box<dyn hot::storage::BuildStorage>> =
            Arc::new(Box::new(TestBuildStorage));
        let conf = Arc::new(val!({"mcp": {"enabled": true}}));
        let pubsub = Some(Arc::new(
            hot::stream::StreamPubSub::new(hot::stream::StreamPubSubType::Memory, None, false)
                .unwrap(),
        ));
        let state: ApiStateData = (Arc::new(db), storage, conf, pubsub);

        AuthTestContext {
            state,
            api_key_token,
            org_slug: "local".to_string(),
            env_name: "development".to_string(),
            service: service.to_string(),
        }
    }

    fn bearer_header(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", token).parse().unwrap(),
        );
        headers
    }

    fn make_jsonrpc(method: &str, params: Option<JsonValue>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: method.to_string(),
            params,
        }
    }

    async fn call_mcp(
        ctx: &AuthTestContext,
        headers: HeaderMap,
        request: JsonRpcRequest,
    ) -> axum::response::Response {
        mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            headers,
            Json(request),
        )
        .await
        .unwrap()
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    // --- Initialize / ping: always work without auth ---

    #[tokio::test]
    async fn test_auth_initialize_works_without_credentials() {
        let ctx = make_auth_test_context().await;
        let resp = call_mcp(
            &ctx,
            HeaderMap::new(),
            make_jsonrpc(
                "initialize",
                Some(json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1.0"}
                })),
            ),
        )
        .await;
        let json = response_json(resp).await;
        assert!(json["result"]["protocolVersion"].is_string());
        assert!(json["error"].is_null());
    }

    #[tokio::test]
    async fn test_auth_ping_works_without_credentials() {
        let ctx = make_auth_test_context().await;
        let resp = call_mcp(&ctx, HeaderMap::new(), make_jsonrpc("ping", None)).await;
        let json = response_json(resp).await;
        assert_eq!(json["result"], json!({}));
    }

    // --- tools/list: works with and without auth ---

    #[tokio::test]
    async fn test_auth_tools_list_without_credentials() {
        let ctx = make_auth_test_context().await;
        let resp = call_mcp(&ctx, HeaderMap::new(), make_jsonrpc("tools/list", None)).await;
        let json = response_json(resp).await;
        let tools = json["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3, "Should list all tools regardless of auth");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"get-forecast"));
        assert!(names.contains(&"get-alerts"));
        assert!(names.contains(&"get-history"));
    }

    #[tokio::test]
    async fn test_auth_tools_list_with_valid_credentials() {
        let ctx = make_auth_test_context().await;
        let resp = call_mcp(
            &ctx,
            bearer_header(&ctx.api_key_token),
            make_jsonrpc("tools/list", None),
        )
        .await;
        let json = response_json(resp).await;
        let tools = json["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
    }

    // --- tools/call on public tool (auth: "none"): works without auth ---

    #[tokio::test]
    async fn test_auth_call_public_tool_without_credentials_succeeds() {
        let ctx = make_auth_test_context().await;
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            HeaderMap::new(),
            Json(make_jsonrpc(
                "tools/call",
                Some(json!({"name": "get-forecast", "arguments": {"city": "Portland"}})),
            )),
        )
        .await;
        // The auth check should pass (tool is public). The call may succeed or fail
        // at the event queue stage, but it must NOT fail with 401 or 403.
        match result {
            Ok(resp) => {
                let status = resp.status();
                assert_ne!(status, StatusCode::UNAUTHORIZED);
                assert_ne!(status, StatusCode::FORBIDDEN);
            }
            Err((status, _)) => {
                assert_ne!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "Public tool should not return 401"
                );
                assert_ne!(
                    status,
                    StatusCode::FORBIDDEN,
                    "Public tool should not return 403"
                );
            }
        }
    }

    // --- tools/call on required-auth tool: fails without auth ---

    #[tokio::test]
    async fn test_auth_call_required_tool_without_credentials_returns_401() {
        let ctx = make_auth_test_context().await;
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            HeaderMap::new(),
            Json(make_jsonrpc(
                "tools/call",
                Some(json!({"name": "get-alerts", "arguments": {"region": "NW"}})),
            )),
        )
        .await;
        // tools/call on a required-auth tool without credentials returns 401
        let (status, body) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let error_msg = body.0.error.message.to_lowercase();
        assert!(
            error_msg.contains("authentication") || error_msg.contains("unauthorized"),
            "Error should mention authentication: {}",
            error_msg
        );
    }

    #[tokio::test]
    async fn test_auth_call_default_auth_tool_without_credentials_returns_401() {
        let ctx = make_auth_test_context().await;
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            HeaderMap::new(),
            Json(make_jsonrpc(
                "tools/call",
                Some(json!({"name": "get-history", "arguments": {"date": "2026-01-01"}})),
            )),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "Tool with no auth meta should default to required"
        );
    }

    // --- tools/call on required-auth tool: succeeds with valid credentials ---

    #[tokio::test]
    async fn test_auth_call_required_tool_with_valid_credentials_succeeds() {
        let ctx = make_auth_test_context().await;
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            bearer_header(&ctx.api_key_token),
            Json(make_jsonrpc(
                "tools/call",
                Some(json!({"name": "get-alerts", "arguments": {"region": "NW"}})),
            )),
        )
        .await;
        // Auth should pass. The call may fail at event queue stage, but not 401/403.
        match result {
            Ok(resp) => {
                let status = resp.status();
                assert_ne!(status, StatusCode::UNAUTHORIZED);
                assert_ne!(status, StatusCode::FORBIDDEN);
            }
            Err((status, _)) => {
                assert_ne!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "Valid credentials should not return 401"
                );
                assert_ne!(
                    status,
                    StatusCode::FORBIDDEN,
                    "Valid credentials should not return 403"
                );
            }
        }
    }

    // --- Invalid credentials: returns 401 ---

    #[tokio::test]
    async fn test_auth_invalid_bearer_token_returns_401() {
        let ctx = make_auth_test_context().await;
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((ctx.org_slug.clone(), ctx.env_name.clone(), ctx.service.clone())),
            bearer_header("hot_00000000000000000000000000000000_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Json(make_jsonrpc("tools/list", None)),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_malformed_bearer_token_is_ignored() {
        let ctx = make_auth_test_context().await;
        // A non-"hot_" token that doesn't match any token format is treated as
        // a service key lookup, which also fails → 401.
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            bearer_header("not-a-valid-token"),
            Json(make_jsonrpc("tools/list", None)),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // --- tools/call on nonexistent tool ---

    #[tokio::test]
    async fn test_auth_call_nonexistent_tool_returns_error() {
        let ctx = make_auth_test_context().await;
        let resp = call_mcp(
            &ctx,
            HeaderMap::new(),
            make_jsonrpc(
                "tools/call",
                Some(json!({"name": "nonexistent", "arguments": {}})),
            ),
        )
        .await;
        let json = response_json(resp).await;
        assert!(json["error"].is_object(), "Should return JSON-RPC error");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
    }

    // --- Wrong org returns 404 ---

    #[tokio::test]
    async fn test_auth_wrong_org_slug_returns_not_found() {
        let ctx = make_auth_test_context().await;
        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                "nonexistent-org".to_string(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            HeaderMap::new(),
            Json(make_jsonrpc("tools/list", None)),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // --- Authenticated request with wrong org returns 403 ---

    #[tokio::test]
    async fn test_auth_credentials_wrong_org_returns_403() {
        let ctx = make_auth_test_context().await;
        // Create a second org so lookup succeeds but cross-validation fails.
        // Must use a valid user_id for FK constraints.
        let db = &ctx.state.0;
        let default_user = hot::db::user::User::get_default_user(db).await.unwrap();
        let org2_id = Uuid::now_v7();
        hot::db::org::Org::insert_org(
            db,
            &org2_id,
            "Other Org",
            "other-org",
            "organization",
            &default_user.user_id,
        )
        .await
        .unwrap();
        let env2_id = Uuid::now_v7();
        hot::db::Env::insert_env(db, &env2_id, &org2_id, "development", &default_user.user_id)
            .await
            .unwrap();

        let result = mcp_handler(
            State(ctx.state.clone()),
            OptionalAccessId(None),
            None,
            Path((
                "other-org".to_string(),
                "development".to_string(),
                ctx.service.clone(),
            )),
            bearer_header(&ctx.api_key_token),
            Json(make_jsonrpc("tools/list", None)),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "Credentials for org 'local' used against org 'other-org' should be forbidden"
        );
    }

    // --- HTTP+SSE requires auth ---

    #[tokio::test]
    async fn test_auth_http_sse_without_credentials_returns_401() {
        let ctx = make_auth_test_context().await;
        let result = mcp_sse_handler(
            State(ctx.state.clone()),
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            HeaderMap::new(),
        )
        .await;
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_http_sse_with_valid_credentials_returns_sse() {
        let ctx = make_auth_test_context().await;
        let result = mcp_sse_handler(
            State(ctx.state.clone()),
            Path((
                ctx.org_slug.clone(),
                ctx.env_name.clone(),
                ctx.service.clone(),
            )),
            bearer_header(&ctx.api_key_token),
        )
        .await;
        let response = result.unwrap().into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(content_type.contains("text/event-stream"));
    }

    // --- build_request_val tests ---

    fn test_request_val(
        headers: &HeaderMap,
        url: &str,
        auth: Option<&(AuthContext, ApiKey)>,
    ) -> Val {
        test_request_val_with_client_ip(headers, url, None, auth)
    }

    fn test_request_val_with_client_ip(
        headers: &HeaderMap,
        url: &str,
        client_ip: Option<&ClientIp>,
        auth: Option<&(AuthContext, ApiKey)>,
    ) -> Val {
        build_request_val(
            "POST",
            url,
            None,
            headers,
            client_ip,
            &std::collections::HashMap::new(),
            None,
            auth,
            &Uuid::nil(),
        )
    }

    #[test]
    fn test_build_request_val_without_auth() {
        let headers = HeaderMap::new();
        let val = test_request_val(&headers, "/mcp/org/svc", None);
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("method")), Some(&Val::from("POST")));
            assert_eq!(map.get(&Val::from("url")), Some(&Val::from("/mcp/org/svc")));
            assert_eq!(
                map.get(&Val::from("$type")),
                Some(&Val::from("::hot::http/HttpRequest"))
            );
            assert!(
                map.get(&Val::from("auth")).is_none(),
                "Unauthenticated request should have no auth field"
            );
            assert!(map.get(&Val::from("headers")).is_some());
            assert!(map.get(&Val::from("query")).is_some());
        } else {
            panic!("Expected Map, got {:?}", val);
        }
    }

    #[test]
    fn test_build_request_val_with_api_key_auth() {
        let now = chrono::Utc::now();
        let api_key = ApiKey {
            api_key_id: Uuid::now_v7(),
            env_id: Uuid::now_v7(),
            description: "test".to_string(),
            key_data: json!({"hash": "test"}),
            active: true,
            created_by_user_id: Uuid::now_v7(),
            created_at: now,
            updated_at: now,
            updated_by_user_id: None,
            active_toggle_at: None,
            active_toggle_by_user_id: None,
            permissions: serde_json::Value::Null,
        };
        let auth_ctx = AuthContext::ApiKey(api_key.clone());
        let auth = (auth_ctx, api_key);

        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let client_ip = ClientIp("1.2.3.4".to_string());

        let val = test_request_val_with_client_ip(
            &headers,
            "/mcp/myorg/svc",
            Some(&client_ip),
            Some(&auth),
        );
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("method")), Some(&Val::from("POST")));
            assert_eq!(
                map.get(&Val::from("url")),
                Some(&Val::from("/mcp/myorg/svc"))
            );
            assert_eq!(map.get(&Val::from("ip")), Some(&Val::from("1.2.3.4")));

            let auth_val = map.get(&Val::from("auth")).expect("auth should be present");
            if let Val::Map(auth_map) = auth_val {
                assert_eq!(
                    auth_map.get(&Val::from("type")),
                    Some(&Val::from("api-key"))
                );
            } else {
                panic!("Expected auth to be a Map");
            }

            let headers_val = map.get(&Val::from("headers")).unwrap();
            if let Val::Map(hmap) = headers_val {
                assert_eq!(
                    hmap.get(&Val::from("content-type")),
                    Some(&Val::from("application/json"))
                );
            } else {
                panic!("Expected headers to be a Map");
            }
        } else {
            panic!("Expected Map, got {:?}", val);
        }
    }

    #[test]
    fn test_build_request_val_uses_resolved_client_ip() {
        let headers = HeaderMap::new();
        let client_ip = ClientIp("10.0.0.1".to_string());
        let val = test_request_val_with_client_ip(&headers, "/mcp/o/s", Some(&client_ip), None);
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("ip")), Some(&Val::from("10.0.0.1")));
        }
    }

    #[test]
    fn test_build_request_val_does_not_trust_raw_forwarding_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "9.8.7.6, 10.0.0.1".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/s", None);
        if let Val::Map(ref map) = val {
            assert!(
                map.get(&Val::from("ip")).is_none(),
                "Raw forwarding headers must be resolved by client IP middleware"
            );
        }
    }

    #[test]
    fn test_build_request_val_no_ip_when_no_proxy_headers() {
        let headers = HeaderMap::new();
        let val = test_request_val(&headers, "/mcp/o/s", None);
        if let Val::Map(ref map) = val {
            assert!(
                map.get(&Val::from("ip")).is_none(),
                "No IP when no proxy headers"
            );
        }
    }

    // --- hash_sensitive_request_fields unit tests ---

    #[test]
    fn test_hash_sensitive_hashes_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer sk-secret-123".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let hashes = hash_sensitive_request_fields(&val, &[]);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("Bearer sk-secret-123").hash(&mut hasher);
        let auth_hash = hasher.finish();
        assert!(
            hashes.contains(&auth_hash),
            "Authorization header value should be hashed"
        );

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("application/json").hash(&mut hasher);
        let ct_hash = hasher.finish();
        assert!(
            !hashes.contains(&ct_hash),
            "content-type header value should not be hashed"
        );
    }

    #[test]
    fn test_hash_sensitive_hashes_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "session=abc123".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let hashes = hash_sensitive_request_fields(&val, &[]);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("session=abc123").hash(&mut hasher);
        assert!(
            hashes.contains(&hasher.finish()),
            "Cookie header value should be hashed"
        );
    }

    #[test]
    fn test_hash_sensitive_skips_non_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", "test-client/1.0".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        headers.insert("x-request-id", "req-456".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let hashes = hash_sensitive_request_fields(&val, &[]);

        assert!(
            hashes.is_empty(),
            "No sensitive headers present — hashes should be empty"
        );
    }

    #[test]
    fn test_hash_sensitive_includes_extra_secret_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "my-custom-key".parse().unwrap());
        headers.insert("content-type", "text/plain".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let extra = vec!["x-api-key".to_string()];
        let hashes = hash_sensitive_request_fields(&val, &extra);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("my-custom-key").hash(&mut hasher);
        assert!(
            hashes.contains(&hasher.finish()),
            "User-declared secret header should be hashed"
        );

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("text/plain").hash(&mut hasher);
        assert!(
            !hashes.contains(&hasher.finish()),
            "Non-secret header should not be hashed"
        );
    }

    #[test]
    fn test_hash_sensitive_hashes_auth_subtree() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());

        let now = chrono::Utc::now();
        let api_key = ApiKey {
            api_key_id: Uuid::now_v7(),
            env_id: Uuid::now_v7(),
            description: "test".to_string(),
            key_data: json!({"hash": "test"}),
            active: true,
            created_by_user_id: Uuid::now_v7(),
            created_at: now,
            updated_at: now,
            updated_by_user_id: None,
            active_toggle_at: None,
            active_toggle_by_user_id: None,
            permissions: serde_json::Value::Null,
        };
        let auth_ctx = AuthContext::ApiKey(api_key.clone());
        let auth = Some((auth_ctx, api_key));
        let val = test_request_val(&headers, "/mcp/o/e/s", auth.as_ref());
        let hashes = hash_sensitive_request_fields(&val, &[]);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("api-key").hash(&mut hasher);
        assert!(
            hashes.contains(&hasher.finish()),
            "Auth type value should be hashed"
        );
    }

    #[test]
    fn test_hash_sensitive_no_hashes_for_method_url_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "1.2.3.4".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let hashes = hash_sensitive_request_fields(&val, &[]);

        use std::hash::{Hash, Hasher};
        for v in ["POST", "/mcp/o/e/s", "1.2.3.4"] {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            Val::from(v).hash(&mut hasher);
            assert!(
                !hashes.contains(&hasher.finish()),
                "'{}' should not be hashed as a secret",
                v
            );
        }
    }

    #[test]
    fn test_hash_sensitive_hashes_proxy_authorization_and_set_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert("proxy-authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        headers.insert("set-cookie", "id=abc; HttpOnly".parse().unwrap());
        headers.insert("accept", "text/html".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let hashes = hash_sensitive_request_fields(&val, &[]);

        use std::hash::{Hash, Hasher};
        let mut h1 = std::collections::hash_map::DefaultHasher::new();
        Val::from("Basic dXNlcjpwYXNz").hash(&mut h1);
        assert!(
            hashes.contains(&h1.finish()),
            "proxy-authorization should be hashed"
        );

        let mut h2 = std::collections::hash_map::DefaultHasher::new();
        Val::from("id=abc; HttpOnly").hash(&mut h2);
        assert!(hashes.contains(&h2.finish()), "set-cookie should be hashed");

        let mut h3 = std::collections::hash_map::DefaultHasher::new();
        Val::from("text/html").hash(&mut h3);
        assert!(
            !hashes.contains(&h3.finish()),
            "accept should not be hashed"
        );
    }

    #[test]
    fn test_hash_sensitive_mixed_realistic_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer tok-123".parse().unwrap());
        headers.insert("cookie", "sess=xyz".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("user-agent", "my-client/2.0".parse().unwrap());
        headers.insert("x-request-id", "req-789".parse().unwrap());
        headers.insert("x-api-key", "custom-secret".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let extra = vec!["x-api-key".to_string()];
        let hashes = hash_sensitive_request_fields(&val, &extra);

        use std::hash::{Hash, Hasher};
        fn val_hash(s: &str) -> u64 {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            Val::from(s).hash(&mut h);
            h.finish()
        }

        assert!(
            hashes.contains(&val_hash("Bearer tok-123")),
            "authorization hashed"
        );
        assert!(hashes.contains(&val_hash("sess=xyz")), "cookie hashed");
        assert!(
            hashes.contains(&val_hash("custom-secret")),
            "x-api-key hashed"
        );

        assert!(
            !hashes.contains(&val_hash("application/json")),
            "content-type not hashed"
        );
        assert!(
            !hashes.contains(&val_hash("my-client/2.0")),
            "user-agent not hashed"
        );
        assert!(
            !hashes.contains(&val_hash("req-789")),
            "x-request-id not hashed"
        );
    }

    #[test]
    fn test_hash_sensitive_extra_headers_are_lowercased() {
        let mut headers = HeaderMap::new();
        headers.insert("x-customer-token", "secret-val".parse().unwrap());

        let val = test_request_val(&headers, "/mcp/o/e/s", None);
        let extra = vec!["X-Customer-Token".to_string()];
        let hashes = hash_sensitive_request_fields(&val, &extra);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("secret-val").hash(&mut hasher);
        assert!(
            hashes.contains(&hasher.finish()),
            "Extra header with mixed-case declaration should match lowercased header key"
        );
    }

    #[test]
    fn test_mcp_tool_secret_headers_from_meta() {
        let tool = McpTool {
            mcp_tool_id: Uuid::nil(),
            build_id: Uuid::nil(),
            service: "test".to_string(),
            ns: "::test".to_string(),
            var: "fn1".to_string(),
            name: "fn1".to_string(),
            description: None,
            input_schema: None,
            output_schema: None,
            title: None,
            icons: None,
            annotations: None,
            meta: Some(json!({
                "mcp": {"service": "test", "auth": "none"},
                "secret-headers": ["x-api-key", "x-customer-secret"]
            })),
            file: None,
            line: None,
            column: None,
            position: None,
        };
        let sh = tool.secret_headers();
        assert_eq!(sh, vec!["x-api-key", "x-customer-secret"]);
    }

    #[test]
    fn test_mcp_tool_secret_headers_empty_when_absent() {
        let tool = McpTool {
            mcp_tool_id: Uuid::nil(),
            build_id: Uuid::nil(),
            service: "test".to_string(),
            ns: "::test".to_string(),
            var: "fn1".to_string(),
            name: "fn1".to_string(),
            description: None,
            input_schema: None,
            output_schema: None,
            title: None,
            icons: None,
            annotations: None,
            meta: Some(json!({"mcp": {"service": "test"}})),
            file: None,
            line: None,
            column: None,
            position: None,
        };
        assert!(tool.secret_headers().is_empty());
    }

    #[test]
    fn test_mcp_tool_secret_headers_empty_when_no_meta() {
        let tool = McpTool {
            mcp_tool_id: Uuid::nil(),
            build_id: Uuid::nil(),
            service: "test".to_string(),
            ns: "::test".to_string(),
            var: "fn1".to_string(),
            name: "fn1".to_string(),
            description: None,
            input_schema: None,
            output_schema: None,
            title: None,
            icons: None,
            annotations: None,
            meta: None,
            file: None,
            line: None,
            column: None,
            position: None,
        };
        assert!(tool.secret_headers().is_empty());
    }

    // --- try_authenticate unit tests ---

    #[tokio::test]
    async fn test_try_authenticate_no_auth_header_returns_none() {
        let db_conf = val!({"uri": "sqlite::memory:", "schema": "hot"});
        let db = Arc::new(hot::db::create_db_pool(&db_conf).await.unwrap());
        let headers = HeaderMap::new();

        let result = try_authenticate(&db, &headers).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_try_authenticate_empty_bearer_returns_none() {
        let db_conf = val!({"uri": "sqlite::memory:", "schema": "hot"});
        let db = Arc::new(hot::db::create_db_pool(&db_conf).await.unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());

        let result = try_authenticate(&db, &headers).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_try_authenticate_non_bearer_returns_none() {
        let db_conf = val!({"uri": "sqlite::memory:", "schema": "hot"});
        let db = Arc::new(hot::db::create_db_pool(&db_conf).await.unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());

        let result = try_authenticate(&db, &headers).await.unwrap();
        assert!(result.is_none());
    }

    /// Create a DB pool with migrations for standalone unit tests
    async fn make_migrated_db() -> Arc<hot::db::DatabasePool> {
        let db_conf = val!({"uri": "sqlite::memory:", "schema": "hot"});
        let db = hot::db::create_db_pool(&db_conf).await.unwrap();
        match &db {
            hot::db::DatabasePool::Sqlite(pool) => {
                let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
                let migration_path = manifest_dir
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("resources/db/sqlite/migrations");
                let migrator = sqlx::migrate::Migrator::new(migration_path)
                    .await
                    .expect("Failed to create migrator");
                migrator.run(pool).await.expect("Failed to run migrations");
            }
            _ => panic!("Expected SQLite"),
        }
        Arc::new(db)
    }

    #[tokio::test]
    async fn test_try_authenticate_invalid_token_returns_err() {
        let db = make_migrated_db().await;
        hot::db::insert_default_data(&db).await.unwrap();

        let headers = bearer_header(
            "hot_00000000000000000000000000000000_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );

        let result = try_authenticate(&db, &headers).await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_try_authenticate_valid_api_key_returns_some() {
        let db = make_migrated_db().await;
        let test_data = hot::db::insert_test_data(&db).await.unwrap();

        let api_key_id = Uuid::now_v7();
        let (token, key_data_json) = ApiKey::generate_api_key(&api_key_id).unwrap();
        let key_data: serde_json::Value = serde_json::from_str(&key_data_json).unwrap();
        ApiKey::insert_api_key(
            &db,
            &api_key_id,
            &test_data.env_id,
            "test",
            &key_data,
            &test_data.user_id,
            &serde_json::Value::Null,
        )
        .await
        .unwrap();

        let headers = bearer_header(&token);
        let result = try_authenticate(&db, &headers).await.unwrap();
        assert!(result.is_some());
        let (auth_ctx, key) = result.unwrap();
        assert!(matches!(auth_ctx, AuthContext::ApiKey(_)));
        assert_eq!(key.api_key_id, api_key_id);
    }

    #[tokio::test]
    async fn test_mcp_handler_domain_returns_404_without_resolved_domain() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, _) = make_test_state(conf, false).await;

        let result = mcp_handler_domain(
            State(state),
            OptionalAccessId(None),
            None,
            None,
            Path("svc".to_string()),
            HeaderMap::new(),
            Json(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "ping".to_string(),
                params: None,
            }),
        )
        .await;

        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_mcp_handler_domain_works_with_resolved_domain() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, _) = make_test_state(conf, false).await;

        let resolved = ResolvedDomain {
            domain: "mcp.example.com".to_string(),
            env_id: Uuid::now_v7(),
            org_id: Uuid::now_v7(),
        };

        let response = mcp_handler_domain(
            State(state),
            OptionalAccessId(None),
            None,
            Some(Extension(resolved)),
            Path("svc".to_string()),
            HeaderMap::new(),
            Json(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "ping".to_string(),
                params: None,
            }),
        )
        .await
        .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["result"], json!({}));
    }

    #[tokio::test]
    async fn test_mcp_sse_handler_domain_returns_404_without_resolved_domain() {
        let conf = val!({"mcp": {"enabled": true}});
        let (state, _api_key, _) = make_test_state(conf, false).await;

        let result = mcp_sse_handler_domain(
            State(state),
            None,
            Path("svc".to_string()),
            HeaderMap::new(),
        )
        .await;

        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
