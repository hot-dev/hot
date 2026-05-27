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

pub mod ast;
pub mod bytecode;
pub mod cache;
pub mod compiler;
pub mod display;
pub mod emitter;
pub mod engine;
pub mod errors;
pub mod event;
pub mod fmt;
pub mod hot;
pub mod json_schema;
pub mod lexer;
pub mod parser;
pub mod project;
pub mod refs;
pub mod repl;
pub mod runtime;
pub mod user_code;

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
