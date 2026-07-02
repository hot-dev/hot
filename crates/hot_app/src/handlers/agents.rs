use crate::auth::Session;
use crate::templates;
use ahash::{AHashMap, AHashSet};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse};
use hot::db::{
    Agent, AgentWithProject, DatabasePool, EventHandler, EventHandlerWithProject, McpTool,
    McpToolWithProject, Schedule, ScheduleWithProject, Webhook, WebhookWithProject, Workflow,
    WorkflowWithProject,
};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use uuid::Uuid;

fn extract_tags(tags: &Option<JsonValue>) -> Vec<String> {
    tags.as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_config_fields(config_fields: &Option<JsonValue>) -> Vec<(String, String)> {
    config_fields
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?;
                    let ty = f.get("type").and_then(|t| t.as_str()).unwrap_or("Any");
                    Some((name.to_string(), ty.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn agent_qualified_name(agent: &AgentWithProject) -> String {
    format!("{}/{}", agent.namespace, agent.type_name)
}

/// Convert internal qualified name to a URL-friendly path.
/// `::demo::agents/LeadQualifier` -> `demo/agents/LeadQualifier`
pub(crate) fn qualified_name_to_url_path(qn: &str) -> String {
    let (ns, type_name) = qn.rsplit_once('/').unwrap_or(("", qn));
    let ns_path = ns.trim_start_matches("::").replace("::", "/");
    if ns_path.is_empty() {
        type_name.to_string()
    } else {
        format!("{}/{}", ns_path, type_name)
    }
}

/// Convert a URL path back to the internal qualified name.
/// `demo/agents/LeadQualifier` -> `::demo::agents/LeadQualifier`
pub(crate) fn url_path_to_qualified_name(path: &str) -> String {
    let path = path.trim_start_matches('/');
    match path.rsplit_once('/') {
        Some((ns_segments, type_name)) => {
            let ns = format!("::{}", ns_segments.replace('/', "::"));
            format!("{}/{}", ns, type_name)
        }
        None => path.to_string(),
    }
}

fn agent_display_name(agent: &AgentWithProject) -> String {
    agent
        .name
        .clone()
        .unwrap_or_else(|| agent.type_name.clone())
}

fn workflow_qualified_name(workflow: &WorkflowWithProject) -> String {
    format!("{}/{}", workflow.namespace, workflow.type_name)
}

fn workflow_display_name(workflow: &WorkflowWithProject) -> String {
    workflow
        .name
        .clone()
        .unwrap_or_else(|| workflow.type_name.clone())
}

fn short_type_name(name: &str) -> String {
    name.rsplit_once('/')
        .map(|(_, ty)| ty.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn meta_agent_type_name(meta: &Option<JsonValue>) -> Option<String> {
    meta.as_ref()
        .and_then(|m| m.get("agent"))
        .and_then(|a| a.as_str())
        .map(short_type_name)
}

fn meta_workflow_type_names(meta: &Option<JsonValue>) -> Vec<String> {
    let Some(meta) = meta.as_ref() else {
        return Vec::new();
    };

    let mut names = Vec::new();
    if let Some(w) = meta.get("workflow").and_then(|v| v.as_str()) {
        names.push(short_type_name(w));
    }
    if let Some(arr) = meta.get("workflows").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(w) = item.as_str() {
                names.push(short_type_name(w));
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn meta_matches_workflow(meta: &Option<JsonValue>, type_name: &str) -> bool {
    meta_workflow_type_names(meta)
        .iter()
        .any(|name| name == type_name)
}

fn is_unnamed_workflow_meta(
    meta: &Option<JsonValue>,
    known_agent_types: &AHashSet<String>,
) -> bool {
    if let Some(agent_type) = meta_agent_type_name(meta)
        && known_agent_types.contains(&agent_type)
    {
        return false;
    }
    meta_workflow_type_names(meta).is_empty()
}

fn count_handlers_for_agent(
    handlers: &[EventHandlerWithProject],
    schedules: &[ScheduleWithProject],
    webhooks: &[WebhookWithProject],
    mcp_tools: &[McpToolWithProject],
    type_name: &str,
) -> i64 {
    let mut count = 0i64;
    for h in handlers {
        if meta_matches_agent(&h.meta, type_name) {
            count += 1;
        }
    }
    for s in schedules {
        if meta_matches_agent(&s.meta, type_name) {
            count += 1;
        }
    }
    for w in webhooks {
        if meta_matches_agent(&w.meta, type_name) {
            count += 1;
        }
    }
    for t in mcp_tools {
        if meta_matches_agent(&t.meta, type_name) {
            count += 1;
        }
    }
    count
}

fn count_handlers_for_workflow(
    handlers: &[EventHandlerWithProject],
    schedules: &[ScheduleWithProject],
    webhooks: &[WebhookWithProject],
    mcp_tools: &[McpToolWithProject],
    type_name: &str,
) -> i64 {
    let mut count = 0i64;
    for h in handlers {
        if meta_matches_workflow(&h.meta, type_name) {
            count += 1;
        }
    }
    for s in schedules {
        if meta_matches_workflow(&s.meta, type_name) {
            count += 1;
        }
    }
    for w in webhooks {
        if meta_matches_workflow(&w.meta, type_name) {
            count += 1;
        }
    }
    for t in mcp_tools {
        if meta_matches_workflow(&t.meta, type_name) {
            count += 1;
        }
    }
    count
}

fn count_unnamed_handlers_for_build(
    handlers: &[EventHandlerWithProject],
    schedules: &[ScheduleWithProject],
    webhooks: &[WebhookWithProject],
    mcp_tools: &[McpToolWithProject],
    known_agent_types: &AHashSet<String>,
    build_id: Uuid,
) -> i64 {
    let mut count = 0i64;
    for h in handlers {
        if h.build_id == build_id && is_unnamed_workflow_meta(&h.meta, known_agent_types) {
            count += 1;
        }
    }
    for s in schedules {
        if s.build_id == build_id && is_unnamed_workflow_meta(&s.meta, known_agent_types) {
            count += 1;
        }
    }
    for w in webhooks {
        if w.build_id == build_id && is_unnamed_workflow_meta(&w.meta, known_agent_types) {
            count += 1;
        }
    }
    for t in mcp_tools {
        if t.build_id == build_id && is_unnamed_workflow_meta(&t.meta, known_agent_types) {
            count += 1;
        }
    }
    count
}

fn meta_matches_agent(meta: &Option<JsonValue>, type_name: &str) -> bool {
    meta.as_ref()
        .and_then(|m| m.get("agent"))
        .and_then(|a| a.as_str())
        .is_some_and(|s| {
            s == type_name
                || s.rsplit_once('/')
                    .is_some_and(|(_, name)| name == type_name)
        })
}

fn build_handler_displays(
    handlers: &[EventHandlerWithProject],
    schedules: &[ScheduleWithProject],
    webhooks: &[WebhookWithProject],
    mcp_tools: &[McpToolWithProject],
    type_name: &str,
) -> Vec<templates::AgentHandlerDisplay> {
    let mut displays = Vec::new();
    for h in handlers {
        if meta_matches_agent(&h.meta, type_name) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Event".to_string(),
                trigger: h.event_type.clone(),
                function: format!("{}/{}", h.ns, h.var),
                retry: format_retry(&h.meta),
                source: format_source(&h.file, h.line),
                source_build_id: h.build_id.to_string(),
                source_file: h.file.clone(),
                source_line: h.line,
            });
        }
    }
    for s in schedules {
        if meta_matches_agent(&s.meta, type_name) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Schedule".to_string(),
                trigger: s.cron.clone(),
                function: format!("{}/{}", s.ns, s.var),
                retry: format_retry(&s.meta),
                source: format_source(&s.file, s.line),
                source_build_id: s.build_id.to_string(),
                source_file: s.file.clone(),
                source_line: s.line,
            });
        }
    }
    for w in webhooks {
        if meta_matches_agent(&w.meta, type_name) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Webhook".to_string(),
                trigger: format!("{} {}/{}", w.method, w.service, w.path),
                function: format!("{}/{}", w.ns, w.var),
                retry: "-".to_string(),
                source: format_source(&w.file, w.line),
                source_build_id: w.build_id.to_string(),
                source_file: w.file.clone(),
                source_line: w.line,
            });
        }
    }
    for t in mcp_tools {
        if meta_matches_agent(&t.meta, type_name) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "MCP Tool".to_string(),
                trigger: format!("{}/{}", t.service, t.name),
                function: format!("{}/{}", t.ns, t.var),
                retry: "-".to_string(),
                source: format_source(&t.file, t.line),
                source_build_id: t.build_id.to_string(),
                source_file: t.file.clone(),
                source_line: t.line,
            });
        }
    }
    displays
}

fn build_workflow_handler_displays(
    handlers: &[EventHandlerWithProject],
    schedules: &[ScheduleWithProject],
    webhooks: &[WebhookWithProject],
    mcp_tools: &[McpToolWithProject],
    workflow_type: &str,
) -> Vec<templates::AgentHandlerDisplay> {
    let mut displays = Vec::new();
    for h in handlers {
        if meta_matches_workflow(&h.meta, workflow_type) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Event".to_string(),
                trigger: h.event_type.clone(),
                function: format!("{}/{}", h.ns, h.var),
                retry: format_retry(&h.meta),
                source: format_source(&h.file, h.line),
                source_build_id: h.build_id.to_string(),
                source_file: h.file.clone(),
                source_line: h.line,
            });
        }
    }
    for s in schedules {
        if meta_matches_workflow(&s.meta, workflow_type) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Schedule".to_string(),
                trigger: s.cron.clone(),
                function: format!("{}/{}", s.ns, s.var),
                retry: format_retry(&s.meta),
                source: format_source(&s.file, s.line),
                source_build_id: s.build_id.to_string(),
                source_file: s.file.clone(),
                source_line: s.line,
            });
        }
    }
    for w in webhooks {
        if meta_matches_workflow(&w.meta, workflow_type) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Webhook".to_string(),
                trigger: format!("{} {}/{}", w.method, w.service, w.path),
                function: format!("{}/{}", w.ns, w.var),
                retry: "-".to_string(),
                source: format_source(&w.file, w.line),
                source_build_id: w.build_id.to_string(),
                source_file: w.file.clone(),
                source_line: w.line,
            });
        }
    }
    for t in mcp_tools {
        if meta_matches_workflow(&t.meta, workflow_type) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "MCP Tool".to_string(),
                trigger: format!("{}/{}", t.service, t.name),
                function: format!("{}/{}", t.ns, t.var),
                retry: "-".to_string(),
                source: format_source(&t.file, t.line),
                source_build_id: t.build_id.to_string(),
                source_file: t.file.clone(),
                source_line: t.line,
            });
        }
    }
    displays
}

fn build_unnamed_handler_displays(
    handlers: &[EventHandlerWithProject],
    schedules: &[ScheduleWithProject],
    webhooks: &[WebhookWithProject],
    mcp_tools: &[McpToolWithProject],
    known_agent_types: &AHashSet<String>,
    build_id: Uuid,
) -> Vec<templates::AgentHandlerDisplay> {
    let mut displays = Vec::new();
    for h in handlers {
        if h.build_id == build_id && is_unnamed_workflow_meta(&h.meta, known_agent_types) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Event".to_string(),
                trigger: h.event_type.clone(),
                function: format!("{}/{}", h.ns, h.var),
                retry: format_retry(&h.meta),
                source: format_source(&h.file, h.line),
                source_build_id: h.build_id.to_string(),
                source_file: h.file.clone(),
                source_line: h.line,
            });
        }
    }
    for s in schedules {
        if s.build_id == build_id && is_unnamed_workflow_meta(&s.meta, known_agent_types) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Schedule".to_string(),
                trigger: s.cron.clone(),
                function: format!("{}/{}", s.ns, s.var),
                retry: format_retry(&s.meta),
                source: format_source(&s.file, s.line),
                source_build_id: s.build_id.to_string(),
                source_file: s.file.clone(),
                source_line: s.line,
            });
        }
    }
    for w in webhooks {
        if w.build_id == build_id && is_unnamed_workflow_meta(&w.meta, known_agent_types) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "Webhook".to_string(),
                trigger: format!("{} {}/{}", w.method, w.service, w.path),
                function: format!("{}/{}", w.ns, w.var),
                retry: "-".to_string(),
                source: format_source(&w.file, w.line),
                source_build_id: w.build_id.to_string(),
                source_file: w.file.clone(),
                source_line: w.line,
            });
        }
    }
    for t in mcp_tools {
        if t.build_id == build_id && is_unnamed_workflow_meta(&t.meta, known_agent_types) {
            displays.push(templates::AgentHandlerDisplay {
                handler_type: "MCP Tool".to_string(),
                trigger: format!("{}/{}", t.service, t.name),
                function: format!("{}/{}", t.ns, t.var),
                retry: "-".to_string(),
                source: format_source(&t.file, t.line),
                source_build_id: t.build_id.to_string(),
                source_file: t.file.clone(),
                source_line: t.line,
            });
        }
    }
    displays
}

fn format_retry(meta: &Option<JsonValue>) -> String {
    meta.as_ref()
        .and_then(|m| m.get("retry"))
        .map(|r| {
            if let Some(n) = r.as_i64() {
                format!("{n}")
            } else {
                r.to_string()
            }
        })
        .unwrap_or_else(|| "-".to_string())
}

fn format_source(file: &Option<String>, line: Option<i32>) -> String {
    match file {
        Some(f) => match line {
            Some(l) => format!("{f}:{l}"),
            None => f.clone(),
        },
        None => "-".to_string(),
    }
}

pub async fn agents_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Workflows".to_string()));

    let search_query = params
        .get("search")
        .map(|s| s.to_string())
        .unwrap_or_default();

    let active_tab = params
        .get("tab")
        .map(|s| {
            if s == "graph" {
                "graph".to_string()
            } else {
                "all".to_string()
            }
        })
        .unwrap_or_else(|| "all".to_string());

    let env_id = session.current_env_id();

    let workflow_cards = if let Some(env_id) = env_id {
        let agents = Agent::get_agents_by_env_deployed(&db, &env_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get agents: {e}");
                Vec::new()
            });

        let workflows = Workflow::get_workflows_by_env_deployed(&db, &env_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get workflows: {e}");
                Vec::new()
            });

        let handlers =
            EventHandler::get_event_handlers_by_env_deployed(&db, &env_id, Some(1000), Some(0))
                .await
                .unwrap_or_default();

        let schedules = Schedule::get_schedules_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default();

        let webhooks = Webhook::get_by_env(&db, &env_id).await.unwrap_or_default();

        let mcp_tools = McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default();

        let known_agent_types: AHashSet<String> =
            agents.iter().map(|a| a.type_name.clone()).collect();
        let mut cards = Vec::new();

        for workflow in &workflows {
            let qualified_name = workflow_qualified_name(workflow);
            let handler_count = count_handlers_for_workflow(
                &handlers,
                &schedules,
                &webhooks,
                &mcp_tools,
                &workflow.type_name,
            );
            cards.push(templates::WorkflowListCard {
                url: format!(
                    "/workflows/named/{}",
                    qualified_name_to_url_path(&qualified_name)
                ),
                build_id: workflow.build_id.to_string(),
                kind: "workflow".to_string(),
                kind_label: "Workflow".to_string(),
                qualified_name,
                display_name: workflow_display_name(workflow),
                description: workflow.description.clone().unwrap_or_default(),
                tags: extract_tags(&workflow.tags),
                handler_count,
                project_name: workflow.project_name.clone(),
                source_file: workflow.file.clone(),
                source_line: workflow.line,
            });
        }

        for agent in &agents {
            let qualified_name = agent_qualified_name(agent);
            let handler_count = count_handlers_for_agent(
                &handlers,
                &schedules,
                &webhooks,
                &mcp_tools,
                &agent.type_name,
            );
            cards.push(templates::WorkflowListCard {
                url: format!(
                    "/workflows/agents/{}",
                    qualified_name_to_url_path(&qualified_name)
                ),
                build_id: agent.build_id.to_string(),
                kind: "agent".to_string(),
                kind_label: "Agent".to_string(),
                qualified_name,
                display_name: agent_display_name(agent),
                description: agent.description.clone().unwrap_or_default(),
                tags: extract_tags(&agent.tags),
                handler_count,
                project_name: agent.project_name.clone(),
                source_file: agent.file.clone(),
                source_line: agent.line,
            });
        }

        let mut unnamed_build_projects: AHashMap<Uuid, String> = AHashMap::new();
        for h in &handlers {
            if is_unnamed_workflow_meta(&h.meta, &known_agent_types) {
                unnamed_build_projects.insert(h.build_id, h.project_name.clone());
            }
        }
        for s in &schedules {
            if is_unnamed_workflow_meta(&s.meta, &known_agent_types) {
                unnamed_build_projects.insert(s.build_id, s.project_name.clone());
            }
        }
        for w in &webhooks {
            if is_unnamed_workflow_meta(&w.meta, &known_agent_types) {
                unnamed_build_projects.insert(w.build_id, w.project_name.clone());
            }
        }
        for t in &mcp_tools {
            if is_unnamed_workflow_meta(&t.meta, &known_agent_types) {
                unnamed_build_projects.insert(t.build_id, t.project_name.clone());
            }
        }

        for (build_id, project_name) in unnamed_build_projects {
            let handler_count = count_unnamed_handlers_for_build(
                &handlers,
                &schedules,
                &webhooks,
                &mcp_tools,
                &known_agent_types,
                build_id,
            );
            if handler_count == 0 {
                continue;
            }
            cards.push(templates::WorkflowListCard {
                url: format!("/workflows/unnamed/{build_id}"),
                build_id: build_id.to_string(),
                kind: "unnamed".to_string(),
                kind_label: "Unnamed".to_string(),
                qualified_name: String::new(),
                display_name: "Unnamed Workflows".to_string(),
                description: "Project-level workflow topology inferred from deployed handlers, triggers, and sends.".to_string(),
                tags: Vec::new(),
                handler_count,
                project_name,
                source_file: None,
                source_line: None,
            });
        }

        if !search_query.is_empty() {
            let q = search_query.to_lowercase();
            cards.retain(|card| {
                card.display_name.to_lowercase().contains(&q)
                    || card.qualified_name.to_lowercase().contains(&q)
                    || card.description.to_lowercase().contains(&q)
                    || card.project_name.to_lowercase().contains(&q)
                    || card.kind_label.to_lowercase().contains(&q)
            });
        }
        cards.sort_by(|a, b| {
            a.project_name
                .cmp(&b.project_name)
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        cards
    } else {
        Vec::new()
    };

    let template = templates::AgentsList {
        title: "Workflows",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "workflows",
            &session,
            breadcrumbs,
        ),
        workflow_cards,
        search_query,
        active_tab,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn agents_detail_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    Path(url_path): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let qualified_name = url_path_to_qualified_name(&url_path);

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Workflows".to_string(),
        "/workflows".to_string(),
    ));

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("Environment not selected".to_string()).into_response();
        }
    };

    let agent = match Agent::get_agent_by_qualified_name(&db, &env_id, &qualified_name).await {
        Ok(a) => a,
        Err(_) => {
            return Html(format!("Agent not found: {qualified_name}")).into_response();
        }
    };

    let display_name = agent_display_name(&agent);
    breadcrumbs.push(templates::BreadcrumbItem::current(display_name.clone()));

    let active_tab = params.get("tab").map(|s| s.as_str()).unwrap_or("graph");
    let active_tab_owned: String = active_tab.to_string();

    let runs_page = params
        .get("rp")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);
    const RUNS_PER_PAGE: i64 = 20;

    // Handlers
    let handlers =
        EventHandler::get_event_handlers_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default();

    let schedules = Schedule::get_schedules_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();

    let webhooks_list = Webhook::get_by_env(&db, &env_id).await.unwrap_or_default();

    let mcp_tools = McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();

    let handler_displays = build_handler_displays(
        &handlers,
        &schedules,
        &webhooks_list,
        &mcp_tools,
        &agent.type_name,
    );

    let handler_count = handler_displays.len() as i64;

    // Runs (paginated, filtered by agent_type)
    let mut runs = hot::db::Run::get_runs_by_agent_type(
        &db,
        &env_id,
        &qualified_name,
        RUNS_PER_PAGE,
        (runs_page - 1) * RUNS_PER_PAGE,
    )
    .await
    .unwrap_or_default();
    crate::handlers::rehydrate_runs_for_display(blob_store.as_ref(), &session, &mut runs).await;

    let runs_total = hot::db::Run::get_count_by_agent_type(&db, &env_id, &qualified_name)
        .await
        .unwrap_or(0);

    let runs_total_pages = ((runs_total as f64) / (RUNS_PER_PAGE as f64)).ceil() as i64;
    let runs_total_pages = if runs_total_pages == 0 {
        1
    } else {
        runs_total_pages
    };

    let run_displays: Vec<templates::RunDisplay> = runs
        .iter()
        .map(|r| {
            templates::RunDisplay::from_with_timezone(
                r,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Streams (recent, involving this agent)
    // Get recent streams involving this agent (distinct stream_ids from recent runs)
    let agent_stream_ids: Vec<uuid::Uuid> = runs
        .iter()
        .map(|r| r.stream_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let mut streams: Vec<templates::StreamListItem> = Vec::new();
    for sid in agent_stream_ids.iter().take(10) {
        if let Ok(summary) = hot::db::stream::StreamSummary::get_stream(&db, sid).await {
            streams.push(templates::StreamListItem::from_with_timezone(
                &summary,
                &session.display_timezone,
                &session.timezone_abbreviation,
            ));
        }
    }

    let agent_card = templates::AgentCard {
        agent_id: agent.agent_id.to_string(),
        build_id: agent.build_id.to_string(),
        namespace_qualified_name: agent_qualified_name(&agent),
        qualified_name: qualified_name_to_url_path(&agent_qualified_name(&agent)),
        display_name,
        namespace: agent.namespace.clone(),
        description: agent.description.clone().unwrap_or_default(),
        tags: extract_tags(&agent.tags),
        handler_count,
        project_name: agent.project_name.clone(),
        source_file: agent.file.clone(),
        source_line: agent.line,
    };

    let template = templates::AgentsDetail {
        title: "Agent",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "workflows",
            &session,
            breadcrumbs,
        ),
        agent: agent_card,
        full_description: agent.description.clone().unwrap_or_default(),
        config_fields: extract_config_fields(&agent.config_fields),
        handlers: handler_displays,
        runs: run_displays,
        runs_current_page: runs_page,
        runs_total_pages,
        runs_has_next: runs_page < runs_total_pages,
        runs_has_prev: runs_page > 1,
        runs_total,
        streams,
        active_tab: match active_tab_owned.as_str() {
            "handlers" => "handlers",
            "runs" => "runs",
            "streams" => "streams",
            _ => "graph",
        },
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn workflow_detail_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(url_path): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let qualified_name = url_path_to_qualified_name(&url_path);

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Workflows".to_string(),
        "/workflows".to_string(),
    ));

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Html("Environment not selected".to_string()).into_response(),
    };

    let workflow =
        match Workflow::get_workflow_by_qualified_name(&db, &env_id, &qualified_name).await {
            Ok(w) => w,
            Err(_) => return Html(format!("Workflow not found: {qualified_name}")).into_response(),
        };

    let display_name = workflow_display_name(&workflow);
    breadcrumbs.push(templates::BreadcrumbItem::current(display_name.clone()));

    let handlers =
        EventHandler::get_event_handlers_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default();
    let schedules = Schedule::get_schedules_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();
    let webhooks = Webhook::get_by_env(&db, &env_id).await.unwrap_or_default();
    let mcp_tools = McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();

    let handler_displays = build_workflow_handler_displays(
        &handlers,
        &schedules,
        &webhooks,
        &mcp_tools,
        &workflow.type_name,
    );

    let workflow_card = templates::WorkflowListCard {
        url: format!(
            "/workflows/named/{}",
            qualified_name_to_url_path(&qualified_name)
        ),
        build_id: workflow.build_id.to_string(),
        kind: "workflow".to_string(),
        kind_label: "Workflow".to_string(),
        qualified_name: workflow_qualified_name(&workflow),
        display_name: display_name.clone(),
        description: workflow.description.clone().unwrap_or_default(),
        tags: extract_tags(&workflow.tags),
        handler_count: handler_displays.len() as i64,
        project_name: workflow.project_name.clone(),
        source_file: workflow.file.clone(),
        source_line: workflow.line,
    };

    let active_tab = params.get("tab").map(|s| s.as_str()).unwrap_or("graph");
    let active_tab = match active_tab {
        "handlers" => "handlers",
        _ => "graph",
    };

    let template = templates::WorkflowDetail {
        title: &display_name,
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "workflows",
            &session,
            breadcrumbs,
        ),
        workflow: workflow_card,
        graph_data_url: format!("/data/workflow-graph/workflows/{url_path}"),
        handlers: handler_displays,
        active_tab,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn unnamed_workflow_detail_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(build_id): Path<Uuid>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Workflows".to_string(),
        "/workflows".to_string(),
    ));

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Html("Environment not selected".to_string()).into_response(),
    };

    let agents = Agent::get_agents_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();
    let known_agent_types: AHashSet<String> = agents.iter().map(|a| a.type_name.clone()).collect();

    let handlers =
        EventHandler::get_event_handlers_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default();
    let schedules = Schedule::get_schedules_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();
    let webhooks = Webhook::get_by_env(&db, &env_id).await.unwrap_or_default();
    let mcp_tools = McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();

    let handler_displays = build_unnamed_handler_displays(
        &handlers,
        &schedules,
        &webhooks,
        &mcp_tools,
        &known_agent_types,
        build_id,
    );

    if handler_displays.is_empty() {
        return Html(format!("Unnamed workflow group not found: {build_id}")).into_response();
    }

    let project_name = handlers
        .iter()
        .find(|h| h.build_id == build_id)
        .map(|h| h.project_name.clone())
        .or_else(|| {
            schedules
                .iter()
                .find(|s| s.build_id == build_id)
                .map(|s| s.project_name.clone())
        })
        .or_else(|| {
            webhooks
                .iter()
                .find(|w| w.build_id == build_id)
                .map(|w| w.project_name.clone())
        })
        .or_else(|| {
            mcp_tools
                .iter()
                .find(|t| t.build_id == build_id)
                .map(|t| t.project_name.clone())
        })
        .unwrap_or_else(|| "Project".to_string());

    let display_name = "Unnamed Workflows".to_string();
    breadcrumbs.push(templates::BreadcrumbItem::current(display_name.clone()));

    let workflow_card = templates::WorkflowListCard {
        url: format!("/workflows/unnamed/{build_id}"),
        build_id: build_id.to_string(),
        kind: "unnamed".to_string(),
        kind_label: "Unnamed".to_string(),
        qualified_name: String::new(),
        display_name: display_name.clone(),
        description:
            "Project-level workflow topology inferred from deployed handlers, triggers, and sends."
                .to_string(),
        tags: Vec::new(),
        handler_count: handler_displays.len() as i64,
        project_name,
        source_file: None,
        source_line: None,
    };

    let active_tab = params.get("tab").map(|s| s.as_str()).unwrap_or("graph");
    let active_tab = match active_tab {
        "handlers" => "handlers",
        _ => "graph",
    };

    let template = templates::WorkflowDetail {
        title: &display_name,
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "workflows",
            &session,
            breadcrumbs,
        ),
        workflow: workflow_card,
        graph_data_url: format!("/data/workflow-graph/unnamed/{build_id}"),
        handlers: handler_displays,
        active_tab,
    };

    Html(template.render().unwrap()).into_response()
}
