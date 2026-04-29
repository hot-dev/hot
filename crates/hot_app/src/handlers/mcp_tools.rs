use crate::auth::Session;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse};
use hot::db::domain::Domain;
use hot::db::{DatabasePool, McpTool};
use std::sync::Arc;

/// Handler for listing MCP services as cards
pub async fn mcp_services_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "MCP Services".to_string(),
    ));

    let search_query = params
        .get("search")
        .map(|s| s.to_string())
        .unwrap_or_default();

    let env_id = session.current_env_id();

    let service_cards = if let Some(env_id) = env_id {
        let summaries = McpTool::get_service_summaries_by_env(&db, &env_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to get MCP service summaries for env {}: {}",
                    env_id,
                    e
                );
                Vec::new()
            });

        let cards: Vec<templates::McpServiceCard> = summaries
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
            .map(|s| templates::McpServiceCard {
                service: s.service,
                tool_count: s.tool_count,
                projects: s.projects,
            })
            .collect();

        cards
    } else {
        Vec::new()
    };

    let template = templates::McpServicesList {
        title: "MCP Services",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "mcp_services",
            &session,
            breadcrumbs,
        ),
        service_cards,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}

/// Handler for showing tools within a specific MCP service
pub async fn mcp_service_detail_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(service): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "MCP Services".to_string(),
        "/mcp".to_string(),
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

    // Build endpoint URL from session org/env
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
    let endpoint_url = format!("{}/mcp/{}/{}/{}", api_url, org_slug, env_name, service);

    let env_id = session.current_env_id();

    let custom_domains: Vec<String> = if let Some(env_id) = env_id {
        Domain::list_by_env(&db, &env_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.is_ready())
            .map(|d| d.domain)
            .collect()
    } else {
        Vec::new()
    };

    let (tools, total_tools) = if let Some(env_id) = env_id {
        // Fetch all tools for this service in the environment
        let all_tools = McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get MCP tools for env {}: {}", env_id, e);
                Vec::new()
            });

        // Filter to this service
        let service_tools: Vec<_> = all_tools
            .into_iter()
            .filter(|t| t.service == service)
            .collect();

        // Apply search filter
        let filtered: Vec<_> = if search_query.is_empty() {
            service_tools
        } else {
            let search_lower = search_query.to_lowercase();
            service_tools
                .into_iter()
                .filter(|t| {
                    t.project_name.to_lowercase().contains(&search_lower)
                        || t.name.to_lowercase().contains(&search_lower)
                        || t.ns.to_lowercase().contains(&search_lower)
                        || t.var.to_lowercase().contains(&search_lower)
                        || t.description
                            .as_deref()
                            .map(|d| d.to_lowercase().contains(&search_lower))
                            .unwrap_or(false)
                })
                .collect()
        };

        let total = filtered.len() as i64;

        let page_tools: Vec<templates::McpToolDisplay> = filtered
            .into_iter()
            .skip(offset as usize)
            .take(ITEMS_PER_PAGE as usize)
            .map(templates::McpToolDisplay::from)
            .collect();

        (page_tools, total)
    } else {
        (Vec::new(), 0)
    };

    let total_pages = if total_tools > 0 {
        (total_tools + ITEMS_PER_PAGE - 1) / ITEMS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::McpServiceDetail {
        title: &format!("MCP Service: {}", service),
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "mcp_services",
            &session,
            breadcrumbs,
        ),
        service,
        endpoint_url,
        custom_domains,
        tools,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_tools,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}
