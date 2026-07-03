use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CallHeader, DatabaseError, DatabasePool};

/// A node in the execution hierarchy tree.
///
/// Nodes intentionally carry only lightweight metadata: the potentially large
/// `args`/`return_value`/`flow` payloads are NOT embedded in the tree. The UI
/// fetches them per call on demand (`GET /data/calls/{call_id}` in hot_app),
/// which keeps the hierarchy response small and guarantees that spilled
/// BlobRef payloads never travel with the tree. `has_args`/`has_return_value`
/// tell the UI whether a detail fetch will yield anything.
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
        has_args: bool,
        has_return_value: bool,
        flow: Option<serde_json::Value>,
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

/// Keep only the small flow metadata the tree rendering needs (pills, borders,
/// branch indentation). Flow payloads can carry arbitrary values — and can
/// even be spilled to a BlobRef — so anything beyond these keys stays out of
/// the hierarchy response; the full flow comes from the per-call detail fetch.
fn slim_flow(flow: &Option<serde_json::Value>) -> Option<serde_json::Value> {
    let obj = flow.as_ref()?.as_object()?;
    let mut slim = serde_json::Map::new();
    for key in ["type", "inline", "fn", "branch"] {
        if let Some(v) = obj.get(key) {
            slim.insert(key.to_string(), v.clone());
        }
    }
    if slim.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(slim))
    }
}

/// Build a complete execution hierarchy for a run
pub async fn build_hierarchy(
    pool: &DatabasePool,
    run_id: &Uuid,
) -> Result<HierarchyResponse, DatabaseError> {
    // Fetch lightweight call headers only — no args/return_value payloads.
    let calls = CallHeader::get_by_run(pool, run_id).await?;

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
fn build_tree(calls: &[CallHeader]) -> Vec<HierarchyNode> {
    // Get root calls (no parent)
    let root_calls: Vec<&CallHeader> = calls
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
fn build_call_node(call: &CallHeader, all_calls: &[CallHeader]) -> HierarchyNode {
    // Get child calls
    let child_calls: Vec<&CallHeader> = all_calls
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
        has_args: call.has_args,
        has_return_value: call.has_return_value,
        flow: slim_flow(&call.flow),
        file: call.file.clone(),
        line: call.line,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slim_flow_keeps_only_metadata_keys() {
        let flow = Some(serde_json::json!({
            "type": "cond",
            "inline": true,
            "branch": "b1",
            "predicate_result": "x".repeat(100_000),
        }));
        let slim = slim_flow(&flow).expect("slim flow");
        let obj = slim.as_object().unwrap();
        assert_eq!(obj.get("type"), Some(&serde_json::json!("cond")));
        assert_eq!(obj.get("inline"), Some(&serde_json::json!(true)));
        assert_eq!(obj.get("branch"), Some(&serde_json::json!("b1")));
        assert!(!obj.contains_key("predicate_result"));
    }

    #[test]
    fn test_slim_flow_drops_blob_ref_maps() {
        // A spilled flow is a BlobRef typed map with none of the metadata
        // keys — slimming must not leak it into the tree.
        let flow = Some(serde_json::json!({
            "$type": "::hot::blob/BlobRef",
            "$val": {"id": "abc", "size": 12345},
        }));
        assert_eq!(slim_flow(&flow), None);
    }

    #[test]
    fn test_slim_flow_none() {
        assert_eq!(slim_flow(&None), None);
    }
}
