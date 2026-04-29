use crate::auth::Session;
use ahash::{AHashMap, AHashSet};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Json};
use hot::db::{
    Agent, AgentWithProject, DatabasePool, EventHandler, McpTool, Schedule, Webhook, Workflow,
    WorkflowWithProject,
};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Data structures for agent graph (tailored for ECharts graph series)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Debug)]
pub struct AgentGraphData {
    pub nodes: Vec<AgentGraphNode>,
    pub edges: Vec<AgentGraphEdge>,
    pub categories: Vec<AgentGraphCategory>,
}

#[derive(serde::Serialize, Debug)]
pub struct AgentGraphNode {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub category: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

#[derive(serde::Serialize, Debug)]
pub struct AgentGraphEdge {
    pub source: String,
    pub target: String,
    pub edge_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(serde::Serialize, Debug)]
pub struct AgentGraphCategory {
    pub name: String,
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn meta_agent_name(meta: &Option<JsonValue>) -> Option<String> {
    meta.as_ref()
        .and_then(|m| m.get("agent"))
        .and_then(|a| a.as_str())
        .map(|s| {
            s.rsplit_once('/')
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| s.to_string())
        })
}

fn meta_on_event(meta: &Option<JsonValue>) -> Option<String> {
    meta.as_ref()
        .and_then(|m| m.get("on-event"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn meta_sends(meta: &Option<JsonValue>) -> Vec<SendInfo> {
    let arr = match meta.as_ref().and_then(|m| m.get("sends")) {
        Some(JsonValue::Array(a)) => a,
        _ => return Vec::new(),
    };
    arr.iter()
        .filter_map(|item| match item {
            JsonValue::String(s) => Some(SendInfo {
                event: s.clone(),
                doc: None,
            }),
            JsonValue::Object(obj) => obj.get("event").and_then(|e| e.as_str()).map(|e| SendInfo {
                event: e.to_string(),
                doc: obj
                    .get("doc")
                    .and_then(|d| d.as_str())
                    .map(|d| d.to_string()),
            }),
            _ => None,
        })
        .collect()
}

struct SendInfo {
    event: String,
    doc: Option<String>,
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

fn short_type_name(name: &str) -> String {
    name.rsplit_once('/')
        .map(|(_, ty)| ty.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn meta_doc(meta: &Option<JsonValue>) -> Option<String> {
    meta.as_ref()
        .and_then(|m| m.get("doc"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn meta_retry(meta: &Option<JsonValue>) -> Option<String> {
    let val = meta.as_ref()?.get("retry")?;
    match val {
        JsonValue::Number(n) => Some(format!("{} attempts", n)),
        JsonValue::Object(obj) => {
            let attempts = obj.get("attempts").and_then(|v| v.as_u64());
            let delay = obj.get("delay").and_then(|v| v.as_u64());
            match (attempts, delay) {
                (Some(a), Some(d)) if d >= 1000 => {
                    Some(format!("{} attempts, {}s delay", a, d / 1000))
                }
                (Some(a), Some(d)) => Some(format!("{} attempts, {}ms delay", a, d)),
                (Some(a), None) => Some(format!("{} attempts", a)),
                _ => None,
            }
        }
        JsonValue::String(s) => Some(s.clone()),
        _ => None,
    }
}

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

fn agent_display_name(agent: &AgentWithProject) -> String {
    agent
        .name
        .clone()
        .unwrap_or_else(|| agent.type_name.clone())
}

fn workflow_display_name(workflow: &WorkflowWithProject) -> String {
    workflow
        .name
        .clone()
        .unwrap_or_else(|| workflow.type_name.clone())
}

fn workflow_meta_matches_filter(
    meta: &Option<JsonValue>,
    filter: &str,
    workflows: &[WorkflowWithProject],
) -> bool {
    let workflow_names = meta_workflow_type_names(meta);

    if filter == "Unnamed Workflow" {
        return workflow_names.is_empty();
    }

    workflow_names.iter().any(|name| name == filter)
        || workflows.iter().any(|workflow| {
            let display = workflow_display_name(workflow);
            (display == filter || workflow.type_name == filter)
                && workflow_names
                    .iter()
                    .any(|name| name == &workflow.type_name)
        })
}

fn agent_or_workflow_matches_filter(
    agent: &AgentWithProject,
    meta: &Option<JsonValue>,
    filter: &str,
    workflows: &[WorkflowWithProject],
) -> bool {
    if filter == "Unnamed Workflow" {
        return false;
    }

    filter == agent_display_name(agent)
        || filter == agent.type_name
        || workflow_meta_matches_filter(meta, filter, workflows)
}

fn agent_qualified_name_internal(agent: &AgentWithProject) -> String {
    format!("{}/{}", agent.namespace, agent.type_name)
}

fn agent_qualified_name_url(agent: &AgentWithProject) -> String {
    super::agents::qualified_name_to_url_path(&agent_qualified_name_internal(agent))
}

fn handler_node_id(ns: &str, var: &str) -> String {
    format!("fn:{}/{}", ns, var)
}

fn event_type_node_id(event_type: &str) -> String {
    format!("evt:{}", event_type)
}

fn schedule_node_id(ns: &str, var: &str) -> String {
    format!("sched:{}/{}", ns, var)
}

fn webhook_node_id(ns: &str, var: &str) -> String {
    format!("wh:{}/{}", ns, var)
}

fn mcp_node_id(ns: &str, var: &str) -> String {
    format!("mcp:{}/{}", ns, var)
}

// ---------------------------------------------------------------------------
// Graph builder
// ---------------------------------------------------------------------------

struct GraphBuilder {
    nodes: Vec<AgentGraphNode>,
    edges: Vec<AgentGraphEdge>,
    seen_nodes: AHashMap<String, usize>,
    seen_edges: AHashSet<String>,
    categories: Vec<AgentGraphCategory>,
}

impl GraphBuilder {
    fn new(categories: Vec<AgentGraphCategory>) -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: AHashMap::new(),
            seen_edges: AHashSet::new(),
            categories,
        }
    }

    fn add_node(&mut self, node: AgentGraphNode) {
        if self.seen_nodes.contains_key(&node.id) {
            return;
        }
        self.seen_nodes.insert(node.id.clone(), self.nodes.len());
        self.nodes.push(node);
    }

    fn add_or_merge_handler(&mut self, node: AgentGraphNode) {
        if let Some(&idx) = self.seen_nodes.get(&node.id) {
            let existing = &mut self.nodes[idx];
            if existing.description.is_none() {
                existing.description = node.description;
            }
            if existing.source_file.is_none() {
                existing.source_file = node.source_file;
                existing.source_line = node.source_line;
                existing.source_build_id = node.source_build_id;
            }
            if existing.retry.is_none() {
                existing.retry = node.retry;
            }
        } else {
            self.seen_nodes.insert(node.id.clone(), self.nodes.len());
            self.nodes.push(node);
        }
    }

    fn add_edge(&mut self, edge: AgentGraphEdge) {
        let key = format!("{}|{}|{}", edge.source, edge.target, edge.edge_type);
        if self.seen_edges.contains(&key) {
            return;
        }
        self.seen_edges.insert(key);
        self.edges.push(edge);
    }

    fn add_event_type(&mut self, event_type: &str, category: usize) {
        let id = event_type_node_id(event_type);
        self.add_node(AgentGraphNode {
            id,
            name: event_type.to_string(),
            node_type: "event_type".to_string(),
            category,
            project: None,
            description: None,
            namespace: None,
            qualified_name: None,
            agent_name: None,
            detail: None,
            tags: Vec::new(),
            source_file: None,
            source_line: None,
            source_build_id: None,
            retry: None,
            active: None,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn add_schedule_node(
        &mut self,
        ns: &str,
        var: &str,
        cron: &str,
        category: usize,
        file: Option<&str>,
        line: Option<i32>,
        build_id: Option<Uuid>,
        active: bool,
        retry: Option<String>,
    ) {
        let id = schedule_node_id(ns, var);
        self.add_node(AgentGraphNode {
            id,
            name: cron.to_string(),
            node_type: "schedule".to_string(),
            category,
            project: None,
            description: None,
            namespace: Some(ns.to_string()),
            qualified_name: None,
            agent_name: None,
            detail: Some(format!("{}/{}", ns, var)),
            tags: Vec::new(),
            source_file: file.map(|s| s.to_string()),
            source_line: line,
            source_build_id: build_id.map(|id| id.to_string()),
            retry,
            active: Some(active),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn add_webhook_node(
        &mut self,
        ns: &str,
        var: &str,
        method: &str,
        service: &str,
        path: &str,
        category: usize,
        file: Option<&str>,
        line: Option<i32>,
        build_id: Option<Uuid>,
    ) {
        let id = webhook_node_id(ns, var);
        let display = format!("{} {}{}", method, service, path);
        self.add_node(AgentGraphNode {
            id,
            name: display,
            node_type: "webhook".to_string(),
            category,
            project: None,
            description: None,
            namespace: Some(ns.to_string()),
            qualified_name: None,
            agent_name: None,
            detail: Some(format!("{}/{}", ns, var)),
            tags: Vec::new(),
            source_file: file.map(|s| s.to_string()),
            source_line: line,
            source_build_id: build_id.map(|id| id.to_string()),
            retry: None,
            active: None,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn add_mcp_node(
        &mut self,
        ns: &str,
        var: &str,
        tool_name: &str,
        service: &str,
        description: Option<&str>,
        category: usize,
        file: Option<&str>,
        line: Option<i32>,
        build_id: Option<Uuid>,
    ) {
        let id = mcp_node_id(ns, var);
        self.add_node(AgentGraphNode {
            id,
            name: tool_name.to_string(),
            node_type: "mcp_tool".to_string(),
            category,
            project: None,
            description: description.map(|s| s.to_string()),
            namespace: Some(ns.to_string()),
            qualified_name: None,
            agent_name: None,
            detail: Some(service.to_string()),
            tags: Vec::new(),
            source_file: file.map(|s| s.to_string()),
            source_line: line,
            source_build_id: build_id.map(|id| id.to_string()),
            retry: None,
            active: None,
        });
    }

    fn build(self) -> AgentGraphData {
        AgentGraphData {
            nodes: self.nodes,
            edges: self.edges,
            categories: self.categories,
        }
    }
}

// ---------------------------------------------------------------------------
// Build graph from DB data
// ---------------------------------------------------------------------------
#[allow(clippy::too_many_arguments)]
fn build_agent_graph(
    agents: &[AgentWithProject],
    workflows: &[WorkflowWithProject],
    handlers: &[hot::db::EventHandlerWithProject],
    schedules: &[hot::db::ScheduleWithProject],
    webhooks: &[hot::db::WebhookWithProject],
    mcp_tools: &[hot::db::McpToolWithProject],
    filter_agent: Option<&str>,
    filter_workflow: Option<&str>,
) -> AgentGraphData {
    let filtered_agents: Vec<&AgentWithProject> = agents
        .iter()
        .filter(|a| {
            if let Some(filter) = filter_agent {
                agent_qualified_name_internal(a) == filter
            } else {
                true
            }
        })
        .collect();

    let mut agent_cat_map: AHashMap<String, usize> = AHashMap::new();
    let mut workflow_cat_map: AHashMap<String, usize> = AHashMap::new();
    let mut categories = Vec::new();

    for agent in &filtered_agents {
        let display = agent_display_name(agent);
        let idx = categories.len();
        agent_cat_map.insert(agent.type_name.clone(), idx);
        categories.push(AgentGraphCategory {
            name: display,
            kind: "agent".to_string(),
        });
    }
    for workflow in workflows {
        let display = workflow_display_name(workflow);
        let idx = categories.len();
        workflow_cat_map.insert(workflow.type_name.clone(), idx);
        categories.push(AgentGraphCategory {
            name: display,
            kind: "workflow".to_string(),
        });
    }
    let unnamed_workflow_category = categories.len();
    categories.push(AgentGraphCategory {
        name: "Unnamed Workflow".to_string(),
        kind: "workflow".to_string(),
    });
    let event_category = categories.len();
    categories.push(AgentGraphCategory {
        name: "Event".to_string(),
        kind: "technical".to_string(),
    });
    let schedule_category = categories.len();
    categories.push(AgentGraphCategory {
        name: "Schedule".to_string(),
        kind: "technical".to_string(),
    });
    let webhook_category = categories.len();
    categories.push(AgentGraphCategory {
        name: "Webhook".to_string(),
        kind: "technical".to_string(),
    });
    let mcp_category = categories.len();
    categories.push(AgentGraphCategory {
        name: "MCP Tool".to_string(),
        kind: "technical".to_string(),
    });

    let mut gb = GraphBuilder::new(categories);
    let known_agent_types: AHashSet<String> = filtered_agents
        .iter()
        .map(|a| a.type_name.clone())
        .collect();

    let workflow_category_for_meta = |meta: &Option<JsonValue>| -> usize {
        meta_workflow_type_names(meta)
            .into_iter()
            .find_map(|name| workflow_cat_map.get(&name).copied())
            .unwrap_or(unnamed_workflow_category)
    };

    for agent in &filtered_agents {
        let qn = agent_qualified_name_url(agent);
        let agent_name = agent_display_name(agent);
        let agent_cat = agent_cat_map[&agent.type_name];

        // -- Event handlers --
        for h in handlers {
            if meta_agent_name(&h.meta).as_deref() != Some(&*agent.type_name) {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !agent_or_workflow_matches_filter(agent, &h.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&h.ns, &h.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: h.var.clone(),
                node_type: "handler".to_string(),
                category: agent_cat,
                project: Some(h.project_name.clone()),
                description: meta_doc(&h.meta),
                namespace: Some(h.ns.clone()),
                qualified_name: Some(qn.clone()),
                agent_name: Some(agent_name.clone()),
                detail: None,
                tags: Vec::new(),
                source_file: h.file.clone(),
                source_line: h.line,
                source_build_id: Some(h.build_id.to_string()),
                retry: meta_retry(&h.meta),
                active: None,
            });

            gb.add_event_type(&h.event_type, event_category);
            gb.add_edge(AgentGraphEdge {
                source: event_type_node_id(&h.event_type),
                target: fn_id.clone(),
                edge_type: "handles".to_string(),
                label: Some(h.event_type.clone()),
                doc: None,
            });

            for send in meta_sends(&h.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }

        // -- Schedules (ingress nodes) --
        for s in schedules {
            if meta_agent_name(&s.meta).as_deref() != Some(&*agent.type_name) {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !agent_or_workflow_matches_filter(agent, &s.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&s.ns, &s.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: s.var.clone(),
                node_type: "handler".to_string(),
                category: agent_cat,
                project: Some(s.project_name.clone()),
                description: meta_doc(&s.meta),
                namespace: Some(s.ns.clone()),
                qualified_name: Some(qn.clone()),
                agent_name: Some(agent_name.clone()),
                detail: None,
                tags: Vec::new(),
                source_file: s.file.clone(),
                source_line: s.line,
                source_build_id: Some(s.build_id.to_string()),
                retry: meta_retry(&s.meta),
                active: None,
            });

            gb.add_schedule_node(
                &s.ns,
                &s.var,
                &s.cron,
                schedule_category,
                s.file.as_deref(),
                s.line,
                Some(s.build_id),
                s.active,
                meta_retry(&s.meta),
            );
            gb.add_edge(AgentGraphEdge {
                source: schedule_node_id(&s.ns, &s.var),
                target: fn_id.clone(),
                edge_type: "triggers".to_string(),
                label: Some(s.cron.clone()),
                doc: None,
            });

            if let Some(event_type) = meta_on_event(&s.meta) {
                gb.add_event_type(&event_type, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: event_type_node_id(&event_type),
                    target: fn_id.clone(),
                    edge_type: "handles".to_string(),
                    label: Some(event_type),
                    doc: None,
                });
            }

            for send in meta_sends(&s.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }

        // -- Webhooks (ingress nodes) --
        for w in webhooks {
            if meta_agent_name(&w.meta).as_deref() != Some(&*agent.type_name) {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !agent_or_workflow_matches_filter(agent, &w.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&w.ns, &w.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: w.var.clone(),
                node_type: "handler".to_string(),
                category: agent_cat,
                project: Some(w.project_name.clone()),
                description: meta_doc(&w.meta),
                namespace: Some(w.ns.clone()),
                qualified_name: Some(qn.clone()),
                agent_name: Some(agent_name.clone()),
                detail: None,
                tags: Vec::new(),
                source_file: w.file.clone(),
                source_line: w.line,
                source_build_id: Some(w.build_id.to_string()),
                retry: None,
                active: None,
            });

            gb.add_webhook_node(
                &w.ns,
                &w.var,
                &w.method,
                &w.service,
                &w.path,
                webhook_category,
                w.file.as_deref(),
                w.line,
                Some(w.build_id),
            );
            gb.add_edge(AgentGraphEdge {
                source: webhook_node_id(&w.ns, &w.var),
                target: fn_id.clone(),
                edge_type: "triggers".to_string(),
                label: None,
                doc: None,
            });

            if let Some(event_type) = meta_on_event(&w.meta) {
                gb.add_event_type(&event_type, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: event_type_node_id(&event_type),
                    target: fn_id.clone(),
                    edge_type: "handles".to_string(),
                    label: Some(event_type),
                    doc: None,
                });
            }

            for send in meta_sends(&w.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }

        // -- MCP tools (ingress nodes) --
        for t in mcp_tools {
            if meta_agent_name(&t.meta).as_deref() != Some(&*agent.type_name) {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !agent_or_workflow_matches_filter(agent, &t.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&t.ns, &t.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: t.var.clone(),
                node_type: "handler".to_string(),
                category: agent_cat,
                project: Some(t.project_name.clone()),
                description: t.description.clone().or_else(|| meta_doc(&t.meta)),
                namespace: Some(t.ns.clone()),
                qualified_name: Some(qn.clone()),
                agent_name: Some(agent_name.clone()),
                detail: None,
                tags: Vec::new(),
                source_file: t.file.clone(),
                source_line: t.line,
                source_build_id: Some(t.build_id.to_string()),
                retry: None,
                active: None,
            });

            let mcp_desc = t.description.clone().or_else(|| meta_doc(&t.meta));
            gb.add_mcp_node(
                &t.ns,
                &t.var,
                &t.name,
                &t.service,
                mcp_desc.as_deref(),
                mcp_category,
                t.file.as_deref(),
                t.line,
                Some(t.build_id),
            );
            gb.add_edge(AgentGraphEdge {
                source: mcp_node_id(&t.ns, &t.var),
                target: fn_id.clone(),
                edge_type: "triggers".to_string(),
                label: Some(t.name.clone()),
                doc: t.description.clone(),
            });

            for send in meta_sends(&t.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }
    }

    // Also render project-level workflow artifacts that are not attached to a
    // known agent. This makes "All Workflows" include unnamed and workflow-only
    // topology instead of using agents as the root set.
    if filter_agent.is_none() {
        for h in handlers {
            if meta_agent_name(&h.meta)
                .as_ref()
                .is_some_and(|name| known_agent_types.contains(name))
            {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !workflow_meta_matches_filter(&h.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&h.ns, &h.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: h.var.clone(),
                node_type: "handler".to_string(),
                category: workflow_category_for_meta(&h.meta),
                project: Some(h.project_name.clone()),
                description: meta_doc(&h.meta),
                namespace: Some(h.ns.clone()),
                qualified_name: None,
                agent_name: meta_agent_name(&h.meta),
                detail: None,
                tags: Vec::new(),
                source_file: h.file.clone(),
                source_line: h.line,
                source_build_id: Some(h.build_id.to_string()),
                retry: meta_retry(&h.meta),
                active: None,
            });
            gb.add_event_type(&h.event_type, event_category);
            gb.add_edge(AgentGraphEdge {
                source: event_type_node_id(&h.event_type),
                target: fn_id.clone(),
                edge_type: "handles".to_string(),
                label: Some(h.event_type.clone()),
                doc: None,
            });
            for send in meta_sends(&h.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }

        for s in schedules {
            if meta_agent_name(&s.meta)
                .as_ref()
                .is_some_and(|name| known_agent_types.contains(name))
            {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !workflow_meta_matches_filter(&s.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&s.ns, &s.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: s.var.clone(),
                node_type: "handler".to_string(),
                category: workflow_category_for_meta(&s.meta),
                project: Some(s.project_name.clone()),
                description: meta_doc(&s.meta),
                namespace: Some(s.ns.clone()),
                qualified_name: None,
                agent_name: meta_agent_name(&s.meta),
                detail: None,
                tags: Vec::new(),
                source_file: s.file.clone(),
                source_line: s.line,
                source_build_id: Some(s.build_id.to_string()),
                retry: meta_retry(&s.meta),
                active: None,
            });
            gb.add_schedule_node(
                &s.ns,
                &s.var,
                &s.cron,
                schedule_category,
                s.file.as_deref(),
                s.line,
                Some(s.build_id),
                s.active,
                meta_retry(&s.meta),
            );
            gb.add_edge(AgentGraphEdge {
                source: schedule_node_id(&s.ns, &s.var),
                target: fn_id.clone(),
                edge_type: "triggers".to_string(),
                label: Some(s.cron.clone()),
                doc: None,
            });
            if let Some(event_type) = meta_on_event(&s.meta) {
                gb.add_event_type(&event_type, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: event_type_node_id(&event_type),
                    target: fn_id.clone(),
                    edge_type: "handles".to_string(),
                    label: Some(event_type),
                    doc: None,
                });
            }
            for send in meta_sends(&s.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }

        for w in webhooks {
            if meta_agent_name(&w.meta)
                .as_ref()
                .is_some_and(|name| known_agent_types.contains(name))
            {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !workflow_meta_matches_filter(&w.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&w.ns, &w.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: w.var.clone(),
                node_type: "handler".to_string(),
                category: workflow_category_for_meta(&w.meta),
                project: Some(w.project_name.clone()),
                description: meta_doc(&w.meta),
                namespace: Some(w.ns.clone()),
                qualified_name: None,
                agent_name: meta_agent_name(&w.meta),
                detail: None,
                tags: Vec::new(),
                source_file: w.file.clone(),
                source_line: w.line,
                source_build_id: Some(w.build_id.to_string()),
                retry: None,
                active: None,
            });
            gb.add_webhook_node(
                &w.ns,
                &w.var,
                &w.method,
                &w.service,
                &w.path,
                webhook_category,
                w.file.as_deref(),
                w.line,
                Some(w.build_id),
            );
            gb.add_edge(AgentGraphEdge {
                source: webhook_node_id(&w.ns, &w.var),
                target: fn_id.clone(),
                edge_type: "triggers".to_string(),
                label: None,
                doc: None,
            });
            if let Some(event_type) = meta_on_event(&w.meta) {
                gb.add_event_type(&event_type, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: event_type_node_id(&event_type),
                    target: fn_id.clone(),
                    edge_type: "handles".to_string(),
                    label: Some(event_type),
                    doc: None,
                });
            }
            for send in meta_sends(&w.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }

        for t in mcp_tools {
            if meta_agent_name(&t.meta)
                .as_ref()
                .is_some_and(|name| known_agent_types.contains(name))
            {
                continue;
            }
            if let Some(filter) = filter_workflow
                && !workflow_meta_matches_filter(&t.meta, filter, workflows)
            {
                continue;
            }
            let fn_id = handler_node_id(&t.ns, &t.var);
            gb.add_or_merge_handler(AgentGraphNode {
                id: fn_id.clone(),
                name: t.var.clone(),
                node_type: "handler".to_string(),
                category: workflow_category_for_meta(&t.meta),
                project: Some(t.project_name.clone()),
                description: t.description.clone().or_else(|| meta_doc(&t.meta)),
                namespace: Some(t.ns.clone()),
                qualified_name: None,
                agent_name: meta_agent_name(&t.meta),
                detail: None,
                tags: Vec::new(),
                source_file: t.file.clone(),
                source_line: t.line,
                source_build_id: Some(t.build_id.to_string()),
                retry: None,
                active: None,
            });
            let mcp_desc = t.description.clone().or_else(|| meta_doc(&t.meta));
            gb.add_mcp_node(
                &t.ns,
                &t.var,
                &t.name,
                &t.service,
                mcp_desc.as_deref(),
                mcp_category,
                t.file.as_deref(),
                t.line,
                Some(t.build_id),
            );
            gb.add_edge(AgentGraphEdge {
                source: mcp_node_id(&t.ns, &t.var),
                target: fn_id.clone(),
                edge_type: "triggers".to_string(),
                label: Some(t.name.clone()),
                doc: t.description.clone(),
            });
            for send in meta_sends(&t.meta) {
                gb.add_event_type(&send.event, event_category);
                gb.add_edge(AgentGraphEdge {
                    source: fn_id.clone(),
                    target: event_type_node_id(&send.event),
                    edge_type: "sends".to_string(),
                    label: Some(send.event.clone()),
                    doc: send.doc,
                });
            }
        }
    }

    gb.build()
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

pub async fn agent_graph_data_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Json(serde_json::json!({"nodes": [], "edges": [], "categories": []})),
    };

    let project_filter = params.get("project").map(|s| s.as_str());
    let tag_filter = params.get("tag").map(|s| s.as_str());
    let search_filter = params.get("search").cloned().unwrap_or_default();
    let agent_filter = params
        .get("agent")
        .map(|s| super::agents::url_path_to_qualified_name(s));
    let workflow_filter = params.get("workflow").map(|s| s.as_str());

    let agents = Agent::get_agents_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

    let filtered_agents: Vec<AgentWithProject> = agents
        .into_iter()
        .filter(|a| {
            if let Some(proj) = project_filter
                && a.project_name != proj
            {
                return false;
            }
            if let Some(tag) = tag_filter {
                let tags = extract_tags(&a.tags);
                if !tags.iter().any(|t| t == tag) {
                    return false;
                }
            }
            if !search_filter.is_empty() {
                let q = search_filter.to_lowercase();
                let name_match = agent_display_name(a).to_lowercase().contains(&q);
                let ns_match = a.namespace.to_lowercase().contains(&q);
                if !name_match && !ns_match {
                    return false;
                }
            }
            true
        })
        .collect();

    let workflows = Workflow::get_workflows_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

    let filtered_workflows: Vec<WorkflowWithProject> = workflows
        .into_iter()
        .filter(|w| {
            if let Some(proj) = project_filter
                && w.project_name != proj
            {
                return false;
            }
            if let Some(tag) = tag_filter {
                let tags = extract_tags(&w.tags);
                if !tags.iter().any(|t| t == tag) {
                    return false;
                }
            }
            if !search_filter.is_empty() {
                let q = search_filter.to_lowercase();
                let name_match = workflow_display_name(w).to_lowercase().contains(&q);
                let ns_match = w.namespace.to_lowercase().contains(&q);
                if !name_match && !ns_match {
                    return false;
                }
            }
            true
        })
        .collect();

    let mut handlers =
        EventHandler::get_event_handlers_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default();

    let mut schedules = Schedule::get_schedules_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();

    let mut webhooks = Webhook::get_by_env(&db, &env_id).await.unwrap_or_default();

    let mut mcp_tools = McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
        .await
        .unwrap_or_default();

    if let Some(proj) = project_filter {
        handlers.retain(|h| h.project_name == proj);
        schedules.retain(|s| s.project_name == proj);
        webhooks.retain(|w| w.project_name == proj);
        mcp_tools.retain(|t| t.project_name == proj);
    }

    let graph = build_agent_graph(
        &filtered_agents,
        &filtered_workflows,
        &handlers,
        &schedules,
        &webhooks,
        &mcp_tools,
        agent_filter.as_deref(),
        workflow_filter,
    );

    Json(serde_json::to_value(graph).unwrap_or_default())
}

pub async fn agent_graph_detail_data_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(url_path): Path<String>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let qualified_name = super::agents::url_path_to_qualified_name(&url_path);
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Json(serde_json::json!({"nodes": [], "edges": [], "categories": []})),
    };

    let agents = Agent::get_agents_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

    let workflows = Workflow::get_workflows_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

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

    let graph = build_agent_graph(
        &agents,
        &workflows,
        &handlers,
        &schedules,
        &webhooks,
        &mcp_tools,
        Some(&qualified_name),
        None,
    );

    Json(serde_json::to_value(graph).unwrap_or_default())
}

pub async fn workflow_graph_detail_data_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(url_path): Path<String>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let qualified_name = super::agents::url_path_to_qualified_name(&url_path);
    let workflow_type = qualified_name
        .rsplit_once('/')
        .map(|(_, type_name)| type_name.to_string())
        .unwrap_or_else(|| qualified_name.clone());

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Json(serde_json::json!({"nodes": [], "edges": [], "categories": []})),
    };

    let agents = Agent::get_agents_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

    let workflows = Workflow::get_workflows_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

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

    let graph = build_agent_graph(
        &agents,
        &workflows,
        &handlers,
        &schedules,
        &webhooks,
        &mcp_tools,
        None,
        Some(&workflow_type),
    );

    Json(serde_json::to_value(graph).unwrap_or_default())
}

pub async fn unnamed_workflow_graph_detail_data_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(build_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Json(serde_json::json!({"nodes": [], "edges": [], "categories": []})),
    };

    let agents = Agent::get_agents_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

    let workflows = Workflow::get_workflows_by_env_deployed(&db, &env_id)
        .await
        .unwrap_or_default();

    let handlers: Vec<_> =
        EventHandler::get_event_handlers_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|h| h.build_id == build_id)
            .collect();

    let schedules: Vec<_> =
        Schedule::get_schedules_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.build_id == build_id)
            .collect();

    let webhooks: Vec<_> = Webhook::get_by_env(&db, &env_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.build_id == build_id)
        .collect();

    let mcp_tools: Vec<_> =
        McpTool::get_mcp_tools_by_env_deployed(&db, &env_id, Some(1000), Some(0))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.build_id == build_id)
            .collect();

    let graph = build_agent_graph(
        &agents,
        &workflows,
        &handlers,
        &schedules,
        &webhooks,
        &mcp_tools,
        None,
        Some("Unnamed Workflow"),
    );

    Json(serde_json::to_value(graph).unwrap_or_default())
}
