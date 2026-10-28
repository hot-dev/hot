//! Runtime value types the compiler emits and `Val` can hold.
//!
//! * [`function_ref`] — function-reference value type.
//! * [`resolution`]   — name resolution shared by the compiler and the VM
//!   (vs. `crate::lang::compiler::resolver`, the compile-time resolver).

pub mod function_ref;
pub mod resolution;
