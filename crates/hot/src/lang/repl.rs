// Stateful REPL session for Hot.
//
// This module provides a stateful REPL session that preserves variables across inputs
// using incremental execution: compiles all code but executes only new instructions.

use crate::lang::engine::Engine;
use crate::val::Val;
use ahash::AHashMap;
use indexmap::IndexMap;

/// Configuration for the REPL compiler
#[derive(Debug, Clone, Default)]
pub struct ReplConfig {
    /// Source paths for dependency resolution
    pub src_paths: Vec<String>,
    /// Test paths for dependency resolution
    pub test_paths: Vec<String>,
    /// Project configuration
    pub conf: Option<Val>,
    /// Project name for dependency resolution
    pub project_name: Option<String>,
    /// Context storage (from ctx.hot) for context variables
    pub context_storage: Option<AHashMap<String, Val>>,
}

/// A stateful REPL session that preserves variables across inputs
pub struct ReplSession {
    /// Accumulated source from all REPL inputs
    /// This is needed for compilation (so the compiler can resolve variable references)
    /// All code is compiled together but only new instructions are executed
    accumulated_source: Vec<String>,

    /// Current namespace for REPL execution (default: ::hot::dev)
    current_namespace: String,

    /// Preserved variable state extracted from VM after each execution
    /// Maps namespace -> (variable_name -> value)
    /// This state is injected into the VM before execution to provide context for new code
    preserved_state: IndexMap<String, IndexMap<String, Val>>,

    /// Number of bytecode instructions already executed
    /// This marks the boundary between previously executed code and new input.
    executed_instruction_count: usize,

    /// Configuration for compilation
    config: ReplConfig,

    /// Input counter for debugging
    input_counter: usize,
}

impl ReplSession {
    /// Create a new REPL session with the given configuration
    pub fn new(config: ReplConfig) -> Self {
        Self {
            accumulated_source: Vec::new(),
            current_namespace: "::hot::dev".to_string(),
            preserved_state: IndexMap::new(),
            executed_instruction_count: 0,
            config,
            input_counter: 0,
        }
    }

    /// Create a new REPL session with default configuration
    pub fn new_default() -> Self {
        Self::new(ReplConfig::default())
    }

    /// Evaluate a REPL input and return the result
    /// Variables and functions are preserved across inputs
    pub fn eval(&mut self, input: &str) -> Result<Val, String> {
        // Increment input counter for debugging
        self.input_counter += 1;

        // Check for namespace directive
        if let Some(new_namespace) = self.parse_namespace_directive(input) {
            self.current_namespace = new_namespace;
            return Ok(Val::Null);
        }

        // Save state for error recovery
        let saved_source = self.accumulated_source.clone();
        let saved_state = self.preserved_state.clone();
        let saved_namespace = self.current_namespace.clone();
        let saved_instruction_count = self.executed_instruction_count;

        // Try to execute the input
        match self.try_eval_input(input) {
            Ok(result) => Ok(result),
            Err(e) => {
                // Rollback on error to maintain consistent state
                self.accumulated_source = saved_source;
                self.preserved_state = saved_state;
                self.current_namespace = saved_namespace;
                self.executed_instruction_count = saved_instruction_count;
                Err(e)
            }
        }
    }

    /// Try to evaluate an input (may fail)
    fn try_eval_input(&mut self, input: &str) -> Result<Val, String> {
        // Determine if this is a declaration or a standalone expression
        // Standalone expressions (like `a` or `add(1, 2)`) shouldn't be accumulated
        // because they're not valid at namespace level
        let is_declaration = self.is_declaration(input);

        let combined_source = if is_declaration {
            // This is a declaration - accumulate it
            self.accumulated_source.push(input.to_string());
            self.build_combined_source()
        } else {
            // This is a standalone expression - create temporary source with dummy variable
            // This allows us to evaluate the expression without polluting the namespace
            let mut temp_source = self.build_combined_source();
            temp_source.push_str("// Temporary expression evaluation\n");
            temp_source.push_str("__repl_tmp_result ");
            temp_source.push_str(input);
            temp_source.push('\n');
            temp_source
        };

        tracing::debug!(
            "REPL: Executing input meta {}, accumulated {} inputs, executed {} instructions, is_declaration: {}",
            self.input_counter + 1,
            self.accumulated_source.len(),
            self.executed_instruction_count,
            is_declaration
        );
        tracing::debug!(
            "REPL: Preserved state has {} namespaces",
            self.preserved_state.len()
        );

        // Execute incrementally: compile all code but execute only new instructions
        // This is the key to preventing side-effect re-execution:
        // 1. Compile all accumulated code (for symbol resolution)
        // 2. Inject preserved state (provides context for new code)
        // 3. Restore current_namespace (so VM knows which namespace we're in)
        // 4. Execute ONLY instructions from executed_instruction_count onward
        // 5. Previously executed code is skipped to prevent duplicate side effects.
        let preserved_namespace = if self.executed_instruction_count > 0 {
            Some(self.current_namespace.as_str())
        } else {
            None
        };

        let result = Engine::execute_incremental(
            &combined_source,
            &self.config.src_paths,
            &self.config.test_paths,
            self.config.conf.as_ref(),
            self.config.project_name.as_deref(),
            &self.preserved_state,
            self.executed_instruction_count,
            preserved_namespace,
            self.config.context_storage.clone(),
            true, // REPL is interactive — use color
        )?;

        tracing::debug!(
            "REPL: Execution complete, new state has {} namespaces, {} total instructions, namespace: '{}'",
            result.new_state.len(),
            result.total_instruction_count,
            result.current_namespace
        );

        // Update state for next execution (only if this was a declaration)
        if is_declaration {
            self.preserved_state = result.new_state;
            self.executed_instruction_count = result.total_instruction_count;
            self.current_namespace = result.current_namespace;
        }
        // For expressions, we don't update state since we used a temporary variable

        Ok(result.result)
    }

    /// Check if input is a declaration (vs a standalone expression)
    /// Declarations have the form: `name value` or `name fn(...) {...}` etc.
    /// Expressions are standalone like `a` or `add(1, 2)`
    fn is_declaration(&self, input: &str) -> bool {
        let trimmed = input.trim();

        // Empty input is not a declaration
        if trimmed.is_empty() {
            return false;
        }

        // Check for declaration keywords
        if trimmed.contains(" fn ") || trimmed.contains(" type ") || trimmed.contains(" -> ") {
            return true;
        }

        // Simple heuristic: if it has whitespace and doesn't start with special chars,
        // it's likely a declaration
        // E.g., "a 1", "b a", "x add(1, 2)" are declarations
        // E.g., "a", "add(1, 2)" are expressions
        if trimmed.contains(char::is_whitespace) {
            // Has whitespace - likely a declaration unless it starts with a function call
            !trimmed.starts_with('(') && !trimmed.starts_with('[') && !trimmed.starts_with('{')
        } else {
            // No whitespace - it's a standalone expression
            false
        }
    }

    /// Build source with all accumulated inputs
    fn build_combined_source(&self) -> String {
        let mut source = format!("{} ns\n\n", self.current_namespace);

        for (i, input) in self.accumulated_source.iter().enumerate() {
            source.push_str(&format!("// REPL input {}\n", i + 1));
            source.push_str(input);
            source.push('\n');
        }

        source
    }

    /// Parse namespace directive from input
    /// Returns Some(namespace) if input is a namespace directive, None otherwise
    /// Syntax: ::path [#metadata] ns
    fn parse_namespace_directive(&self, input: &str) -> Option<String> {
        let trimmed = input.trim();

        // Must end with ` ns`
        if !trimmed.ends_with(" ns") {
            return None;
        }

        // Strip the ` ns` suffix
        let before_ns = trimmed.strip_suffix(" ns")?.trim();

        // If there's metadata (#...), we need to find the namespace path before it
        // The namespace path starts with :: and ends before any # or whitespace
        let namespace = if before_ns.contains('#') {
            // Find the namespace path (everything before the first #)
            let ns_end = before_ns.find('#')?;
            before_ns[..ns_end].trim()
        } else {
            before_ns
        };

        // Must start with ::
        if namespace.starts_with("::") {
            Some(namespace.to_string())
        } else {
            None
        }
    }

    /// Get the current namespace
    pub fn current_namespace(&self) -> &str {
        &self.current_namespace
    }

    /// Get the number of inputs executed so far
    pub fn input_count(&self) -> usize {
        self.input_counter
    }

    /// Reset the REPL session (clear all state)
    pub fn reset(&mut self) {
        self.accumulated_source.clear();
        self.preserved_state.clear();
        self.executed_instruction_count = 0;
        self.current_namespace = "::hot::dev".to_string();
        self.input_counter = 0;
    }

    /// Get the accumulated source (for debugging)
    pub fn get_accumulated_source(&self) -> &[String] {
        &self.accumulated_source
    }

    /// Get the executed instruction count (for debugging)
    pub fn get_executed_instruction_count(&self) -> usize {
        self.executed_instruction_count
    }

    /// Get the preserved variable state (for debugging)
    pub fn get_preserved_state(&self) -> &IndexMap<String, IndexMap<String, Val>> {
        &self.preserved_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repl_basic_variable() {
        let mut repl = ReplSession::new_default();

        // Define a variable
        let result = repl.eval("x 42").unwrap();
        assert_eq!(result, Val::Int(42));

        // Use the variable in another assignment
        let result = repl.eval("y x").unwrap();
        assert_eq!(result, Val::Int(42));
    }

    #[test]
    fn test_repl_namespace_switch() {
        let mut repl = ReplSession::new_default();

        // Start in ::hot::dev
        assert_eq!(repl.current_namespace(), "::hot::dev");

        // Switch namespace
        repl.eval("::my::code ns").unwrap();
        assert_eq!(repl.current_namespace(), "::my::code");

        // Variables should be in the new namespace
        let result = repl.eval("x 100").unwrap();
        assert_eq!(result, Val::Int(100));
    }

    #[test]
    fn test_repl_error_recovery() {
        let mut repl = ReplSession::new_default();

        // Define a valid variable
        repl.eval("x 42").unwrap();
        assert_eq!(repl.input_count(), 1);

        // Try invalid syntax (should fail but not corrupt state)
        let result = repl.eval("invalid syntax here");
        assert!(result.is_err());

        // Input count should have increased (error happened after counter increment)
        // But state should be rolled back
        assert_eq!(repl.input_count(), 2);

        // Previous variable should still be accessible
        let result = repl.eval("y x").unwrap();
        assert_eq!(result, Val::Int(42));
        assert_eq!(repl.input_count(), 3);
    }

    #[test]
    fn test_repl_reset() {
        let mut repl = ReplSession::new_default();

        // Define variables
        repl.eval("x 42").unwrap();
        repl.eval("y 100").unwrap();
        assert_eq!(repl.input_count(), 2);

        // Reset
        repl.reset();
        assert_eq!(repl.input_count(), 0);
        assert_eq!(repl.current_namespace(), "::hot::dev");

        // Previous variables should not exist
        // (this will fail because x is undefined)
        let result = repl.eval("z x");
        assert!(result.is_err());
    }
}
