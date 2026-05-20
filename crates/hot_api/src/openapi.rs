//! OpenAPI document for the Hot API.
#![allow(dead_code)]

use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};
use uuid::Uuid;

use crate::handlers::{
    CreateDomainRequest, CreateServiceKeyRequest, CreateSessionRequest, DomainResponse,
    DomainVerifyResponse, EnvSseEvent, EventPublishedEvent, Limits, OrgUsageResponse, PlanInfo,
    RevokeAllResponse, RevokeAllServiceKeysResponse, ServiceKeyResponse, SessionResponse,
    StreamEvent, SubscribeWithEventRequest, UsagePercent, UsageStats,
};
use crate::models::{
    ApiError, ApiErrorResponse, ApiListResponse, ApiResponse, BuildResponse, BuildUploadResponse,
    BuildWithProjectResponse, ContextVariableResponse, CreateContextVariableRequest,
    CreateProjectRequest, EnvironmentResponse, EventHandlerResponse, EventResponse, FileResponse,
    InitiateUploadRequest, InitiateUploadResponse, PaginationMeta, ProjectActivateResponse,
    ProjectResponse, PublishEventRequest, ResponseMeta, RunResponse, RunStatsResponse,
    ScheduleResponse, UpdateContextVariableRequest, UpdateProjectRequest, UploadPartResponse,
};

#[derive(Debug, Serialize, ToSchema)]
struct StatusResponse {
    status: String,
    service: String,
    version: String,
    git_sha: String,
    start_time: String,
}

#[derive(Debug, ToSchema)]
struct UploadBuildMultipart {
    #[schema(value_type = String, format = Binary)]
    file: String,
    hash: String,
    build_id: Option<Uuid>,
}

#[derive(Debug, ToSchema)]
struct BinaryUploadBody {
    #[schema(value_type = String, format = Binary)]
    file: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hot API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Authenticated REST and SSE API for Hot Dev.",
        license(name = "Apache-2.0"),
    ),
    paths(
        status_doc,
        list_projects_doc,
        create_project_doc,
        get_project_doc,
        update_project_doc,
        delete_project_doc,
        activate_project_doc,
        deactivate_project_doc,
        list_builds_by_env_doc,
        list_builds_doc,
        upload_build_doc,
        get_deployed_build_doc,
        get_live_build_doc,
        get_build_doc,
        download_build_doc,
        deploy_build_doc,
        list_context_variables_doc,
        create_context_variable_doc,
        update_context_variable_doc,
        delete_context_variable_doc,
        list_runs_doc,
        get_run_stats_doc,
        get_run_doc,
        publish_event_doc,
        list_events_doc,
        get_event_doc,
        get_event_runs_doc,
        list_project_event_handlers_doc,
        list_project_schedules_doc,
        create_session_doc,
        list_sessions_doc,
        revoke_all_sessions_doc,
        revoke_session_doc,
        create_service_key_doc,
        list_service_keys_doc,
        revoke_all_service_keys_doc,
        get_service_key_doc,
        update_service_key_doc,
        revoke_service_key_doc,
        create_domain_doc,
        list_domains_doc,
        get_domain_doc,
        delete_domain_doc,
        verify_domain_doc,
        get_env_info_doc,
        subscribe_to_env_doc,
        get_org_usage_doc,
        subscribe_to_stream_doc,
        subscribe_to_stream_post_doc,
        subscribe_with_event_doc,
        list_files_doc,
        get_file_doc,
        delete_file_doc,
        download_file_doc,
        upload_file_doc,
        initiate_upload_doc,
        upload_part_doc,
        complete_upload_doc,
        abort_upload_doc,
    ),
    components(schemas(
        ApiError,
        ApiErrorResponse,
        ApiListResponse<ProjectResponse>,
        ApiListResponse<BuildResponse>,
        ApiListResponse<BuildWithProjectResponse>,
        ApiListResponse<ContextVariableResponse>,
        ApiListResponse<RunResponse>,
        ApiListResponse<EventResponse>,
        ApiListResponse<EventHandlerResponse>,
        ApiListResponse<ScheduleResponse>,
        ApiListResponse<SessionResponse>,
        ApiListResponse<ServiceKeyResponse>,
        ApiListResponse<DomainResponse>,
        ApiListResponse<FileResponse>,
        ApiResponse<ProjectResponse>,
        ApiResponse<ProjectActivateResponse>,
        ApiResponse<BuildResponse>,
        ApiResponse<BuildUploadResponse>,
        ApiResponse<ContextVariableResponse>,
        ApiResponse<RunResponse>,
        ApiResponse<RunStatsResponse>,
        ApiResponse<EventResponse>,
        ApiResponse<SessionResponse>,
        ApiResponse<RevokeAllResponse>,
        ApiResponse<ServiceKeyResponse>,
        ApiResponse<RevokeAllServiceKeysResponse>,
        ApiResponse<DomainResponse>,
        ApiResponse<DomainVerifyResponse>,
        ApiResponse<EnvironmentResponse>,
        ApiResponse<OrgUsageResponse>,
        ApiResponse<FileResponse>,
        ApiResponse<InitiateUploadResponse>,
        ApiResponse<UploadPartResponse>,
        BinaryUploadBody,
        BuildResponse,
        BuildUploadResponse,
        BuildWithProjectResponse,
        ContextVariableResponse,
        CreateContextVariableRequest,
        CreateDomainRequest,
        CreateProjectRequest,
        CreateServiceKeyRequest,
        CreateSessionRequest,
        DomainResponse,
        DomainVerifyResponse,
        EnvironmentResponse,
        EnvSseEvent,
        EventHandlerResponse,
        EventPublishedEvent,
        EventResponse,
        FileResponse,
        InitiateUploadRequest,
        InitiateUploadResponse,
        Limits,
        OrgUsageResponse,
        PaginationMeta,
        PlanInfo,
        ProjectActivateResponse,
        ProjectResponse,
        PublishEventRequest,
        ResponseMeta,
        RevokeAllResponse,
        RevokeAllServiceKeysResponse,
        RunResponse,
        RunStatsResponse,
        ScheduleResponse,
        ServiceKeyResponse,
        SessionResponse,
        StatusResponse,
        StreamEvent,
        SubscribeWithEventRequest,
        UpdateContextVariableRequest,
        UpdateProjectRequest,
        UploadBuildMultipart,
        UploadPartResponse,
        UsagePercent,
        UsageStats,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "Status", description = "Health and metadata endpoints"),
        (name = "Projects", description = "Project management"),
        (name = "Builds", description = "Build listing, upload, download, and deployment"),
        (name = "Context", description = "Encrypted project context variables"),
        (name = "Runs", description = "Run tracking and observability"),
        (name = "Events", description = "Event publishing and lookup"),
        (name = "Sessions", description = "Short-lived scoped access tokens"),
        (name = "Service Keys", description = "Long-lived scoped service credentials"),
        (name = "Domains", description = "Custom domain management"),
        (name = "Env", description = "Environment info and real-time updates"),
        (name = "Org", description = "Organization usage and limits"),
        (name = "Streams", description = "SSE stream subscriptions"),
        (name = "Files", description = "File storage and multipart uploads"),
    )
)]
pub struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("API key, service key, or session token")
                        .build(),
                ),
            );
        }
    }
}

#[utoipa::path(
    get,
    path = "/status",
    tag = "Status",
    responses((status = 200, description = "API status", body = StatusResponse))
)]
fn status_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects",
    tag = "Projects",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Projects", body = ApiListResponse<ProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_projects_doc() {}

#[utoipa::path(
    post,
    path = "/v1/projects",
    tag = "Projects",
    request_body = CreateProjectRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Created project", body = ApiResponse<ProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn create_project_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}",
    tag = "Projects",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Project", body = ApiResponse<ProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_project_doc() {}

#[utoipa::path(
    patch,
    path = "/v1/projects/{project_id_or_slug}",
    tag = "Projects",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    request_body = UpdateProjectRequest,
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Updated project", body = ApiResponse<ProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn update_project_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/projects/{project_id_or_slug}",
    tag = "Projects",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Deleted project"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn delete_project_doc() {}

#[utoipa::path(
    post,
    path = "/v1/projects/{project_id_or_slug}/activate",
    tag = "Projects",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Activated project", body = ApiResponse<ProjectActivateResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn activate_project_doc() {}

#[utoipa::path(
    post,
    path = "/v1/projects/{project_id_or_slug}/deactivate",
    tag = "Projects",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Deactivated project", body = ApiResponse<ProjectActivateResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn deactivate_project_doc() {}

#[utoipa::path(
    get,
    path = "/v1/builds",
    tag = "Builds",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Builds across environment", body = ApiListResponse<BuildWithProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_builds_by_env_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/builds",
    tag = "Builds",
    params(
        ("project_id_or_slug" = String, Path, description = "Project UUID or name"),
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Project builds", body = ApiListResponse<BuildResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_builds_doc() {}

#[utoipa::path(
    post,
    path = "/v1/projects/{project_id_or_slug}/builds",
    tag = "Builds",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    request_body(content = UploadBuildMultipart, content_type = "multipart/form-data"),
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Uploaded build", body = ApiResponse<BuildUploadResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn upload_build_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/builds/deployed",
    tag = "Builds",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Deployed build", body = ApiResponse<BuildResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_deployed_build_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/builds/live",
    tag = "Builds",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Live build", body = ApiResponse<BuildResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_live_build_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/builds/{build_id}",
    tag = "Builds",
    params(
        ("project_id_or_slug" = String, Path, description = "Project UUID or name"),
        ("build_id" = Uuid, Path, description = "Build UUID")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Build", body = ApiResponse<BuildResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_build_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/builds/{build_id}/download",
    tag = "Builds",
    params(
        ("project_id_or_slug" = String, Path, description = "Project UUID or name"),
        ("build_id" = Uuid, Path, description = "Build UUID")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Build archive", content_type = "application/octet-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn download_build_doc() {}

#[utoipa::path(
    post,
    path = "/v1/projects/{project_id_or_slug}/builds/{build_id}/deploy",
    tag = "Builds",
    params(
        ("project_id_or_slug" = String, Path, description = "Project UUID or name"),
        ("build_id" = Uuid, Path, description = "Build UUID")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Deployed build", body = ApiResponse<BuildResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn deploy_build_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/context",
    tag = "Context",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Context variables", body = ApiListResponse<ContextVariableResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_context_variables_doc() {}

#[utoipa::path(
    post,
    path = "/v1/projects/{project_id_or_slug}/context",
    tag = "Context",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    request_body = CreateContextVariableRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Created context variable", body = ApiResponse<ContextVariableResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn create_context_variable_doc() {}

#[utoipa::path(
    put,
    path = "/v1/projects/{project_id_or_slug}/context/{key}",
    tag = "Context",
    params(
        ("project_id_or_slug" = String, Path, description = "Project UUID or name"),
        ("key" = String, Path, description = "Context variable key")
    ),
    request_body = UpdateContextVariableRequest,
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Updated context variable", body = ApiResponse<ContextVariableResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn update_context_variable_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/projects/{project_id_or_slug}/context/{key}",
    tag = "Context",
    params(
        ("project_id_or_slug" = String, Path, description = "Project UUID or name"),
        ("key" = String, Path, description = "Context variable key")
    ),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Deleted context variable"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn delete_context_variable_doc() {}

#[utoipa::path(
    get,
    path = "/v1/runs",
    tag = "Runs",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset"),
        ("status" = Option<String>, Query, description = "running|succeeded|failed|cancelled"),
        ("type" = Option<String>, Query, description = "call|event|schedule|run|eval|repl"),
        ("time_range" = Option<String>, Query, description = "ISO 8601 duration, e.g. P7D")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Runs", body = ApiListResponse<RunResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_runs_doc() {}

#[utoipa::path(
    get,
    path = "/v1/runs/stats",
    tag = "Runs",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Run stats", body = ApiResponse<RunStatsResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_run_stats_doc() {}

#[utoipa::path(
    get,
    path = "/v1/runs/{run_id}",
    tag = "Runs",
    params(("run_id" = Uuid, Path, description = "Run UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Run", body = ApiResponse<RunResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_run_doc() {}

#[utoipa::path(
    post,
    path = "/v1/events",
    tag = "Events",
    request_body = PublishEventRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Published event", body = ApiResponse<EventResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn publish_event_doc() {}

#[utoipa::path(
    get,
    path = "/v1/events",
    tag = "Events",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Events", body = ApiListResponse<EventResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_events_doc() {}

#[utoipa::path(
    get,
    path = "/v1/events/{event_id}",
    tag = "Events",
    params(("event_id" = Uuid, Path, description = "Event UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Event", body = ApiResponse<EventResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_event_doc() {}

#[utoipa::path(
    get,
    path = "/v1/events/{event_id}/runs",
    tag = "Events",
    params(("event_id" = Uuid, Path, description = "Event UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Runs for event", body = ApiListResponse<RunResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_event_runs_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/event-handlers",
    tag = "Events",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Project event handlers", body = ApiListResponse<EventHandlerResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_project_event_handlers_doc() {}

#[utoipa::path(
    get,
    path = "/v1/projects/{project_id_or_slug}/schedules",
    tag = "Events",
    params(("project_id_or_slug" = String, Path, description = "Project UUID or name")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Project schedules", body = ApiListResponse<ScheduleResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_project_schedules_doc() {}

#[utoipa::path(
    post,
    path = "/v1/sessions",
    tag = "Sessions",
    request_body = CreateSessionRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Created session", body = ApiResponse<SessionResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn create_session_doc() {}

#[utoipa::path(
    get,
    path = "/v1/sessions",
    tag = "Sessions",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Sessions", body = ApiListResponse<SessionResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_sessions_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/sessions",
    tag = "Sessions",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Revoked sessions", body = ApiResponse<RevokeAllResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn revoke_all_sessions_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/sessions/{session_id}",
    tag = "Sessions",
    params(("session_id" = Uuid, Path, description = "Session UUID")),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Revoked session"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn revoke_session_doc() {}

#[utoipa::path(
    post,
    path = "/v1/service-keys",
    tag = "Service Keys",
    request_body = CreateServiceKeyRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Created service key", body = ApiResponse<ServiceKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn create_service_key_doc() {}

#[utoipa::path(
    get,
    path = "/v1/service-keys",
    tag = "Service Keys",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Service keys", body = ApiListResponse<ServiceKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_service_keys_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/service-keys",
    tag = "Service Keys",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Revoked service keys", body = ApiResponse<RevokeAllServiceKeysResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn revoke_all_service_keys_doc() {}

#[utoipa::path(
    get,
    path = "/v1/service-keys/{service_key_id}",
    tag = "Service Keys",
    params(("service_key_id" = Uuid, Path, description = "Service key UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Service key", body = ApiResponse<ServiceKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_service_key_doc() {}

#[utoipa::path(
    patch,
    path = "/v1/service-keys/{service_key_id}",
    tag = "Service Keys",
    params(("service_key_id" = Uuid, Path, description = "Service key UUID")),
    request_body = crate::handlers::UpdateServiceKeyRequest,
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Updated service key", body = ApiResponse<ServiceKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn update_service_key_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/service-keys/{service_key_id}",
    tag = "Service Keys",
    params(("service_key_id" = Uuid, Path, description = "Service key UUID")),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Revoked service key"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn revoke_service_key_doc() {}

#[utoipa::path(
    post,
    path = "/v1/domains",
    tag = "Domains",
    request_body = CreateDomainRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Created domain", body = ApiResponse<DomainResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn create_domain_doc() {}

#[utoipa::path(
    get,
    path = "/v1/domains",
    tag = "Domains",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Domains", body = ApiListResponse<DomainResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_domains_doc() {}

#[utoipa::path(
    get,
    path = "/v1/domains/{domain_id}",
    tag = "Domains",
    params(("domain_id" = Uuid, Path, description = "Domain UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Domain", body = ApiResponse<DomainResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_domain_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/domains/{domain_id}",
    tag = "Domains",
    params(("domain_id" = Uuid, Path, description = "Domain UUID")),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Deleted domain"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn delete_domain_doc() {}

#[utoipa::path(
    post,
    path = "/v1/domains/{domain_id}/verify",
    tag = "Domains",
    params(("domain_id" = Uuid, Path, description = "Domain UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Verification status", body = ApiResponse<DomainVerifyResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn verify_domain_doc() {}

#[utoipa::path(
    get,
    path = "/v1/env",
    tag = "Env",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Environment info", body = ApiResponse<EnvironmentResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_env_info_doc() {}

#[utoipa::path(
    get,
    path = "/v1/env/subscribe",
    tag = "Env",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Environment SSE events", body = EnvSseEvent, content_type = "text/event-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn subscribe_to_env_doc() {}

#[utoipa::path(
    get,
    path = "/v1/org/usage",
    tag = "Org",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Organization usage", body = ApiResponse<OrgUsageResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_org_usage_doc() {}

#[utoipa::path(
    get,
    path = "/v1/streams/{stream_id}/subscribe",
    tag = "Streams",
    params(
        ("stream_id" = Uuid, Path, description = "Stream UUID"),
        ("project" = Option<String>, Query, description = "Optional project filter")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Stream SSE events", body = StreamEvent, content_type = "text/event-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn subscribe_to_stream_doc() {}

#[utoipa::path(
    post,
    path = "/v1/streams/{stream_id}/subscribe",
    tag = "Streams",
    params(
        ("stream_id" = Uuid, Path, description = "Stream UUID"),
        ("project" = Option<String>, Query, description = "Optional project filter")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Streamable HTTP SSE events", body = StreamEvent, content_type = "text/event-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn subscribe_to_stream_post_doc() {}

#[utoipa::path(
    post,
    path = "/v1/streams/subscribe-with-event",
    tag = "Streams",
    params(("project" = Option<String>, Query, description = "Optional project filter")),
    request_body = SubscribeWithEventRequest,
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Event publication and stream SSE events", body = EventPublishedEvent, content_type = "text/event-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn subscribe_with_event_doc() {}

#[utoipa::path(
    get,
    path = "/v1/files",
    tag = "Files",
    params(
        ("limit" = Option<i64>, Query, description = "Max results"),
        ("offset" = Option<i64>, Query, description = "Pagination offset"),
        ("prefix" = Option<String>, Query, description = "Path prefix")
    ),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Files", body = ApiListResponse<FileResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn list_files_doc() {}

#[utoipa::path(
    get,
    path = "/v1/files/{file_id}",
    tag = "Files",
    params(("file_id" = Uuid, Path, description = "File UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "File", body = ApiResponse<FileResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_file_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/files/{file_id}",
    tag = "Files",
    params(("file_id" = Uuid, Path, description = "File UUID")),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Deleted file"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn delete_file_doc() {}

#[utoipa::path(
    get,
    path = "/v1/files/{file_id}/download",
    tag = "Files",
    params(("file_id" = Uuid, Path, description = "File UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "File bytes", content_type = "application/octet-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn download_file_doc() {}

#[utoipa::path(
    put,
    path = "/v1/files/upload/{path}",
    tag = "Files",
    params(("path" = String, Path, description = "Target file path")),
    request_body(content = BinaryUploadBody, content_type = "application/octet-stream"),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Uploaded file", body = ApiResponse<FileResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn upload_file_doc() {}

#[utoipa::path(
    post,
    path = "/v1/files/uploads",
    tag = "Files",
    request_body = InitiateUploadRequest,
    security(("bearer_auth" = [])),
    responses((status = 201, description = "Initiated multipart upload", body = ApiResponse<InitiateUploadResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn initiate_upload_doc() {}

#[utoipa::path(
    put,
    path = "/v1/files/uploads/{upload_id}/{part_number}",
    tag = "Files",
    params(
        ("upload_id" = Uuid, Path, description = "Upload UUID"),
        ("part_number" = i32, Path, description = "Part number")
    ),
    request_body(content = BinaryUploadBody, content_type = "application/octet-stream"),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Uploaded part", body = ApiResponse<UploadPartResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn upload_part_doc() {}

#[utoipa::path(
    post,
    path = "/v1/files/uploads/{upload_id}/complete",
    tag = "Files",
    params(("upload_id" = Uuid, Path, description = "Upload UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Completed upload", body = ApiResponse<FileResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn complete_upload_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/files/uploads/{upload_id}",
    tag = "Files",
    params(("upload_id" = Uuid, Path, description = "Upload UUID")),
    security(("bearer_auth" = [])),
    responses((status = 204, description = "Aborted upload"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn abort_upload_doc() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_contains_core_v1_routes() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("serialize OpenAPI document");
        let paths = doc["paths"].as_object().expect("OpenAPI paths object");

        for path in [
            "/v1/projects",
            "/v1/builds",
            "/v1/runs/stats",
            "/v1/events",
            "/v1/sessions",
            "/v1/service-keys",
            "/v1/env/subscribe",
            "/v1/org/usage",
            "/v1/streams/{stream_id}/subscribe",
            "/v1/streams/subscribe-with-event",
            "/v1/files",
            "/v1/files/uploads",
        ] {
            assert!(paths.contains_key(path), "missing OpenAPI path: {path}");
        }

        assert!(
            doc["components"]["securitySchemes"]
                .as_object()
                .is_some_and(|schemes| schemes.contains_key("bearer_auth")),
            "missing bearer_auth security scheme"
        );
    }
}
