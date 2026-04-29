use crate::auth::Session;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse};
use hot::db::{DatabasePool, Webhook};
use std::sync::Arc;

/// Handler for listing webhook services as cards
pub async fn webhook_services_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Webhooks".to_string(),
    ));

    let search_query = params
        .get("search")
        .map(|s| s.to_string())
        .unwrap_or_default();

    let env_id = session.current_env_id();

    let service_cards = if let Some(env_id) = env_id {
        let summaries = Webhook::get_service_summaries_by_env(&db, &env_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to get webhook service summaries for env {}: {}",
                    env_id,
                    e
                );
                Vec::new()
            });

        let cards: Vec<templates::WebhookServiceCard> = summaries
            .into_iter()
            .filter(|s| {
                if search_query.is_empty() {
                    true
                } else {
                    let q = search_query.to_lowercase();
                    s.service.to_lowercase().contains(&q)
                        || s.projects.iter().any(|p| p.to_lowercase().contains(&q))
                }
            })
            .map(|s| templates::WebhookServiceCard {
                service: s.service,
                endpoint_count: s.endpoint_count,
                projects: s.projects,
            })
            .collect();

        cards
    } else {
        Vec::new()
    };

    let template = templates::WebhookServicesList {
        title: "Webhooks",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "webhook_services",
            &session,
            breadcrumbs,
        ),
        service_cards,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}

/// Handler for showing endpoints within a specific webhook service
pub async fn webhook_service_detail_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(service): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Webhooks".to_string(),
        "/webhooks".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(service.clone()));

    let search_query = params
        .get("search")
        .map(|s| s.to_string())
        .unwrap_or_default();

    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    const ITEMS_PER_PAGE: i64 = 20;
    let offset = (current_page_num - 1) * ITEMS_PER_PAGE;

    // Build base URL from session org/env
    let org_slug = session
        .current_org
        .as_ref()
        .map(|o| o.slug.as_str())
        .unwrap_or("local");
    let env_name = session
        .current_env
        .as_ref()
        .map(|e| e.name.as_str())
        .unwrap_or("development");
    let api_url = hot::env::get_api_url();
    let base_url = format!(
        "{}/webhook/{}/{}/{}",
        api_url, org_slug, env_name, service
    );

    let env_id = session.current_env_id();

    let (endpoints, total_endpoints) = if let Some(env_id) = env_id {
        // Fetch all endpoints for this environment
        let all_endpoints = Webhook::get_by_env(&db, &env_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to get webhook endpoints for env {}: {}",
                    env_id,
                    e
                );
                Vec::new()
            });

        // Filter to this service
        let service_endpoints: Vec<_> = all_endpoints
            .into_iter()
            .filter(|ep| ep.service == service)
            .collect();

        // Apply search filter
        let filtered: Vec<_> = if search_query.is_empty() {
            service_endpoints
        } else {
            let search_lower = search_query.to_lowercase();
            service_endpoints
                .into_iter()
                .filter(|ep| {
                    ep.project_name.to_lowercase().contains(&search_lower)
                        || ep.path.to_lowercase().contains(&search_lower)
                        || ep.method.to_lowercase().contains(&search_lower)
                        || ep.name.to_lowercase().contains(&search_lower)
                        || ep.ns.to_lowercase().contains(&search_lower)
                        || ep.var.to_lowercase().contains(&search_lower)
                        || ep.description
                            .as_deref()
                            .map(|d| d.to_lowercase().contains(&search_lower))
                            .unwrap_or(false)
                })
                .collect()
        };

        let total = filtered.len() as i64;

        let page_endpoints: Vec<templates::WebhookDisplay> = filtered
            .into_iter()
            .skip(offset as usize)
            .take(ITEMS_PER_PAGE as usize)
            .map(templates::WebhookDisplay::from)
            .collect();

        (page_endpoints, total)
    } else {
        (Vec::new(), 0)
    };

    let total_pages = if total_endpoints > 0 {
        (total_endpoints + ITEMS_PER_PAGE - 1) / ITEMS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::WebhookServiceDetail {
        title: &format!("Webhook Service: {}", service),
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "webhook_services",
            &session,
            breadcrumbs,
        ),
        service,
        base_url,
        endpoints,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_endpoints,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}
