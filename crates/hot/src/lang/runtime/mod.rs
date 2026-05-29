//! Hot's bytecode execution layer — everything that runs *after* compilation.
//!
//! * [`vm`]            — the register-based bytecode interpreter.
//! * [`jit`]           — Cranelift-backed JIT for hot functions.
//! * [`error`]         — runtime error type raised by the VM/JIT.
//! * [`limits`]        — VM resource limits (call depth, allocation, etc.).
//! * [`oom_logger`]    — out-of-memory and stack-overflow diagnostic logger.
//! * [`function_ref`]  — runtime function-reference value type.
//! * [`resolution`]    — runtime name resolution (vs. `crate::lang::compiler::resolver`,
//!   which is the compile-time resolver).

pub mod error;
pub mod function_ref;
pub mod hof_fusion;
pub mod jit;
pub mod limits;
pub mod oom_logger;
pub mod resolution;
pub mod vm;
