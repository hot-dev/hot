//! Custom domain management handlers
//!
//! Custom domains allow Hot Dev customers to map their own domains
//! (e.g., mcp.example.com) to their Hot Dev environments.
//! This feature is gated to Pro+ subscription plans.
//!
//! Domain provisioning is delegated to the configured domain provider. The
//! customer adds 2 DNS records:
//! 1. Certificate validation CNAME (proves ownership)
//! 2. Traffic CNAME pointing to the provider routing target

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use hot::db::Features;
use hot::db::api_key::ApiKey;
use hot::db::domain::{Domain, DomainStatus};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::ApiStateData;
use crate::handlers::get_org_id_for_env;
use crate::models::*;

// ============================================================================
// Request/Response DTOs
// ============================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDomainRequest {
    /// The custom domain (e.g., "mcp.example.com")
    pub domain: String,
}

/// A DNS record the customer needs to create.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub record_type: String,
    pub name: String,
    pub value: String,
    pub purpose: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DomainResponse {
    pub domain_id: Uuid,
    pub env_id: Uuid,
    pub domain: String,
    #[schema(value_type = String)]
    pub status: DomainStatus,
    /// DNS records the customer needs to create.
    pub dns_records: Vec<DnsRecord>,
    /// Provider routing domain (available after provisioning).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_domain: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_provisioned_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisioning_error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DomainVerifyResponse {
    pub domain_id: Uuid,
    pub domain: String,
    #[schema(value_type = String)]
    pub status: DomainStatus,
    pub message: String,
}

// ============================================================================
// Helpers
// ============================================================================

fn domain_to_response(d: &Domain) -> DomainResponse {
    let mut dns_records = Vec::new();

    // Certificate validation CNAME (always shown until verified).
    if let (Some(cname_name), Some(cname_value)) =
        (&d.validation_cname_name, &d.validation_cname_value)
    {
        dns_records.push(DnsRecord {
            record_type: "CNAME".to_string(),
            name: cname_name.clone(),
            value: cname_value.clone(),
            purpose: "certificate_validation".to_string(),
        });
    }

    // Traffic CNAME (shown once routing is available).
    if let Some(cf_domain) = &d.routing_domain {
        dns_records.push(DnsRecord {
            record_type: "CNAME".to_string(),
            name: d.domain.clone(),
            value: cf_domain.clone(),
            purpose: "traffic".to_string(),
        });
    }

    DomainResponse {
        domain_id: d.domain_id,
        env_id: d.env_id,
        domain: d.domain.clone(),
        status: d.status(),
        dns_records,
        routing_domain: d.routing_domain.clone(),
        created_at: d.created_at,
        verified_at: d.verified_at,
        tls_provisioned_at: d.tls_provisioned_at,
        provisioning_error: d.provisioning_error.clone(),
    }
}

/// Enqueue a maintenance task to the worker queue (best-effort, fire-and-forget).
fn enqueue_domain_task(conf: &Arc<hot::val::Val>, task: &str) {
    let conf = Arc::clone(conf);
    let task = task.to_string();
    tokio::spawn(async move {
        if let Err(e) = enqueue_maintenance_task_inner(&conf, &task).await {
            tracing::warn!("Failed to enqueue {}: {}", task, e);
        }
    });
}

async fn enqueue_maintenance_task_inner(conf: &hot::val::Val, task: &str) -> Result<(), String> {
    use hot::queue::Queue;
    use std::str::FromStr;

    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let queue_type =
        hot::queue::QueueType::from_str(&queue_type_str).unwrap_or(hot::queue::QueueType::Memory);

    let redis_uri_str = conf.get_str_or_default("redis.uri", "");
    let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
        None
    } else {
        Some(redis_uri_str)
    };
    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);
    let serialization = hot::data::serialization::Serialization::default();

    let queue = hot::queue::ProcessingQueue::<hot::data::msg::Message>::new_with_cluster(
        queue_type,
        "hot:event".to_string(),
        redis_uri,
        redis_cluster,
        serialization,
    )
    .map_err(|e| e.to_string())?;

    let msg = hot::lang::event::MaintenanceMessage::single_task(task);
    let message: hot::data::msg::Message = msg.into();
    Queue::enqueue(&queue, message)
        .await
        .map_err(|e| e.to_string())
}

/// Check if the org's resolved features allow custom domains.
/// Returns the resolved features on success (for limit checking).
async fn require_custom_domains_feature(
    db: &hot::db::DatabasePool,
    org_id: &Uuid,
) -> Result<Features, (StatusCode, Json<ApiErrorResponse>)> {
    let features = Features::resolve_for_org(db, org_id).await;
    if !features.has_custom_domains() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "plan_required",
                "Custom domains require a Pro or Scale plan. Upgrade at https://hot.dev/pricing"
                    .to_string(),
            )),
        ));
    }
    Ok(features)
}

/// Check that the org has not exceeded its custom domain limit.
async fn check_domain_limit(
    db: &hot::db::DatabasePool,
    org_id: &Uuid,
    features: &Features,
) -> Result<(), (StatusCode, Json<ApiErrorResponse>)> {
    let max = features.max_custom_domains();
    if max < 0 {
        return Ok(()); // unlimited
    }

    let count = Domain::count_by_org(db, org_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    if count >= max as i64 {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "domain_limit_reached",
                format!(
                    "Your plan allows up to {} custom domain{}. \
                     Delete an existing domain or upgrade your plan at https://hot.dev/pricing",
                    max,
                    if max == 1 { "" } else { "s" }
                ),
            )),
        ));
    }
    Ok(())
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /v1/domains — Register a new custom domain
///
/// 1. Creates the domain record in the DB.
/// 2. Requests a provider certificate if custom domain provisioning is enabled.
/// 3. Returns the validation CNAME record for the customer to add.
pub async fn create_domain(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Json(body): Json<CreateDomainRequest>,
) -> Result<(StatusCode, Json<ApiResponse<DomainResponse>>), (StatusCode, Json<ApiErrorResponse>)> {
    if !hot::domain::custom_domains_enabled(&conf) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::new(
                "custom_domains_disabled",
                "Custom domain provisioning is not enabled for this instance.".to_string(),
            )),
        ));
    }

    // Check custom domains feature requirement and limit
    let org_id = get_org_id_for_env(&db, &api_key.env_id).await?;
    let features = require_custom_domains_feature(&db, &org_id).await?;
    check_domain_limit(&db, &org_id, &features).await?;

    // Create the domain record
    let domain = Domain::create(&db, &api_key.env_id, &body.domain)
        .await
        .map_err(|e| match e {
            hot::db::domain::DomainError::AlreadyExists => (
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::new(
                    "domain_exists",
                    format!("Domain '{}' is already registered", body.domain),
                )),
            ),
            hot::db::domain::DomainError::PendingDeletion => (
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::new(
                    "domain_pending_deletion",
                    format!("Domain '{}' was recently removed and is still being cleaned up. Please wait a few minutes and try again.", body.domain),
                )),
            ),
            hot::db::domain::DomainError::InvalidDomain => (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request("Invalid domain name")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    // If custom domain provisioning is enabled, request a provider certificate.
    let domain = if hot::domain::custom_domains_enabled(&conf) {
        match hot::domain_provider::domain_provider()
            .request_certificate(&conf, &db, &domain)
            .await
        {
            Ok(()) => hot::db::domain::Domain::get_domain(&db, &domain.domain_id)
                .await
                .unwrap_or(domain),
            Err(e) => {
                // Log but don't fail the create — domain is still created,
                // provisioning can be retried by the background worker.
                tracing::warn!(
                    "Domain certificate request failed for domain '{}': {}. Will retry in background.",
                    domain.domain,
                    e
                );
                domain
            }
        }
    } else {
        domain
    };

    enqueue_domain_task(&conf, "domain_provisioning");

    let response = domain_to_response(&domain);

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse {
            data: response,
            meta: ResponseMeta {
                request_id: Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
            },
        }),
    ))
}

/// GET /v1/domains — List domains for this environment
pub async fn list_domains(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<Vec<DomainResponse>>>, (StatusCode, Json<ApiErrorResponse>)> {
    let domains = Domain::list_by_env(&db, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let data: Vec<DomainResponse> = domains.iter().map(domain_to_response).collect();

    Ok(Json(ApiResponse {
        data,
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

/// GET /v1/domains/{domain_id} — Get a specific domain
pub async fn get_domain(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Path(domain_id): Path<Uuid>,
) -> Result<Json<ApiResponse<DomainResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let domain = Domain::get_domain(&db, &domain_id)
        .await
        .map_err(|e| match e {
            hot::db::domain::DomainError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Domain")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if domain.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Domain")),
        ));
    }

    Ok(Json(ApiResponse {
        data: domain_to_response(&domain),
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

/// POST /v1/domains/{domain_id}/verify — Check provisioning status
///
/// Polls provider certificate status and returns current domain state.
pub async fn verify_domain(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    domain_cache: Option<Extension<crate::domain_resolver::DomainCache>>,
    Path(domain_id): Path<Uuid>,
) -> Result<Json<ApiResponse<DomainVerifyResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let domain = Domain::get_domain(&db, &domain_id)
        .await
        .map_err(|e| match e {
            hot::db::domain::DomainError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Domain")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if domain.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Domain")),
        ));
    }

    // Already fully active?
    if domain.is_ready() {
        return Ok(Json(ApiResponse {
            data: DomainVerifyResponse {
                domain_id: domain.domain_id,
                domain: domain.domain.clone(),
                status: DomainStatus::Active,
                message: "Domain is active and serving traffic.".to_string(),
            },
            meta: ResponseMeta {
                request_id: Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
            },
        }));
    }

    // If a provider certificate exists and is not yet verified, check cert status.
    if let Some(arn) = &domain.certificate_ref
        && !domain.is_verified()
    {
        // Check if provider cert has been issued.
        match check_certificate_and_update(&conf, &db, &domain, arn).await {
            Ok(true) => {
                // Invalidate cache
                if let Some(Extension(cache)) = domain_cache {
                    cache.remove(&domain.domain);
                }
                return Ok(Json(ApiResponse {
                        data: DomainVerifyResponse {
                            domain_id: domain.domain_id,
                            domain: domain.domain.clone(),
                            status: DomainStatus::Validated,
                            message: "Certificate validated! Domain routing will be provisioned automatically.".to_string(),
                        },
                        meta: ResponseMeta {
                            request_id: Uuid::new_v4(),
                            timestamp: chrono::Utc::now(),
                        },
                    }));
            }
            Ok(false) => {
                // Still pending
                let msg = if let (Some(name), Some(value)) = (
                    &domain.validation_cname_name,
                    &domain.validation_cname_value,
                ) {
                    format!(
                        "Waiting for DNS validation. Add a CNAME record: {} -> {}. \
                             DNS changes may take a few minutes to propagate.",
                        name, value
                    )
                } else {
                    "Waiting for DNS validation records to become available.".to_string()
                };

                return Ok(Json(ApiResponse {
                    data: DomainVerifyResponse {
                        domain_id: domain.domain_id,
                        domain: domain.domain.clone(),
                        status: DomainStatus::PendingValidation,
                        message: msg,
                    },
                    meta: ResponseMeta {
                        request_id: Uuid::new_v4(),
                        timestamp: chrono::Utc::now(),
                    },
                }));
            }
            Err(e) => {
                tracing::warn!(
                    "Certificate status check failed for {}: {}",
                    domain.domain,
                    e
                );
            }
        }
    }

    // Clear any previous provisioning error so the worker retries cleanly
    if domain.provisioning_error.is_some() {
        let _ = Domain::clear_provisioning_error(&db, &domain_id).await;
    }

    // Enqueue immediate verification + provisioning for the worker
    enqueue_domain_task(&conf, "domain_verification");
    enqueue_domain_task(&conf, "domain_provisioning");

    // Reload and return current status
    let domain = Domain::get_domain(&db, &domain_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok(Json(ApiResponse {
        data: DomainVerifyResponse {
            domain_id: domain.domain_id,
            domain: domain.domain.clone(),
            status: domain.status(),
            message: match domain.status() {
                DomainStatus::Active => "Domain is active.".to_string(),
                DomainStatus::Provisioning => "Domain routing is deploying.".to_string(),
                DomainStatus::Validated => {
                    "Validated. Domain routing will be provisioned automatically.".to_string()
                }
                DomainStatus::PendingValidation => "Waiting for DNS validation.".to_string(),
                DomainStatus::PendingDeletion => "Domain is being deleted.".to_string(),
            },
        },
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

/// Check provider certificate status and mark verified if issued. Returns true if issued.
async fn check_certificate_and_update(
    conf: &hot::val::Val,
    db: &hot::db::DatabasePool,
    domain: &Domain,
    certificate_arn: &str,
) -> Result<bool, String> {
    let status = hot::domain_provider::domain_provider()
        .certificate_status(conf, domain, certificate_arn)
        .await
        .map_err(|e| e.to_string())?;

    match status {
        hot::domain_provider::DomainCertificateStatus::Issued => {
            Domain::mark_verified(db, &domain.domain_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(true)
        }
        hot::domain_provider::DomainCertificateStatus::Failed(reason) => {
            Err(format!("certificate failed: {}", reason))
        }
        _ => Ok(false),
    }
}

/// DELETE /v1/domains/{domain_id} — Remove a custom domain
///
/// Soft-deletes the domain record. The worker handles provider resource cleanup
/// and then performs the final hard delete.
pub async fn delete_domain(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    domain_cache: Option<Extension<crate::domain_resolver::DomainCache>>,
    Path(domain_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    let domain = Domain::get_domain(&db, &domain_id)
        .await
        .map_err(|e| match e {
            hot::db::domain::DomainError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Domain")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if domain.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Domain")),
        ));
    }

    // Soft-delete stops routing immediately; worker will clean up provider resources.
    Domain::soft_delete(&db, &domain_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    // Invalidate the domain resolution cache
    if let Some(Extension(cache)) = domain_cache {
        cache.remove(&domain.domain);
    }

    enqueue_domain_task(&conf, "domain_cleanup");

    Ok(StatusCode::NO_CONTENT)
}
