use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Call, DatabaseError, DatabasePool};

/// A node in the execution hierarchy tree
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HierarchyNode {
    Call {
        id: String,
        call_id: Uuid,
        name: String,
        function_name: String,
        static_scope: String,
        start_time: DateTime<Utc>,
        stop_time: Option<DateTime<Utc>>,
        duration_us: Option<i64>,
        call_depth: i32,
        runtime_path: Option<String>,
        args: Box<Option<serde_json::Value>>,
        return_value: Box<Option<serde_json::Value>>,
        flow: Box<Option<serde_json::Value>>,
        file: Option<String>,
        line: Option<i32>,
        children: Vec<HierarchyNode>,
    },
    Var {
        id: String,
        var_id: Uuid,
        name: String,
        var_name: String,
        namespace: String,
        start_time: DateTime<Utc>,
        stop_time: Option<DateTime<Utc>>,
        duration_us: Option<i64>,
    },
}

/// Response structure for the hierarchy API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchyResponse {
    pub run_id: Uuid,
    pub total_duration_us: i64,
    pub total_calls: usize,
    pub total_vars: usize,
    pub tree: Vec<HierarchyNode>,
}

impl HierarchyNode {
    pub fn id(&self) -> &str {
        match self {
            HierarchyNode::Call { id, .. } => id,
            HierarchyNode::Var { id, .. } => id,
        }
    }

    pub fn start_time(&self) -> DateTime<Utc> {
        match self {
            HierarchyNode::Call { start_time, .. } => *start_time,
            HierarchyNode::Var { start_time, .. } => *start_time,
        }
    }

    pub fn duration_us(&self) -> Option<i64> {
        match self {
            HierarchyNode::Call { duration_us, .. } => *duration_us,
            HierarchyNode::Var { duration_us, .. } => *duration_us,
        }
    }
}

/// Build a complete execution hierarchy for a run
pub async fn build_hierarchy(
    pool: &DatabasePool,
    run_id: &Uuid,
) -> Result<HierarchyResponse, DatabaseError> {
    // Fetch all calls
    let calls = Call::get_calls_by_run(pool, run_id).await?;

    // Build tree structure
    let tree = build_tree(&calls);

    // Calculate totals
    let total_duration_us = calls
        .iter()
        .filter(|c| c.parent_call_id.is_none())
        .filter_map(|c| c.duration_us)
        .sum();

    Ok(HierarchyResponse {
        run_id: *run_id,
        total_duration_us,
        total_calls: calls.len(),
        total_vars: 0, // Vars are not included in hierarchy responses yet.
        tree,
    })
}

/// Build tree structure from flat call list
fn build_tree(calls: &[Call]) -> Vec<HierarchyNode> {
    // Get root calls (no parent)
    let root_calls: Vec<&Call> = calls
        .iter()
        .filter(|c| c.parent_call_id.is_none())
        .collect();

    // Build tree recursively
    root_calls
        .iter()
        .map(|call| build_call_node(call, calls))
        .collect()
}

/// Build a call node with its children
fn build_call_node(call: &Call, all_calls: &[Call]) -> HierarchyNode {
    // Get child calls
    let child_calls: Vec<&Call> = all_calls
        .iter()
        .filter(|c| c.parent_call_id == Some(call.call_id))
        .collect();

    // Build children recursively
    let mut children: Vec<HierarchyNode> = child_calls
        .iter()
        .map(|child_call| build_call_node(child_call, all_calls))
        .collect();

    // Sort children by start time
    children.sort_by_key(|a| a.start_time());

    HierarchyNode::Call {
        id: format!("call-{}", call.call_id),
        call_id: call.call_id,
        name: call.function_name.clone(),
        function_name: call.function_name.clone(),
        static_scope: call.static_scope.clone(),
        start_time: call.start_time,
        stop_time: call.stop_time,
        duration_us: call.duration_us,
        call_depth: call.call_depth,
        runtime_path: call.runtime_path.clone(),
        args: Box::new(call.args.clone()),
        return_value: Box::new(call.return_value.clone()),
        flow: Box::new(call.flow.clone()),
        file: call.file.clone(),
        line: call.line,
        children,
    }
}
