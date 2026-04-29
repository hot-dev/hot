// Function Reference Type for Bytecode Engine
//
// This module provides a proper function reference type that can be stored
// in Val::Box, representing function references more efficiently than strings.

use crate::val::Val;
use std::fmt;

/// A function reference that can be stored in a Val
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FunctionRef {
    /// Fully qualified function name (e.g., "::hot::math/add")
    pub name: String,
    /// Optional arity for validation
    pub arity: Option<u8>,
}

impl FunctionRef {
    /// Create a new function reference
    pub fn new(name: String) -> Self {
        Self { name, arity: None }
    }

    /// Create a new function reference with arity
    pub fn with_arity(name: String, arity: u8) -> Self {
        Self {
            name,
            arity: Some(arity),
        }
    }

    /// Get the function name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the arity if available
    pub fn arity(&self) -> Option<u8> {
        self.arity
    }
}

impl fmt::Display for FunctionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// Helper function to create a function reference Val
pub fn function_ref(name: String) -> Val {
    Val::Box(Box::new(FunctionRef::new(name)))
}

/// Helper function to create a function reference Val with arity
pub fn function_ref_with_arity(name: String, arity: u8) -> Val {
    Val::Box(Box::new(FunctionRef::with_arity(name, arity)))
}

/// Helper function to extract a function reference from a Val
pub fn extract_function_ref(val: &Val) -> Option<&FunctionRef> {
    match val {
        Val::Box(boxed) => boxed.as_any().downcast_ref::<FunctionRef>(),
        _ => None,
    }
}
