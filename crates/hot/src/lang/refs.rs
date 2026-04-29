// Boxed reference values used by the Hot runtime.

use crate::val::{Val, ValBox};
use indexmap::IndexMap;
use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A namespace reference that can be stored in a Val
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NamespaceRef {
    /// Fully qualified namespace path (e.g., "::hot::test")
    pub path: String,
    /// Optional metadata about the namespace
    pub metadata: Option<String>,
}

impl NamespaceRef {
    /// Create a new namespace reference
    pub fn new(path: String) -> Self {
        Self {
            path,
            metadata: None,
        }
    }

    /// Create a new namespace reference with metadata
    pub fn with_metadata(path: String, metadata: String) -> Self {
        Self {
            path,
            metadata: Some(metadata),
        }
    }

    /// Get the namespace path
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the metadata if available
    pub fn metadata(&self) -> Option<&str> {
        self.metadata.as_deref()
    }
}

impl ValBox for NamespaceRef {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn clone_box(&self) -> Box<dyn ValBox> {
        Box::new(self.clone())
    }

    fn equals(&self, other: &dyn ValBox) -> bool {
        if let Some(other_ns) = other.as_any().downcast_ref::<NamespaceRef>() {
            self == other_ns
        } else {
            false
        }
    }

    fn hash(&self, mut state: &mut dyn Hasher) {
        use std::hash::Hash;
        Hash::hash(self, &mut state);
    }

    fn to_string(&self) -> String {
        self.path.clone()
    }

    fn compare(&self, other: &dyn ValBox) -> Option<Ordering> {
        if let Some(other_ns) = other.as_any().downcast_ref::<NamespaceRef>() {
            self.path.partial_cmp(&other_ns.path)
        } else {
            None
        }
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "type": "Namespace",
            "path": self.path,
            "metadata": self.metadata
        }))
    }

    fn type_name(&self) -> &'static str {
        "Namespace"
    }
}

impl std::fmt::Display for NamespaceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)
    }
}

/// Helper function to create a namespace reference Val
pub fn namespace_ref(path: String) -> Val {
    Val::Box(Box::new(NamespaceRef::new(path)))
}

/// Helper function to create a namespace reference Val with metadata
pub fn namespace_ref_with_metadata(path: String, metadata: String) -> Val {
    Val::Box(Box::new(NamespaceRef::with_metadata(path, metadata)))
}

/// A type reference for variant union types that can be stored in a Val
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TypeRef {
    /// Fully qualified type name (e.g., "::my-app/Shape")
    pub name: String,
    /// Variant definitions: name -> optional referenced type name
    /// None for unit variants, Some(type_name) for type-reference variants
    pub variants: IndexMap<String, Option<String>>,
}

impl std::hash::Hash for TypeRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.name, state);
        // Hash variant count and each variant for deterministic hashing
        std::hash::Hash::hash(&self.variants.len(), state);
        for (k, v) in &self.variants {
            std::hash::Hash::hash(k, state);
            std::hash::Hash::hash(v, state);
        }
    }
}

impl PartialOrd for TypeRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TypeRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Compare by name first, then by variant count
        self.name
            .cmp(&other.name)
            .then_with(|| self.variants.len().cmp(&other.variants.len()))
    }
}

impl TypeRef {
    /// Create a new type reference with variants
    pub fn new(name: String, variants: IndexMap<String, Option<String>>) -> Self {
        Self { name, variants }
    }

    /// Get the type name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get variant by name
    pub fn get_variant(&self, name: &str) -> Option<&Option<String>> {
        self.variants.get(name)
    }

    /// Check if a variant exists
    pub fn has_variant(&self, name: &str) -> bool {
        self.variants.contains_key(name)
    }

    /// Get all variant names
    pub fn variant_names(&self) -> impl Iterator<Item = &String> {
        self.variants.keys()
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// Helper function to create a type reference Val
pub fn type_ref(name: String, variants: IndexMap<String, Option<String>>) -> Val {
    Val::Box(Box::new(TypeRef::new(name, variants)))
}

/// Helper function to extract a type reference from a Val
pub fn extract_type_ref(val: &Val) -> Option<&TypeRef> {
    match val {
        Val::Box(boxed) => boxed.as_any().downcast_ref::<TypeRef>(),
        _ => None,
    }
}
