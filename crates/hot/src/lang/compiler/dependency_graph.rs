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
use ahash::AHashSet;
use indexmap::IndexMap;
use std::collections::VecDeque;

// ============================================================================
// Dependency Analysis for Parallel Execution
// ============================================================================
// These functions analyze dependencies between variables in a flow to enable
// parallel execution with proper dependency resolution.

/// Result type for dependency graph: (graph, var_sources)
/// - graph: Map from variable name to set of dependencies
/// - var_sources: Map from variable name to source location (for error reporting)
type DependencyGraphResult = (IndexMap<String, AHashSet<String>>, IndexMap<String, ()>);

impl Compiler {
    /// Extract all variable dependencies from a value expression
    /// Returns a set of variable names that this value depends on
    fn extract_dependencies(&self, value: &Value) -> AHashSet<String> {
        let mut deps = AHashSet::new();
        self.extract_dependencies_recursive(value, &mut deps);
        deps
    }

    /// Recursively extract dependencies from a value expression
    #[allow(clippy::only_used_in_recursion)]
    fn extract_dependencies_recursive(&self, value: &Value, deps: &mut AHashSet<String>) {
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
        let mut graph: IndexMap<String, AHashSet<String>> = IndexMap::new();
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

    /// Perform topological sort on a dependency graph using Kahn's algorithm
    /// Returns the execution order or an error if a cycle is detected
    pub fn topological_sort(
        &self,
        graph: &IndexMap<String, AHashSet<String>>,
    ) -> Result<Vec<String>, String> {
        let mut in_degree: IndexMap<String, usize> = IndexMap::new();
        let mut adj_list: IndexMap<String, Vec<String>> = IndexMap::new();

        // Initialize in-degree and adjacency list
        for (node, deps) in graph {
            in_degree.entry(node.clone()).or_insert(0);
            adj_list.entry(node.clone()).or_default();

            for dep in deps {
                in_degree.entry(dep.clone()).or_insert(0);
                adj_list.entry(dep.clone()).or_default();
                adj_list.get_mut(dep).unwrap().push(node.clone());
                *in_degree.get_mut(node).unwrap() += 1;
            }
        }

        // Kahn's algorithm with FIFO queue
        let mut queue = VecDeque::new();
        for (node, degree) in &in_degree {
            if *degree == 0 {
                queue.push_back(node.clone());
            }
        }

        let mut result = Vec::new();
        while let Some(node) = queue.pop_front() {
            result.push(node.clone());

            if let Some(neighbors) = adj_list.get(&node) {
                for neighbor in neighbors {
                    if let Some(degree) = in_degree.get_mut(neighbor) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(neighbor.clone());
                        }
                    }
                }
            }
        }

        // Check for circular dependencies
        if result.len() != in_degree.len() {
            let remaining: Vec<String> = in_degree
                .iter()
                .filter(|(_, degree)| **degree > 0)
                .map(|(name, _)| name.clone())
                .collect();

            return Err(format!(
                "Circular dependency detected in parallel flow. Variables involved: {}",
                remaining.join(", ")
            ));
        }

        Ok(result)
    }

    /// Group variables by dependency level
    /// Variables in the same level have no dependencies on each other and can execute in parallel
    pub fn group_by_dependency_levels(
        &self,
        graph: &IndexMap<String, AHashSet<String>>,
        execution_order: &[String],
    ) -> Vec<Vec<String>> {
        let mut dependency_levels: Vec<Vec<String>> = Vec::new();
        let mut processed = AHashSet::new();

        // Keep processing until all variables are grouped
        while processed.len() < execution_order.len() {
            let mut current_level = Vec::new();

            // Find all variables whose dependencies are already processed
            for var_name in execution_order {
                if processed.contains(var_name) {
                    continue;
                }

                let empty_set = AHashSet::new();
                let dependencies = graph.get(var_name).unwrap_or(&empty_set);

                // Check if all dependencies are already processed
                let all_deps_processed = dependencies.iter().all(|dep| processed.contains(dep));

                if all_deps_processed {
                    current_level.push(var_name.clone());
                }
            }

            // Mark all variables in this level as processed
            for var in &current_level {
                processed.insert(var.clone());
            }

            if !current_level.is_empty() {
                dependency_levels.push(current_level);
            } else {
                // No progress made - this shouldn't happen with proper topological sort
                break;
            }
        }

        dependency_levels
    }
}
