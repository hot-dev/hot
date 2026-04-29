//! Custom Domains dashboard handlers
//!
//! Manages custom domains — mapping customer domains to Hot Dev environments.
//! This feature is gated to Pro+ plans via the `custom_domains` feature flag.

use crate::auth::Session;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect};
use hot::db::DatabasePool;
use hot::db::domain::Domain;
use std::sync::Arc;
use uuid::Uuid;

#[allow(unused_imports)]
use hot::db::domain::DomainError;

fn domain_new_breadcrumbs(session: &Session) -> crate::templates::Breadcrumbs {
    let mut bc = crate::templates::build_base_breadcrumbs_with_env(session);
    bc.push(crate::templates::BreadcrumbItem::clickable(
        "Domains".to_string(),
        "/domains".to_string(),
    ));
    bc.push(crate::templates::BreadcrumbItem::current(
        "Add Domain".to_string(),
    ));
    bc
}

fn domain_detail_breadcrumbs(
    session: &Session,
    domain_name: &str,
) -> crate::templates::Breadcrumbs {
    let mut bc = crate::templates::build_base_breadcrumbs_with_env(session);
    bc.push(crate::templates::BreadcrumbItem::clickable(
        "Domains".to_string(),
        "/domains".to_string(),
    ));
    bc.push(crate::templates::BreadcrumbItem::current(
        domain_name.to_string(),
    ));
    bc
}

/// Enqueue a maintenance task (e.g. domain_provisioning, domain_cleanup) to the worker.
/// Best-effort: logs a warning on failure rather than propagating the error.
fn enqueue_domain_task(conf: &hot::val::Val, task: &str) {
    let conf = conf.clone();
    let task = task.to_string();
    tokio::spawn(async move {
        if let Err(e) = enqueue_maintenance_task(&conf, &task).await {
            tracing::warn!("Failed to enqueue {}: {}", task, e);
        }
    });
}

async fn enqueue_maintenance_task(conf: &hot::val::Val, task: &str) -> Result<(), String> {
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

/// GET /domains — list all custom domains for the current environment
pub async fn domains_list_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let domains = Domain::list_by_env(&db, &env.env_id)
        .await
        .unwrap_or_default();

    let mut breadcrumbs = crate::templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(crate::templates::BreadcrumbItem::current(
        "Domains".to_string(),
    ));

    let template = crate::templates::DomainsList {
        title: "Domains",
        page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
            "domains",
            &session,
            breadcrumbs,
        ),
        domains,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// GET /domains/new — show add domain form
pub async fn domains_new_handler(
    State(_db): State<Arc<DatabasePool>>,
    State(conf): State<hot::val::Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    let error_message = if hot::domain::custom_domains_enabled(&conf) {
        ""
    } else {
        "Custom domain provisioning is not enabled for this instance."
    };

    let template = crate::templates::DomainsNew {
        title: "Add Custom Domain",
        page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
            "domains",
            &session,
            domain_new_breadcrumbs(&session),
        ),
        error_message,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// POST /domains/new — create a new custom domain
pub async fn domains_create_handler(
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<hot::val::Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    axum::extract::Form(form): axum::extract::Form<CreateDomainForm>,
) -> Result<Redirect, Result<Html<String>, (StatusCode, String)>> {
    let env = session.current_env.as_ref().ok_or(Err((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    )))?;

    if !hot::domain::custom_domains_enabled(&conf) {
        let template = crate::templates::DomainsNew {
            title: "Add Custom Domain",
            page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
                "domains",
                &session,
                domain_new_breadcrumbs(&session),
            ),
            error_message: "Custom domain provisioning is not enabled for this instance.",
        };
        return Err(Ok(Html(
            template
                .render()
                .unwrap_or_else(|_| "Template error".into()),
        )));
    }

    // Check feature gate
    if !session.current_org_features.has_custom_domains() {
        let template = crate::templates::DomainsNew {
            title: "Add Custom Domain",
            page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
                "domains",
                &session,
                domain_new_breadcrumbs(&session),
            ),
            error_message: "Custom domains require a Pro or Scale plan.",
        };
        return Err(Ok(Html(
            template
                .render()
                .unwrap_or_else(|_| "Template error".into()),
        )));
    }

    // Check domain count limit
    let max = session.current_org_features.max_custom_domains();
    if max >= 0 {
        let org_id = &session
            .current_org
            .as_ref()
            .ok_or(Err((
                StatusCode::BAD_REQUEST,
                "No organization selected".to_string(),
            )))?
            .org_id;
        let count = Domain::count_by_org(&db, org_id).await.unwrap_or(0);
        if count >= max as i64 {
            let msg = format!(
                "Your plan allows up to {} custom domain{}. Delete an existing domain or upgrade your plan.",
                max,
                if max == 1 { "" } else { "s" }
            );
            let template = crate::templates::DomainsNew {
                title: "Add Custom Domain",
                page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
                    "domains",
                    &session,
                    domain_new_breadcrumbs(&session),
                ),
                error_message: &msg,
            };
            return Err(Ok(Html(
                template
                    .render()
                    .unwrap_or_else(|_| "Template error".into()),
            )));
        }
    }

    match Domain::create(&db, &env.env_id, &form.domain).await {
        Ok(domain) => {
            // Request provider certificate inline for immediate UX.
            // If this fails, the background worker will retry.
            if hot::domain::custom_domains_enabled(&conf)
                && let Err(e) = hot::domain_provider::domain_provider()
                    .request_certificate(&conf, &db, &domain)
                    .await
            {
                tracing::warn!(
                    "Domain certificate request failed for domain '{}': {}. Will retry in background.",
                    domain.domain,
                    e
                );
            }
            enqueue_domain_task(&conf, "domain_provisioning");
            Ok(Redirect::to(&format!("/domains/{}", domain.domain_id)))
        }
        Err(e) => {
            let msg = match e {
                hot::db::domain::DomainError::InvalidDomain => {
                    "Invalid domain name. Use format: subdomain.example.com".to_string()
                }
                hot::db::domain::DomainError::AlreadyExists => {
                    "This domain is already registered.".to_string()
                }
                hot::db::domain::DomainError::PendingDeletion => {
                    "This domain was recently removed and is still being cleaned up. Please wait a few minutes and try again.".to_string()
                }
                _ => format!("Failed to add domain: {}", e),
            };
            let template = crate::templates::DomainsNew {
                title: "Add Custom Domain",
                page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
                    "domains",
                    &session,
                    domain_new_breadcrumbs(&session),
                ),
                error_message: &msg,
            };
            Err(Ok(Html(
                template
                    .render()
                    .unwrap_or_else(|_| "Template error".into()),
            )))
        }
    }
}

/// Query parameters for domain detail page (verification feedback).
#[derive(serde::Deserialize, Default)]
pub struct DomainDetailQuery {
    pub verified: Option<String>,
    pub dns_check: Option<String>,
}

/// GET /domains/{domain_id} — view domain detail
pub async fn domains_detail_handler(
    Path(domain_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<DomainDetailQuery>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let domain = Domain::get_domain(&db, &domain_id)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Domain not found".to_string()))?;

    if domain.env_id != env.env_id {
        return Err((StatusCode::NOT_FOUND, "Domain not found".to_string()));
    }

    // Build flash message from redirect query parameters
    let (flash_message, flash_type) = if let Some(ref dns) = query.dns_check {
        match dns.as_str() {
            "ok" => ("Domain CNAME is configured correctly!", "success"),
            "wrong" => (
                "Domain CNAME is not pointing to the expected routing target. Update your DNS CNAME record to match the value shown in Step 3 below.",
                "warning",
            ),
            "missing" => (
                "No CNAME record found for this domain yet. Add a CNAME record pointing to the distribution shown in Step 3 below, then allow a few minutes for DNS propagation.",
                "warning",
            ),
            _ => ("", ""),
        }
    } else {
        match query.verified.as_deref() {
            Some("true") => ("Domain verified successfully!", "success"),
            Some("false") => (
                "DNS record not found yet. It may take a few minutes for DNS changes to propagate. Verification is also checked automatically in the background.",
                "warning",
            ),
            Some("already") => ("Domain is already verified.", "success"),
            _ => ("", ""),
        }
    };

    let template = crate::templates::DomainDetail {
        title: "Custom Domain",
        page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
            "domains",
            &session,
            domain_detail_breadcrumbs(&session, &domain.domain),
        ),
        domain,
        flash_message,
        flash_type,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// POST /domains/{domain_id}/verify — trigger domain verification
pub async fn domains_verify_handler(
    Path(domain_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<hot::val::Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, (StatusCode, String)> {
    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let domain = Domain::get_domain(&db, &domain_id)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Domain not found".to_string()))?;

    if domain.env_id != env.env_id {
        return Err((StatusCode::NOT_FOUND, "Domain not found".to_string()));
    }

    // Clear any previous provisioning error so the worker retries cleanly
    if domain.provisioning_error.is_some() {
        let _ = Domain::clear_provisioning_error(&db, &domain_id).await;
    }

    // Enqueue immediate verification + provisioning checks
    enqueue_domain_task(&conf, "domain_verification");
    enqueue_domain_task(&conf, "domain_provisioning");

    // If the domain has a routing target, check if the CNAME is configured.
    if domain.routing_domain.is_some() {
        use hot::db::domain::CnameCheckResult;
        let result = domain
            .check_domain_cname()
            .await
            .unwrap_or(CnameCheckResult::Missing);
        let param = match result {
            CnameCheckResult::Ok => "ok",
            CnameCheckResult::Missing => "missing",
            CnameCheckResult::Wrong(_) => "wrong",
            CnameCheckResult::NoDist => "missing",
        };
        return Ok(Redirect::to(&format!(
            "/domains/{}?dns_check={}",
            domain_id, param
        )));
    }

    Ok(Redirect::to(&format!("/domains/{}", domain_id)))
}

/// POST /domains/{domain_id}/delete — soft-delete a domain
pub async fn domains_delete_handler(
    Path(domain_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<hot::val::Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, (StatusCode, String)> {
    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let domain = Domain::get_domain(&db, &domain_id)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Domain not found".to_string()))?;

    if domain.env_id != env.env_id {
        return Err((StatusCode::NOT_FOUND, "Domain not found".to_string()));
    }

    Domain::soft_delete(&db, &domain_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete domain: {}", e),
        )
    })?;

    enqueue_domain_task(&conf, "domain_cleanup");

    Ok(Redirect::to("/domains"))
}

#[derive(serde::Deserialize)]
pub struct CreateDomainForm {
    pub domain: String,
}
