// Hot Language - High-Performance Bytecode Implementation
//
// This module contains a complete reimplementation of the Hot language
// focusing on maximum performance through bytecode compilation:
//
// 1. Register-based bytecode instruction set optimized for Hot features
// 2. High-performance virtual machine with minimal overhead
// 3. Single-pass bytecode compiler with strict compile-time type checking
// 4. Efficient memory management and register allocation
// 5. Native function integration with zero-copy semantics
// 6. Advanced optimization passes for performance-critical code

pub use hot_lang::lang::ast;
pub use hot_lang::lang::bytecode;
pub mod cache;
pub use hot_lang::lang::compiler;
pub use hot_lang::lang::display;
pub mod emitter;
pub mod engine;
pub use hot_lang::lang::errors;
pub mod event;
pub use hot_lang::lang::fmt;
pub mod hot;
pub use hot_lang::lang::json_schema;
pub use hot_lang::lang::lexer;
pub use hot_lang::lang::parser;
pub use hot_lang::lang::project;
pub use hot_lang::lang::refs;
pub mod repl;
pub mod runtime;
pub mod user_code;

#[cfg(test)]
mod compiler_vm_tests;

// Re-export key components
pub use bytecode::{BytecodeProgram, Constant, Instruction};
pub use compiler::Compiler;
pub use engine::Engine;
pub use runtime::vm::VirtualMachine;

/// Version identifier for the implementation
/// Used for cache validation to ensure compatibility
pub const VERSION: &str = "2.1.0-bytecode";

/// Cache format version - increment when BytecodeProgram serialization format changes
/// Version 4: Added pre-built HotAst with variable index for fast cached execution
pub const CACHE_FORMAT_VERSION: u32 = 6;

/// Feature flags for experimental functionality
pub struct FeatureFlags {
    pub incremental_compilation: bool,
    pub jit_compilation: bool,
    pub parallel_execution: bool,
    pub memory_optimization: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            incremental_compilation: true,
            jit_compilation: false, // Enabled by default on supported platforms via hot.jit.mode conf; disable with --jit disabled
            parallel_execution: false, // Future
            memory_optimization: true,
        }
    }
}
