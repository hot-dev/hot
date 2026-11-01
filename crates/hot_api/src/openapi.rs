//! OpenAPI document for the Hot API.
#![allow(dead_code)]

use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};
use uuid::Uuid;

use crate::handlers::orgs::{
    ApiKeyResponse, CreateApiKeyRequest, CreateEnvRequest, CreateOrgRequest, CreateOrgResponse,
    OrgEnvResponse, OrgResponse, RotateApiKeyRequest, UpdateApiKeyRequest, UpdateOrgRequest,
};
use crate::handlers::{
    CallPurgeResponse, CreateDomainRequest, CreateServiceKeyRequest, CreateSessionRequest,
    DomainResponse, DomainVerifyResponse, EnvSseEvent, EventPublishedEvent, Limits,
    OrgUsageResponse, PlanInfo, RevokeAllResponse, RevokeAllServiceKeysResponse, RunEvent,
    ServiceKeyResponse, SessionResponse, StreamEvent, SubscribeWithEventRequest, TaskEvent,
    UsagePercent, UsageStats,
};
use crate::models::{
    ApiError, ApiErrorResponse, ApiListResponse, ApiResponse, BuildResponse, BuildUploadResponse,
    BuildWithProjectResponse, ContextVariableResponse, CreateContextVariableRequest,
    CreateProjectRequest, EnvironmentResponse, EventHandlerResponse, EventResponse, FileResponse,
    InitiateUploadRequest, InitiateUploadResponse, PaginationMeta, ProjectActivateResponse,
    ProjectResponse, PublishEventRequest, ResponseMeta, RunResponse, RunStatsResponse,
    ScheduleResponse, TaskResponse, UpdateContextVariableRequest, UpdateProjectRequest,
    UploadPartResponse,
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
        orgs_list_doc,
        orgs_create_doc,
        orgs_get_doc,
        orgs_update_doc,
        orgs_usage_doc,
        orgs_purge_calls_doc,
        orgs_list_envs_doc,
        orgs_create_env_doc,
        orgs_get_env_doc,
        orgs_list_env_keys_doc,
        orgs_mint_env_key_doc,
        orgs_list_org_keys_doc,
        orgs_mint_org_key_doc,
        api_keys_get_doc,
        api_keys_update_doc,
        api_keys_revoke_doc,
        api_keys_rotate_doc,
        orgs_env_projects_doc,
        orgs_env_builds_doc,
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
        subscribe_to_run_doc,
        purge_run_calls_doc,
        get_task_doc,
        subscribe_to_task_doc,
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
        purge_env_calls_doc,
        subscribe_to_env_doc,
        get_org_usage_doc,
        subscribe_to_stream_doc,
        purge_stream_calls_doc,
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
        download_blob_doc,
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
        ApiListResponse<OrgResponse>,
        ApiListResponse<OrgEnvResponse>,
        ApiListResponse<ApiKeyResponse>,
        ApiResponse<OrgResponse>,
        ApiResponse<CreateOrgResponse>,
        ApiResponse<OrgEnvResponse>,
        ApiResponse<ApiKeyResponse>,
        ApiResponse<ProjectResponse>,
        ApiResponse<ProjectActivateResponse>,
        ApiResponse<BuildResponse>,
        ApiResponse<BuildUploadResponse>,
        ApiResponse<ContextVariableResponse>,
        ApiResponse<RunResponse>,
        ApiResponse<RunStatsResponse>,
        ApiResponse<CallPurgeResponse>,
        ApiResponse<TaskResponse>,
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
        ApiKeyResponse,
        BinaryUploadBody,
        BuildResponse,
        BuildUploadResponse,
        BuildWithProjectResponse,
        CallPurgeResponse,
        ContextVariableResponse,
        CreateApiKeyRequest,
        CreateContextVariableRequest,
        CreateEnvRequest,
        CreateOrgRequest,
        CreateOrgResponse,
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
        OrgEnvResponse,
        OrgResponse,
        OrgUsageResponse,
        PaginationMeta,
        PlanInfo,
        ProjectActivateResponse,
        ProjectResponse,
        PublishEventRequest,
        ResponseMeta,
        RevokeAllResponse,
        RevokeAllServiceKeysResponse,
        RotateApiKeyRequest,
        RunResponse,
        RunStatsResponse,
        RunEvent,
        ScheduleResponse,
        ServiceKeyResponse,
        SessionResponse,
        StatusResponse,
        StreamEvent,
        SubscribeWithEventRequest,
        TaskEvent,
        TaskResponse,
        UpdateApiKeyRequest,
        UpdateContextVariableRequest,
        UpdateOrgRequest,
        UpdateProjectRequest,
        UploadBuildMultipart,
        UploadPartResponse,
        UsagePercent,
        UsageStats,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "Status", description = "Health and metadata endpoints"),
        (name = "Orgs", description = "Organizations, environments and API keys, for org-scoped API keys"),
        (name = "Projects", description = "Project management"),
        (name = "Builds", description = "Build listing, upload, download, and deployment"),
        (name = "Context", description = "Encrypted project context variables"),
        (name = "Runs", description = "Run tracking and observability"),
        (name = "Tasks", description = "Asynchronous task status and completion"),
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
                        .bearer_format("API key (org- or environment-scoped), service key, or session token. Org keys call /v1/orgs and /v1/api-keys; every other route needs an environment credential")
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
    get,
    path = "/v1/runs/{run_id}/subscribe",
    tag = "Runs",
    params(("run_id" = Uuid, Path, description = "Run UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "SSE run snapshots until terminal state", body = RunEvent, content_type = "text/event-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn subscribe_to_run_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/runs/{run_id}/calls",
    tag = "Runs",
    description = "Delete a run's call traces (`call:{run_id}/*` with `delete`). The run itself remains. A task is its own run: pass its task ID.",
    params(("run_id" = Uuid, Path, description = "Run UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Calls deleted, payloads released, and whether the purge finished; repeat until `complete` is true", body = ApiResponse<CallPurgeResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn purge_run_calls_doc() {}

#[utoipa::path(
    get,
    path = "/v1/tasks/{task_id}",
    tag = "Tasks",
    params(("task_id" = Uuid, Path, description = "Task UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Task snapshot", body = ApiResponse<TaskResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn get_task_doc() {}

#[utoipa::path(
    get,
    path = "/v1/tasks/{task_id}/subscribe",
    tag = "Tasks",
    params(("task_id" = Uuid, Path, description = "Task UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "SSE task snapshots until terminal state", body = TaskEvent, content_type = "text/event-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn subscribe_to_task_doc() {}

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
    delete,
    path = "/v1/env/calls",
    tag = "Env",
    description = "Delete every call trace in the environment (`call:*` with `delete`). Runs remain.",
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Calls deleted, payloads released, and whether the purge finished; repeat until `complete` is true", body = ApiResponse<CallPurgeResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn purge_env_calls_doc() {}

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
    delete,
    path = "/v1/streams/{stream_id}/calls",
    tag = "Streams",
    description = "Delete the call traces of every run in a stream, including the tasks those runs started (`call:*` with `delete`). Runs remain.",
    params(("stream_id" = Uuid, Path, description = "Stream UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Calls deleted, payloads released, and whether the purge finished; repeat until `complete` is true", body = ApiResponse<CallPurgeResponse>), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn purge_stream_calls_doc() {}

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

#[utoipa::path(
    get,
    path = "/v1/blobs/{blob_ref_id}/download",
    tag = "Files",
    description = "Raw bytes of a large payload spilled to blob storage, in the caller's environment (`blob:{blob_ref_id}` with `read`). API keys only; string payloads are served as UTF-8 text.",
    params(("blob_ref_id" = Uuid, Path, description = "Blob reference UUID")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Blob bytes", content_type = "application/octet-stream"), (status = "default", description = "Error", body = ApiErrorResponse))
)]
fn download_blob_doc() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_contains_core_v1_routes() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("serialize OpenAPI document");
        let paths = doc["paths"].as_object().expect("OpenAPI paths object");

        for path in [
            "/v1/projects",
            "/v1/orgs",
            "/v1/orgs/{org_id}",
            "/v1/orgs/{org_id}/usage",
            "/v1/orgs/{org_id}/envs",
            "/v1/orgs/{org_id}/envs/{env_id}",
            "/v1/orgs/{org_id}/envs/{env_id}/api-keys",
            "/v1/orgs/{org_id}/envs/{env_id}/projects",
            "/v1/orgs/{org_id}/api-keys",
            "/v1/api-keys/{key_id}",
            "/v1/api-keys/{key_id}/rotate",
            "/v1/builds",
            "/v1/runs/stats",
            "/v1/runs/{run_id}/subscribe",
            "/v1/tasks/{task_id}",
            "/v1/tasks/{task_id}/subscribe",
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

    /// OpenAPI 3.1 requires a description on every response, and strict
    /// generators reject a document whose `$ref`s do not resolve.
    #[test]
    fn openapi_document_is_valid_for_strict_tooling() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("serialize OpenAPI document");
        let schemas = doc["components"]["schemas"]
            .as_object()
            .expect("component schemas");
        for (path, item) in doc["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                for (status, response) in operation["responses"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{method} {path} has no responses"))
                {
                    assert!(
                        response["description"]
                            .as_str()
                            .is_some_and(|text| !text.is_empty()),
                        "{method} {path} {status}: response without a description"
                    );
                }
            }
        }
        fn refs<'a>(value: &'a serde_json::Value, found: &mut Vec<&'a str>) {
            match value {
                serde_json::Value::Object(map) => {
                    if let Some(target) = map.get("$ref").and_then(|v| v.as_str()) {
                        found.push(target);
                    }
                    map.values().for_each(|v| refs(v, found));
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| refs(v, found)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        refs(&doc, &mut found);
        for target in found {
            let name = target
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected $ref {target}"));
            assert!(schemas.contains_key(name), "unresolved $ref {target}");
        }
    }

    /// The OpenAPI document of the last release. Replace it with the new
    /// release's `/openapi.json` when tagging, and empty the allowlists below.
    const RELEASED: &str = include_str!("../tests/fixtures/openapi-v3.0.0.json");

    /// Intentional removals since the last release, each listed in the
    /// release's upgrade notes. Hot 3.0.1 replaced 3.0.0's `/v1/management`
    /// API and its `hm_` credentials with org API keys.
    const REMOVED_OPERATIONS: &[(&str, &str)] = &[
        ("get", "/v1/management/children"),
        ("post", "/v1/management/children"),
        ("post", "/v1/management/children/{child_id}/environments"),
        (
            "post",
            "/v1/management/children/{child_id}/environments/{env_id}/api-keys",
        ),
        ("patch", "/v1/management/children/{child_id}/state"),
        ("get", "/v1/management/usage"),
    ];
    const REMOVED_SECURITY_SCHEMES: &[&str] = &["management_auth"];
    const REMOVED_SCHEMAS: &[&str] = &[
        "ChildStateRequest",
        "CreateChildRequest",
        "CreateEnvironmentRequest",
        "IssueRuntimeKeyRequest",
    ];

    fn names(value: &serde_json::Value) -> std::collections::BTreeSet<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A patch must not break clients of the last release: no operation,
    /// response status, security scheme or schema may disappear, a response
    /// schema keeps its properties and required fields, and a request schema
    /// gains no required field. Removals need an entry in the allowlists above.
    #[test]
    fn public_api_keeps_the_last_release_surface() {
        let released: serde_json::Value =
            serde_json::from_str(RELEASED).expect("parse released OpenAPI document");
        let current = serde_json::to_value(ApiDoc::openapi()).expect("serialize OpenAPI document");
        let mut breaks = Vec::new();

        let mut request_schemas = std::collections::BTreeSet::new();
        for (path, item) in released["paths"].as_object().expect("released paths") {
            for (method, operation) in item.as_object().expect("path item") {
                if let Some(target) =
                    operation["requestBody"]["content"]["application/json"]["schema"]["$ref"]
                        .as_str()
                {
                    request_schemas.insert(target.trim_start_matches("#/components/schemas/"));
                }
                if REMOVED_OPERATIONS.contains(&(method.as_str(), path.as_str())) {
                    continue;
                }
                let now = &current["paths"][path][method];
                if now.is_null() {
                    breaks.push(format!("operation removed: {method} {path}"));
                    continue;
                }
                for status in operation["responses"].as_object().into_iter().flatten() {
                    if now["responses"][status.0].is_null() {
                        breaks.push(format!("response removed: {method} {path} {}", status.0));
                    }
                }
            }
        }

        for name in released["components"]["securitySchemes"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, _)| name)
        {
            if !REMOVED_SECURITY_SCHEMES.contains(&name.as_str())
                && current["components"]["securitySchemes"][name].is_null()
            {
                breaks.push(format!("security scheme removed: {name}"));
            }
        }

        for (name, schema) in released["components"]["schemas"]
            .as_object()
            .expect("released schemas")
        {
            if REMOVED_SCHEMAS.contains(&name.as_str()) {
                continue;
            }
            let now = &current["components"]["schemas"][name];
            if now.is_null() {
                breaks.push(format!("schema removed: {name}"));
                continue;
            }
            for property in schema["properties"].as_object().into_iter().flatten() {
                if now["properties"][property.0].is_null() {
                    breaks.push(format!("property removed: {name}.{}", property.0));
                }
            }
            let (was, is) = (names(&schema["required"]), names(&now["required"]));
            if request_schemas.contains(name.as_str()) {
                for field in is.difference(&was) {
                    breaks.push(format!("request field became required: {name}.{field}"));
                }
            } else {
                for field in was.difference(&is) {
                    breaks.push(format!("response field became optional: {name}.{field}"));
                }
            }
        }

        assert!(
            breaks.is_empty(),
            "the public API changed incompatibly since the last release:\n{}",
            breaks.join("\n")
        );
    }

    /// The org API's responses are typed, so generated clients get models.
    #[test]
    fn org_operations_reference_response_schemas() {
        let doc = serde_json::to_value(ApiDoc::openapi()).expect("serialize OpenAPI document");
        for (path, item) in doc["paths"].as_object().expect("paths") {
            if !(path.starts_with("/v1/orgs") || path.starts_with("/v1/api-keys")) {
                continue;
            }
            for (method, operation) in item.as_object().expect("path item") {
                for (status, response) in operation["responses"].as_object().unwrap() {
                    if status == "204" || path.ends_with("/usage") {
                        continue;
                    }
                    let schema = &response["content"]["application/json"]["schema"];
                    assert!(
                        schema.get("$ref").is_some(),
                        "{method} {path} {status}: untyped response {schema}"
                    );
                }
            }
        }
    }
}

#[utoipa::path(get, path = "/v1/orgs", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Sub-organizations of the key's organization that it may read (`org:*` or `org:{org}` with `read`).",
    params(("limit" = Option<i64>, Query, description = "Max results (default 20, maximum 500)"), ("offset" = Option<i64>, Query, description = "Pagination offset")),
    responses((status = 200, description = "Sub-organizations", body = ApiListResponse<OrgResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_list_doc() {}

#[utoipa::path(post, path = "/v1/orgs", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Create a sub-organization and its default environment (`org:*` with `create`). Idempotent by `idempotency_key`: a replay returns the original sub-organization with `created: false` and status 200. A slug taken by any account answers 409 `slug_taken`; an `external_id` another sub-organization of the account uses answers 409 `external_id_taken`.",
    request_body = crate::handlers::orgs::CreateOrgRequest,
    responses(
        (status = 201, description = "Sub-organization and its default environment", body = ApiResponse<CreateOrgResponse>),
        (status = 200, description = "Replay of an earlier request with the same idempotency key", body = ApiResponse<CreateOrgResponse>),
        (status = 409, description = "`slug_taken` or `external_id_taken`", body = ApiErrorResponse),
        (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_create_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}", tag = "Orgs", security(("bearer_auth" = [])),
    description = "The key's organization or one of its sub-organizations (`org:{org}` with `read`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID")),
    responses((status = 200, description = "Organization", body = ApiResponse<OrgResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_get_doc() {}

#[utoipa::path(patch, path = "/v1/orgs/{org_id}", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Change a sub-organization's lifecycle state (`org:{org}` with `update`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID")), request_body = crate::handlers::orgs::UpdateOrgRequest,
    responses((status = 200, description = "Updated organization", body = ApiResponse<OrgResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_update_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/usage", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Account usage across the parent organization and its sub-organizations (`org:{parent}/usage:*` with `read`). The response has `org_id`, `usage_scope: \"account\"`, `period_start`, `period_end` and `usage`.",
    params(("org_id" = Uuid, Path, description = "Organization UUID")),
    responses((status = 200, description = "Account usage", body = serde_json::Value), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_usage_doc() {}

#[utoipa::path(delete, path = "/v1/orgs/{org_id}/calls", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Delete every call trace in an organization's environments, and its sub-organizations' with `include_sub_orgs` (`org:{org}/env:*/call:*` with `delete` for each organization). Runs remain. Repeat until `complete` is true.",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), crate::handlers::orgs::PurgeCallsQuery),
    responses((status = 200, description = "Calls deleted, payloads released, and whether the purge finished", body = ApiResponse<CallPurgeResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_purge_calls_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/envs", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Environments of an organization (`org:{org}/env:*` with `read`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("limit" = Option<i64>, Query, description = "Max results (default 20, maximum 500)"), ("offset" = Option<i64>, Query, description = "Pagination offset")),
    responses((status = 200, description = "Environments", body = ApiListResponse<OrgEnvResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_list_envs_doc() {}

#[utoipa::path(post, path = "/v1/orgs/{org_id}/envs", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Create an environment (`org:{org}/env:*` with `create`). Idempotent by exact name: an existing environment with the name is returned with status 200.",
    params(("org_id" = Uuid, Path, description = "Organization UUID")), request_body = crate::handlers::orgs::CreateEnvRequest,
    responses(
        (status = 201, description = "Environment created", body = ApiResponse<OrgEnvResponse>),
        (status = 200, description = "Existing environment with that name", body = ApiResponse<OrgEnvResponse>),
        (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_create_env_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/envs/{env_id}", tag = "Orgs", security(("bearer_auth" = [])),
    description = "One environment (`org:{org}/env:{env}` with `read`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("env_id" = Uuid, Path, description = "Environment UUID")),
    responses((status = 200, description = "Environment", body = ApiResponse<OrgEnvResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_get_env_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/envs/{env_id}/api-keys", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Environment API keys, newest first (`org:{org}/env:{env}/api-key:*` with `read`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("env_id" = Uuid, Path, description = "Environment UUID"), ("limit" = Option<i64>, Query, description = "Max results (default 20, maximum 500)"), ("offset" = Option<i64>, Query, description = "Pagination offset")),
    responses((status = 200, description = "API keys, without secrets", body = ApiListResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_list_env_keys_doc() {}

#[utoipa::path(post, path = "/v1/orgs/{org_id}/envs/{env_id}/api-keys", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Mint an environment API key (`org:{org}/env:{env}/api-key:*` with `create`). Its grants must fit inside the org key's grants for that environment, and its expiry inside the org key's.",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("env_id" = Uuid, Path, description = "Environment UUID")), request_body = crate::handlers::orgs::CreateApiKeyRequest,
    responses((status = 201, description = "Environment API key; token returned once", body = ApiResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_mint_env_key_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/api-keys", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Org API keys of the key's own organization, newest first (`org:{org}/api-key:*` with `read`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("limit" = Option<i64>, Query, description = "Max results (default 20, maximum 500)"), ("offset" = Option<i64>, Query, description = "Pagination offset")),
    responses((status = 200, description = "Org API keys, without secrets", body = ApiListResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_list_org_keys_doc() {}

#[utoipa::path(post, path = "/v1/orgs/{org_id}/api-keys", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Mint an org API key in the key's own organization (`org:{org}/api-key:*` with `create`). Its grants must fit inside the issuer's; chains are at most two org keys deep.",
    params(("org_id" = Uuid, Path, description = "Organization UUID")), request_body = crate::handlers::orgs::CreateApiKeyRequest,
    responses((status = 201, description = "Org API key; token returned once", body = ApiResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_mint_org_key_doc() {}

#[utoipa::path(get, path = "/v1/api-keys/{key_id}", tag = "Orgs", security(("bearer_auth" = [])),
    description = "An org or environment API key in the caller's organization or a sub-organization (`api-key:{key}` with `read`, qualified by its org and environment).",
    params(("key_id" = Uuid, Path, description = "Org or environment API key UUID")),
    responses((status = 200, description = "API key, without its secret", body = ApiResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn api_keys_get_doc() {}

#[utoipa::path(patch, path = "/v1/api-keys/{key_id}", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Replace a key's grants with ones inside the caller's own (`update`). The caller must have issued the key, directly or through keys it issued.",
    params(("key_id" = Uuid, Path, description = "Org or environment API key UUID")), request_body = crate::handlers::orgs::UpdateApiKeyRequest,
    responses((status = 200, description = "Updated API key", body = ApiResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn api_keys_update_doc() {}

#[utoipa::path(delete, path = "/v1/api-keys/{key_id}", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Revoke a key the caller issued (`delete`). Keys it issued stop working too. Revoking an org key is permanent; an environment key can be re-enabled in the app.",
    params(("key_id" = Uuid, Path, description = "Org or environment API key UUID")),
    responses((status = 204, description = "Revoked"), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn api_keys_revoke_doc() {}

#[utoipa::path(post, path = "/v1/api-keys/{key_id}/rotate", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Issue a new secret for a key the caller issued (`update`). The key ID, lineage and keys it issued are unchanged; the previous secret works until the overlap ends.",
    params(("key_id" = Uuid, Path, description = "Org or environment API key UUID")), request_body(content = Option<crate::handlers::orgs::RotateApiKeyRequest>),
    responses((status = 200, description = "API key with its new token", body = ApiResponse<ApiKeyResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn api_keys_rotate_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/envs/{env_id}/projects", tag = "Orgs", security(("bearer_auth" = [])),
    description = "Every `/v1/projects…` route is also served under `/v1/orgs/{org_id}/envs/{env_id}` for org keys, with the same request and response as the environment route. The org key's grants for that environment (`org:{org}/env:{env}/project:*`, `…/context:*`, `…/build:*`) are checked exactly as an environment key's grants are.",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("env_id" = Uuid, Path, description = "Environment UUID"), ("limit" = Option<i64>, Query, description = "Max results (default 20, maximum 500)"), ("offset" = Option<i64>, Query, description = "Pagination offset")),
    responses((status = 200, description = "Projects in the environment", body = ApiListResponse<ProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_env_projects_doc() {}

#[utoipa::path(get, path = "/v1/orgs/{org_id}/envs/{env_id}/builds", tag = "Orgs", security(("bearer_auth" = [])),
    description = "`GET /v1/builds` for org keys, in one environment (`org:{org}/env:{env}/build:*` with `read`).",
    params(("org_id" = Uuid, Path, description = "Organization UUID"), ("env_id" = Uuid, Path, description = "Environment UUID"), ("limit" = Option<i64>, Query, description = "Max results (default 20, maximum 500)"), ("offset" = Option<i64>, Query, description = "Pagination offset")),
    responses((status = 200, description = "Builds across the environment", body = ApiListResponse<BuildWithProjectResponse>), (status = "default", description = "Error", body = ApiErrorResponse)))]
fn orgs_env_builds_doc() {}
