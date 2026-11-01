//! Variable dependency analysis used by parallel-flow scheduling.
//!
//! Builds a per-flow dependency graph (`build_dependency_graph`),
//! orders it via Kahn's algorithm (`topological_sort`), and groups
//! mutually-independent variables into parallel-executable batches
//! (`group_by_dependency_levels`).
//!
//! Lives next to (rather than inside) `mod.rs` because it is a
//! self-contained analysis with no other compiler entanglements
//! beyond the AST it walks.

use super::Compiler;
use crate::lang::ast::{Flow, FnCall, Ref, Value};
use crate::val::Val;
use ahash::AHashMap;
use indexmap::{IndexMap, IndexSet};
use std::collections::VecDeque;

// ============================================================================
// Dependency Analysis for Parallel Execution
// ============================================================================
// These functions analyze dependencies between variables in a flow to enable
// parallel execution with proper dependency resolution.

/// Result type for dependency graph: (graph, var_sources)
/// - graph: Map from variable name to set of dependencies, in source order
///   (both the bindings and each one's dependencies, so every order derived
///   from it is the same on every compile)
/// - var_sources: Map from variable name to source location (for error reporting)
type DependencyGraphResult = (IndexMap<String, IndexSet<String>>, IndexMap<String, ()>);

impl Compiler {
    /// Extract all variable dependencies from a value expression
    /// Returns a set of variable names that this value depends on
    fn extract_dependencies(&self, value: &Value) -> IndexSet<String> {
        let mut deps = IndexSet::new();
        self.extract_dependencies_recursive(value, &mut deps);
        deps
    }

    /// Recursively extract dependencies from a value expression
    #[allow(clippy::only_used_in_recursion)]
    fn extract_dependencies_recursive(&self, value: &Value, deps: &mut IndexSet<String>) {
        match value {
            Value::Val(val, _) => {
                // Check if Val contains boxed expressions that might have dependencies
                if let Val::Box(boxed) = val
                    && let Some(fn_call) = boxed.as_any().downcast_ref::<FnCall>()
                {
                    self.extract_dependencies_recursive(&Value::FnCall(fn_call.clone()), deps);
                }
            }
            Value::Ref(reference) => match reference {
                Ref::Var(var_ref) => {
                    // All Ref::Var are local variable references
                    // (namespaced references use Ref::Ns)
                    deps.insert(var_ref.var.sym.to_string());
                }
                Ref::Ns(_) => {
                    // Namespaced references are not local dependencies
                }
            },
            Value::FnCall(fn_call) => {
                // Check if function itself is a dependency (e.g., calling a variable that holds a function)
                self.extract_dependencies_recursive(fn_call.function.as_ref(), deps);

                // Extract dependencies from arguments
                for arg in &fn_call.args {
                    self.extract_dependencies_recursive(&arg.value, deps);
                }
            }
            Value::Flow(flow) => {
                // Extract dependencies from flow expressions
                for expr in &flow.expressions {
                    self.extract_dependencies_recursive(expr, deps);
                }
            }
            Value::Fn(fn_defs) => {
                // Extract dependencies from function bodies
                for fn_def in fn_defs {
                    self.extract_dependencies_recursive(&fn_def.body, deps);
                }
            }
            Value::TypeDef(_) | Value::TypeImplementation(_) | Value::Unbound(_) => {}
            Value::Cond(_, condition, result_flow) => {
                self.extract_dependencies_recursive(condition, deps);
                for expr in &result_flow.expressions {
                    self.extract_dependencies_recursive(expr, deps);
                }
            }
            Value::CondDefault(flow) => {
                for expr in &flow.expressions {
                    self.extract_dependencies_recursive(expr, deps);
                }
            }
            Value::Match(match_expr) => {
                self.extract_dependencies_recursive(&match_expr.value, deps);
                for arm in &match_expr.arms {
                    self.extract_dependencies_recursive(&arm.body, deps);
                }
            }
            Value::TemplateLiteral(template) => {
                for part in &template.parts {
                    if let crate::lang::ast::TemplatePart::Expression(expr) = part {
                        self.extract_dependencies_recursive(expr.as_ref(), deps);
                    }
                }
            }
            Value::Raw(inner) | Value::Do(inner) => {
                self.extract_dependencies_recursive(inner, deps);
            }
            Value::VariadicExpansion(identifier) => {
                deps.insert(identifier.clone());
            }
            Value::MultipleValues(values) => {
                for value in values {
                    self.extract_dependencies_recursive(value, deps);
                }
            }
            Value::Lambda(lambda) => {
                // Extract dependencies from lambda body
                self.extract_dependencies_recursive(&lambda.body, deps);
            }
            Value::MatchArm(arm) => {
                // Extract dependencies from match arm body
                self.extract_dependencies_recursive(&arm.body, deps);
            }
            Value::MapWithSpread { spread_entries, .. } => {
                for (_, spread_val) in spread_entries {
                    self.extract_dependencies_recursive(spread_val, deps);
                }
            }
            Value::Placeholder(_) => {}
        }
    }

    /// Build a dependency graph for variables in a flow
    /// Returns (dependency_graph, var_sources) where:
    /// - dependency_graph maps each variable to its set of dependencies
    /// - var_sources maps each variable to its source location (for error reporting)
    pub fn build_dependency_graph(&self, flow: &Flow) -> Result<DependencyGraphResult, String> {
        let mut graph: IndexMap<String, IndexSet<String>> = IndexMap::new();
        let var_sources: IndexMap<String, ()> = IndexMap::new();

        // Parse flow expressions to find variable assignments
        // Format: var_name value_expression
        let mut i = 0;
        while i < flow.expressions.len() {
            // Check for variable assignment pattern
            if i + 1 < flow.expressions.len()
                && let Value::Ref(Ref::Var(var_ref)) = &flow.expressions[i]
            {
                let var_name = var_ref.var.sym.to_string();
                let value_expr = &flow.expressions[i + 1];

                // Extract dependencies from the value expression
                let dependencies = self.extract_dependencies(value_expr);

                graph.insert(var_name, dependencies);
                i += 2;
                continue;
            }
            i += 1;
        }

        Ok((graph, var_sources))
    }

    /// Perform topological sort on a dependency graph using Kahn's algorithm.
    /// Returns the flow's bindings in execution order (names from outside
    /// the flow are ready from the start, so they aren't in it), or an
    /// error if a cycle is detected. Ties keep source order.
    pub fn topological_sort(
        &self,
        graph: &IndexMap<String, IndexSet<String>>,
    ) -> Result<Vec<String>, String> {
        let mut in_degree: IndexMap<&str, usize> =
            graph.keys().map(|node| (node.as_str(), 0)).collect();
        let mut adj_list: AHashMap<&str, Vec<&str>> = AHashMap::new();
        for (node, deps) in graph {
            for dep in deps.iter().filter(|dep| graph.contains_key(*dep)) {
                adj_list
                    .entry(dep.as_str())
                    .or_default()
                    .push(node.as_str());
                in_degree[node.as_str()] += 1;
            }
        }

        // Kahn's algorithm with FIFO queue
        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(node, _)| *node)
            .collect();

        let mut result = Vec::with_capacity(graph.len());
        while let Some(node) = queue.pop_front() {
            result.push(node.to_string());
            for neighbor in adj_list.get(node).into_iter().flatten() {
                let degree = &mut in_degree[*neighbor];
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(neighbor);
                }
            }
        }

        // Check for circular dependencies
        if result.len() != in_degree.len() {
            let remaining: Vec<&str> = in_degree
                .iter()
                .filter(|(_, degree)| **degree > 0)
                .map(|(name, _)| *name)
                .collect();

            return Err(format!(
                "Circular dependency detected in parallel flow. Variables involved: {}",
                remaining.join(", ")
            ));
        }

        Ok(result)
    }

    /// Group the flow's bindings by dependency level. A binding's level is
    /// one past the deepest binding of the flow it reads (names from outside
    /// the flow don't count), so the bindings of a level can run in
    /// parallel. Each level lists its bindings in source order.
    pub fn group_by_dependency_levels(
        &self,
        graph: &IndexMap<String, IndexSet<String>>,
        execution_order: &[String],
    ) -> Vec<Vec<String>> {
        let mut level_of: AHashMap<&str, usize> = AHashMap::with_capacity(execution_order.len());
        for var_name in execution_order {
            let level = graph
                .get(var_name)
                .into_iter()
                .flatten()
                .filter_map(|dep| level_of.get(dep.as_str()))
                .map(|level| level + 1)
                .max()
                .unwrap_or(0);
            level_of.insert(var_name.as_str(), level);
        }

        let level_count = level_of.values().max().map_or(0, |max| max + 1);
        let mut dependency_levels: Vec<Vec<String>> = vec![Vec::new(); level_count];
        for var_name in graph.keys() {
            if let Some(&level) = level_of.get(var_name.as_str()) {
                dependency_levels[level].push(var_name.clone());
            }
        }
        dependency_levels
    }
}
