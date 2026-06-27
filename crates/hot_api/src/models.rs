//! API Data Transfer Objects (DTOs)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

// ============================================================================
// Response Wrappers
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiResponse<T> {
    pub data: T,
    pub meta: ResponseMeta,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiListResponse<T> {
    pub data: Vec<T>,
    pub pagination: PaginationMeta,
    pub meta: ResponseMeta,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiErrorResponse {
    pub error: ApiError,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResponseMeta {
    pub request_id: Uuid,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PaginationMeta {
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub request_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

// ============================================================================
// Project DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectResponse {
    pub project_id: Uuid,
    pub env_id: Uuid,
    pub name: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectActivateResponse {
    pub project: ProjectResponse,
    /// If activation triggered a redeploy of the latest build, this is its id.
    /// `None` when deactivating, or when the project has no builds yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redeployed_build_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    pub name: String,
}

// ============================================================================
// Build DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct BuildRuntimeWarningResponse {
    pub build_engine_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_hot_std_version: Option<String>,
    pub runtime_version: String,
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BuildResponse {
    pub build_id: Uuid,
    pub project_id: Uuid,
    pub hash: String,
    pub size: i32,
    pub build_type: String,
    pub deployed: bool,
    pub active: bool,
    pub runtime_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_error: Option<String>,
    pub deployment_sequence: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_warning: Option<BuildRuntimeWarningResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BuildWithProjectResponse {
    pub build_id: Uuid,
    pub project_id: Uuid,
    pub project_name: String,
    pub hash: String,
    pub size: i32,
    pub build_type: String,
    pub deployed: bool,
    pub active: bool,
    pub runtime_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_error: Option<String>,
    pub deployment_sequence: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_warning: Option<BuildRuntimeWarningResponse>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateBuildRequest {
    pub hash: String,
    pub size: i32,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UploadBuildRequest {
    // Multipart form field names
    // - file: the build zip file
    // - hash: the build hash for validation
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BuildUploadResponse {
    pub build_id: Uuid,
    pub project_id: Uuid,
    pub hash: String,
    pub size: i32,
    pub storage_path: String,
    pub storage_backend: String,
    pub created_at: DateTime<Utc>,
}

// ============================================================================
// Context Variable DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct ContextVariableResponse {
    pub key: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateContextVariableRequest {
    pub key: String,
    pub value: String, // Plain text - will be encrypted
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateContextVariableRequest {
    pub value: String, // Plain text - will be encrypted
    pub description: Option<String>,
}

// ============================================================================
// Run DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct RunResponse {
    pub run_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub build_id: Option<Uuid>,
    pub run_type: String,
    pub status: String,
    pub start_time: DateTime<Utc>,
    pub stop_time: Option<DateTime<Utc>>,
    pub origin_run_id: Option<Uuid>,
    pub event_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    // Retry fields
    pub retry_attempt: i16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RunStatsResponse {
    pub total_runs: i64,
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub cancelled: i64,
}

// ============================================================================
// Event DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct EventResponse {
    pub event_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub event_data: serde_json::Value,
    pub event_time: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PublishEventRequest {
    pub event_type: String,
    pub event_data: serde_json::Value,
    /// Optional stream ID to add this event to an existing stream.
    /// If not provided, a new stream will be created.
    #[serde(default)]
    pub stream_id: Option<uuid::Uuid>,
}

// ============================================================================
// Event Handler DTOs (Read-only)
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct EventHandlerResponse {
    pub event_handler_id: Uuid,
    pub build_id: Uuid,
    pub event_type: String,
    pub ns: String,
    pub var: String,
}

// ============================================================================
// Schedule DTOs (Read-only)
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleResponse {
    pub schedule_id: Uuid,
    pub build_id: Uuid,
    pub cron: String,
    pub ns: String,
    pub var: String,
}

// ============================================================================
// Environment DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentResponse {
    pub env_id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub active: bool,
}

// ============================================================================
// Helper functions
// ============================================================================

impl ResponseMeta {
    pub fn new() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            timestamp: Utc::now(),
        }
    }
}

impl Default for ResponseMeta {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ApiResponse<T> {
    pub fn new(data: T) -> Self {
        Self {
            data,
            meta: ResponseMeta::new(),
        }
    }
}

impl<T> ApiListResponse<T> {
    pub fn new(data: Vec<T>, total: i64, limit: i64, offset: i64) -> Self {
        let has_more = offset + limit < total;
        Self {
            data,
            pagination: PaginationMeta {
                total,
                limit,
                offset,
                has_more,
            },
            meta: ResponseMeta::new(),
        }
    }
}

impl ApiErrorResponse {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ApiError {
                code: code.into(),
                message: message.into(),
                request_id: Uuid::new_v4(),
                retry_after: None,
            },
        }
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.error.retry_after = Some(seconds);
        self
    }

    pub fn not_found(resource: &str) -> Self {
        Self::new("not_found", format!("{} not found", resource))
    }

    pub fn unauthorized(message: &str) -> Self {
        Self::new("unauthorized", message)
    }

    pub fn bad_request(message: &str) -> Self {
        Self::new("bad_request", message)
    }

    pub fn internal_error(message: &str) -> Self {
        tracing::error!("internal server error: {}", message);
        Self::new("internal_server_error", "An internal server error occurred")
    }

    pub fn rate_limit_exceeded(retry_after: u64) -> Self {
        Self::new(
            "rate_limit_exceeded",
            format!("Rate limit exceeded. Try again in {} seconds", retry_after),
        )
        .with_retry_after(retry_after)
    }
}

// ============================================================================
// File DTOs
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct FileResponse {
    pub file_id: Uuid,
    pub path: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub storage_backend: String,
    pub created_by_run_id: Option<Uuid>,
    pub updated_by_run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FileListQueryParams {
    #[serde(
        default = "default_file_limit",
        deserialize_with = "crate::handlers::deserialize_clamped_limit"
    )]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub prefix: Option<String>,
}

fn default_file_limit() -> i64 {
    20
}

// ============================================================================
// Multipart Upload DTOs
// ============================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct InitiateUploadRequest {
    pub path: String,
    #[serde(default)]
    pub expected_size: Option<i64>,
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InitiateUploadResponse {
    pub upload_id: Uuid,
    pub path: String,
    pub part_size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts_expected: Option<i32>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UploadPartResponse {
    pub part_number: i32,
    pub size: i64,
    pub etag: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UploadStatusResponse {
    pub upload_id: Uuid,
    pub path: String,
    pub status: String,
    pub parts_received: i32,
    pub bytes_received: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts_expected: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_size: Option<i64>,
}
