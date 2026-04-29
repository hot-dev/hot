// AST types for the Hot language.

use crate::val;
use crate::val::Val;
use crate::val::ValBox;
use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;
use std::any::Any;
use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use std::hash::Hasher;

// Source tracking for parsed syntax.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub file: Option<String>,
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub length: usize,
}

impl Source {
    pub fn new(
        file: Option<String>,
        line: usize,
        column: usize,
        position: usize,
        length: usize,
    ) -> Self {
        Self {
            file,
            line,
            column,
            position,
            length,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}:{}:{}", file, self.line, self.column)
        } else {
            write!(f, "{}:{}", self.line, self.column)
        }
    }
}

// Symbol type used throughout parsed syntax.
#[derive(
    Debug, Clone, PartialEq, Hash, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Sym {
    String(String),
}

impl From<&str> for Sym {
    fn from(s: &str) -> Self {
        Sym::String(s.to_string())
    }
}

impl From<String> for Sym {
    fn from(s: String) -> Self {
        Sym::String(s)
    }
}

impl Sym {
    pub fn name(&self) -> &str {
        match self {
            Sym::String(s) => s,
        }
    }
}

impl Display for Sym {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

// Namespace path types.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum NsPathPart {
    Sym(Sym),
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct NsPath(pub Vec<NsPathPart>);

impl Default for NsPath {
    fn default() -> Self {
        Self::new()
    }
}

impl NsPath {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn from_vec(parts: Vec<NsPathPart>) -> Self {
        Self(parts)
    }

    pub fn from(s: &str) -> Self {
        if s.is_empty() {
            return Self::new();
        }

        let parts = s
            .trim_start_matches("::")
            .split("::")
            .filter(|part| !part.is_empty())
            .map(|part| NsPathPart::Sym(Sym::from(part)))
            .collect();

        Self::from_vec(parts)
    }

    pub fn push(&mut self, part: NsPathPart) {
        self.0.push(part);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn parts(&self) -> &[NsPathPart] {
        &self.0
    }

    pub fn from_string(s: &str) -> Self {
        Self::from(s)
    }

    /// Render this path as `foo::bar` (no leading `::`). The `Display` impl
    /// produces the prefixed form `::foo::bar`; this helper exists for the
    /// many call sites (compiler, build cache, runtime mirror) that key
    /// namespaces by the unprefixed form.
    pub fn to_unprefixed_string(&self) -> String {
        self.0
            .iter()
            .map(|part| match part {
                NsPathPart::Sym(Sym::String(s)) => s.clone(),
            })
            .collect::<Vec<_>>()
            .join("::")
    }

    /// The canonical default user namespace (`::hot::main`). Used by the
    /// engine and project loader as the entry point for ad-hoc programs
    /// that don't declare their own namespace.
    pub fn hot_main() -> Self {
        Self::from_vec(vec![
            NsPathPart::Sym(Sym::String("hot".to_string())),
            NsPathPart::Sym(Sym::String("main".to_string())),
        ])
    }
}

impl Display for NsPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "::")?;
        for (i, part) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, "::")?;
            }
            match part {
                NsPathPart::Sym(sym) => write!(f, "{}", sym)?,
            }
        }
        Ok(())
    }
}

// Namespace reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NsRef {
    pub ns: NsPath,
    pub src: Option<Source>,
    pub function_name: Option<String>, // Function name for qualified references like ::hot::run/exit
}

// Deep path for variable access.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DeepPath {
    Index(usize),
    Key(String),
    DynamicIndex(String), // Variable name to be resolved at runtime
    Chain(Box<DeepPath>, Box<DeepPath>),
    Append, // Append to vector: items[] value
}

impl DeepPath {
    pub fn from_vec(parts: Vec<DeepPathPart>) -> Self {
        if parts.is_empty() {
            return DeepPath::Key("".to_string());
        }

        let mut result = match &parts[0] {
            DeepPathPart::Index(i) => DeepPath::Index(*i),
            DeepPathPart::Key(k) => DeepPath::Key(k.clone()),
            DeepPathPart::DynamicIndex(var) => DeepPath::DynamicIndex(var.clone()),
            DeepPathPart::Append => DeepPath::Append,
        };

        for part in parts.iter().skip(1) {
            let next = match part {
                DeepPathPart::Index(i) => DeepPath::Index(*i),
                DeepPathPart::Key(k) => DeepPath::Key(k.clone()),
                DeepPathPart::DynamicIndex(var) => DeepPath::DynamicIndex(var.clone()),
                DeepPathPart::Append => DeepPath::Append,
            };
            result = DeepPath::Chain(Box::new(result), Box::new(next));
        }

        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DeepPathPart {
    Index(usize),
    Key(String),
    DynamicIndex(String), // Variable name to be resolved at runtime
    Append,               // Append to vector: items[] value
}

// Literal types for enum-style unions (e.g., "apple" | "banana" | "orange")
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LiteralType {
    Str(String),
    Int(i64),
    Dec(String), // Store as string to preserve precision
    Bool(bool),
    Null,
}

impl std::fmt::Display for LiteralType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiteralType::Str(s) => write!(f, "\"{}\"", s),
            LiteralType::Int(n) => write!(f, "{}", n),
            LiteralType::Dec(d) => write!(f, "{}", d),
            LiteralType::Bool(b) => write!(f, "{}", b),
            LiteralType::Null => write!(f, "null"),
        }
    }
}

// Type expressions for type checking
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TypeExpr {
    Simple(String),
    Generic(String, Vec<TypeExpr>),
    Union(Vec<TypeExpr>),
    Optional(Box<TypeExpr>),
    Named(String),
    Builtin(BuiltinType),
    Literal(LiteralType), // Literal types for enum-style unions
    Any,
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::Simple(name) => write!(f, "{}", name),
            TypeExpr::Generic(name, params) => {
                let param_strs: Vec<String> = params.iter().map(|t| t.to_string()).collect();
                write!(f, "{}<{}>", name, param_strs.join(", "))
            }
            TypeExpr::Union(types) => {
                let type_strs: Vec<String> = types.iter().map(|t| t.to_string()).collect();
                write!(f, "{}", type_strs.join(" | "))
            }
            TypeExpr::Optional(inner) => write!(f, "{}?", inner),
            TypeExpr::Named(name) => write!(f, "{}", name),
            TypeExpr::Builtin(builtin) => write!(f, "{}", builtin),
            TypeExpr::Literal(lit) => write!(f, "{}", lit),
            TypeExpr::Any => write!(f, "Any"),
        }
    }
}

// Built-in types
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BuiltinType {
    Str,
    Int,
    Dec,
    Bool,
    Vec,
    Map,
    Null,
}

impl std::fmt::Display for BuiltinType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuiltinType::Str => write!(f, "Str"),
            BuiltinType::Int => write!(f, "Int"),
            BuiltinType::Dec => write!(f, "Dec"),
            BuiltinType::Bool => write!(f, "Bool"),
            BuiltinType::Vec => write!(f, "Vec"),
            BuiltinType::Map => write!(f, "Map"),
            BuiltinType::Null => write!(f, "Null"),
        }
    }
}

/// Scope identifier for nested variables
/// Examples:
/// - "::demo::schedule" - namespace level
/// - "::demo::schedule/send-daily-newsletter" - function level
/// - "::demo::schedule/send-daily-newsletter/lambda_0" - lambda level
/// - "::demo::schedule/send-daily-newsletter/flow_0" - flow level
pub type ScopePath = String;

/// Fast O(1) lookup index for AST variables at all nesting levels
/// Maps (scope_path, base_var_name) to Vec of Var entries with their Values
/// Multiple Var entries can exist for:
/// - Same var with different deep paths ($person.name, $person.age)
/// - Same var declared multiple times (overwrites)
/// - Same var with different meta annotations
#[derive(Debug, Clone, Default)]
pub struct AstVarIndex {
    /// Main lookup: (scope_path, var_name) -> Vec<(Var, Value)>
    /// scope_path is hierarchical: "::ns", "::ns/func", "::ns/func/lambda_0"
    lookup: AHashMap<(ScopePath, String), Vec<(Var, Value)>>,

    /// Quick check: does this scope_path exist?
    scopes: AHashSet<ScopePath>,
}

// Type alias for complex serialization type to satisfy clippy
type SerializedVarLookup = Vec<((String, String), Vec<(Var, Value)>)>;

// Custom serialization for AstVarIndex to handle complex keys
impl serde::Serialize for AstVarIndex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("AstVarIndex", 2)?;

        // Convert HashMap to Vec for serialization
        let lookup_vec: SerializedVarLookup = self
            .lookup
            .iter()
            .map(|((scope, var), values)| ((scope.clone(), var.clone()), values.clone()))
            .collect();

        // Convert HashSet to Vec for serialization
        let scopes_vec: Vec<String> = self.scopes.iter().cloned().collect();

        state.serialize_field("lookup", &lookup_vec)?;
        state.serialize_field("scopes", &scopes_vec)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for AstVarIndex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct AstVarIndexHelper {
            lookup: SerializedVarLookup,
            scopes: Vec<String>,
        }

        let helper = AstVarIndexHelper::deserialize(deserializer)?;
        Ok(AstVarIndex {
            lookup: helper.lookup.into_iter().collect(),
            scopes: helper.scopes.into_iter().collect(),
        })
    }
}

impl AstVarIndex {
    /// Build index from Program for fast O(1) variable lookups at all nesting levels
    pub fn build(program: &Program) -> Self {
        let mut lookup = AHashMap::new();
        let mut scopes = AHashSet::new();

        // Index variables at all levels: namespace, function, lambda, flow
        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ToString::to_string(&ns_path);
            scopes.insert(ns_str.clone());

            // Index namespace-level variables
            for (var, value) in &namespace.scope.vars {
                let var_name = var.sym.name().to_string();
                let scope_key = (ns_str.clone(), var_name);

                lookup
                    .entry(scope_key)
                    .or_insert_with(Vec::new)
                    .push((var.clone(), value.clone()));

                // Recursively index variables inside this value's nested scopes
                Self::index_nested_vars(&mut lookup, &mut scopes, &ns_str, var.sym.name(), value);
            }
        }

        Self { lookup, scopes }
    }

    /// Recursively traverse Value AST and index variables in nested scopes
    fn index_nested_vars(
        lookup: &mut AHashMap<(ScopePath, String), Vec<(Var, Value)>>,
        scopes: &mut AHashSet<ScopePath>,
        parent_scope: &str,
        parent_var_name: &str,
        value: &Value,
    ) {
        match value {
            Value::Fn(fn_defs) => {
                // Each function definition creates a new scope
                for (idx, fn_def) in fn_defs.iter().enumerate() {
                    let fn_scope = if fn_defs.len() > 1 {
                        format!("{}/{}/{}", parent_scope, parent_var_name, idx)
                    } else {
                        format!("{}/{}", parent_scope, parent_var_name)
                    };
                    scopes.insert(fn_scope.clone());

                    // Index function arguments as variables in the function scope
                    for arg in &fn_def.args.args {
                        let arg_name = arg.var.sym.name().to_string();
                        let scope_key = (fn_scope.clone(), arg_name);

                        // Function args don't have values in the AST, use Null placeholder
                        lookup
                            .entry(scope_key)
                            .or_default()
                            .push((arg.var.clone(), Value::Val(crate::val::Val::Null, None)));
                    }

                    // Recursively index variables in function body
                    Self::index_value_vars(lookup, scopes, &fn_scope, &fn_def.body);
                }
            }
            Value::Lambda(lambda) => {
                // Lambda creates a new scope
                let lambda_scope = format!("{}/lambda", parent_scope);
                scopes.insert(lambda_scope.clone());

                // Index lambda arguments
                for arg in &lambda.args.args {
                    let arg_name = arg.var.sym.name().to_string();
                    let scope_key = (lambda_scope.clone(), arg_name);

                    lookup
                        .entry(scope_key)
                        .or_default()
                        .push((arg.var.clone(), Value::Val(crate::val::Val::Null, None)));
                }

                // Recursively index variables in lambda body
                Self::index_value_vars(lookup, scopes, &lambda_scope, &lambda.body);
            }
            Value::Flow(flow) => {
                // Flow creates a new scope for each expression
                let flow_scope = format!("{}/flow", parent_scope);
                scopes.insert(flow_scope.clone());

                for (idx, expr) in flow.expressions.iter().enumerate() {
                    let expr_scope = format!("{}/{}", flow_scope, idx);
                    Self::index_value_vars(lookup, scopes, &expr_scope, expr);
                }
            }
            _ => {
                // For other value types, just recurse without creating a new scope
                Self::index_value_vars(lookup, scopes, parent_scope, value);
            }
        }
    }

    /// Helper to traverse Value and extract any embedded variable declarations
    fn index_value_vars(
        lookup: &mut AHashMap<(ScopePath, String), Vec<(Var, Value)>>,
        scopes: &mut AHashSet<ScopePath>,
        scope: &str,
        value: &Value,
    ) {
        match value {
            Value::Fn(_) | Value::Lambda(_) | Value::Flow(_) => {
                // These create their own scopes, handle them specially
                Self::index_nested_vars(lookup, scopes, scope, "_inline", value);
            }
            Value::FnCall(fn_call) => {
                // Recurse into function arguments
                Self::index_value_vars(lookup, scopes, scope, &fn_call.function);
                for arg in &fn_call.args {
                    Self::index_value_vars(lookup, scopes, scope, &arg.value);
                }
            }
            Value::Cond(_label, condition, flow) => {
                Self::index_value_vars(lookup, scopes, scope, condition);
                Self::index_value_vars(lookup, scopes, scope, &Value::Flow(flow.clone()));
            }
            Value::CondDefault(flow) => {
                Self::index_value_vars(lookup, scopes, scope, &Value::Flow(flow.clone()));
            }
            Value::MultipleValues(values) => {
                for v in values {
                    Self::index_value_vars(lookup, scopes, scope, v);
                }
            }
            Value::Raw(inner) | Value::Do(inner) => {
                Self::index_value_vars(lookup, scopes, scope, inner);
            }
            Value::TypeImplementation(type_impl) => {
                // Recurse into type implementation body
                Self::index_value_vars(lookup, scopes, scope, &type_impl.implementation);
            }
            Value::Match(match_expr) => {
                Self::index_value_vars(lookup, scopes, scope, &match_expr.value);
                for arm in &match_expr.arms {
                    Self::index_value_vars(lookup, scopes, scope, &arm.body);
                }
            }
            Value::MatchArm(arm) => {
                Self::index_value_vars(lookup, scopes, scope, &arm.body);
            }
            Value::MapWithSpread { spread_entries, .. } => {
                for (_, spread_val) in spread_entries {
                    Self::index_value_vars(lookup, scopes, scope, spread_val);
                }
            }
            // Leaf nodes - no further traversal needed
            Value::Val(_, _)
            | Value::Ref(_)
            | Value::TypeDef(_)
            | Value::Unbound(_)
            | Value::TemplateLiteral(_)
            | Value::VariadicExpansion(_)
            | Value::Placeholder(_) => {}
        }
    }

    /// Fast O(1) lookup: (scope_path, var_name) -> all matching Var entries with Values
    /// Returns all Var entries for this base name (including deep-path variants)
    pub fn get_var_entries(&self, scope: &str, var_name: &str) -> Option<&[(Var, Value)]> {
        self.lookup
            .get(&(scope.to_string(), var_name.to_string()))
            .map(|v| v.as_slice())
    }

    /// Get the primary Var entry (first one, typically the base var without deep paths)
    pub fn get_primary_var(&self, scope: &str, var_name: &str) -> Option<(&Var, &Value)> {
        self.get_var_entries(scope, var_name)?
            .first()
            .map(|(var, value)| (var, value))
    }

    /// Check if a scope exists in the index
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    /// Get all scopes in the index
    pub fn all_scopes(&self) -> impl Iterator<Item = &str> {
        self.scopes.iter().map(|s| s.as_str())
    }
}

// Hot AST wrapper with fast lookup index
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HotAst {
    pub program: Program,
    pub namespaces: Namespaces,
    /// Fast O(1) variable lookup index
    pub var_index: AstVarIndex,
}

impl Default for HotAst {
    fn default() -> Self {
        Self::new()
    }
}

impl HotAst {
    pub fn new() -> Self {
        let program = Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: NsPath::new(),
        };
        let var_index = AstVarIndex::build(&program);

        Self {
            program,
            namespaces: Namespaces::new(),
            var_index,
        }
    }

    /// Create HotAst from a Program, building the index
    pub fn from_program(program: Program) -> Self {
        let var_index = AstVarIndex::build(&program);
        // Copy namespaces from program into the HotAst wrapper
        let namespaces = Namespaces {
            namespaces: program.namespaces.clone(),
        };
        Self {
            program,
            namespaces,
            var_index,
        }
    }

    /// Fast O(1) lookup of variable metadata
    /// Returns the primary (first) Var and its Value for a given scope
    ///
    /// # Arguments
    /// * `scope` - Hierarchical scope path (e.g., "::demo::schedule" or "::demo::schedule/send-daily-newsletter")
    /// * `var_name` - Variable name (without $ prefix)
    pub fn get_var_metadata(&self, scope: &str, var_name: &str) -> Option<(&Var, &Value)> {
        // O(1) - Direct lookup from index (includes Value)
        self.var_index.get_primary_var(scope, var_name)
    }

    /// Fast O(1) lookup of all variable metadata entries in a scope
    /// Returns all Var entries and their Values (e.g., for deep-path variants, overwrites)
    ///
    /// # Arguments
    /// * `scope` - Hierarchical scope path
    /// * `var_name` - Variable name (without $ prefix)
    pub fn get_all_var_metadata(&self, scope: &str, var_name: &str) -> Option<&[(Var, Value)]> {
        // O(1) - Direct lookup from index (includes Values)
        self.var_index.get_var_entries(scope, var_name)
    }

    /// Check if a scope exists in the AST
    pub fn has_scope(&self, scope: &str) -> bool {
        self.var_index.has_scope(scope)
    }

    /// Get all scopes in the AST (for debugging/introspection)
    pub fn all_scopes(&self) -> Vec<String> {
        self.var_index.all_scopes().map(|s| s.to_string()).collect()
    }

    pub fn with_namespaces_and_core() -> Self {
        Self::new()
    }

    /// Extract rich metadata for a variable value, including function signatures, type info, and docs
    ///
    /// Returns a Val::Map with structured metadata:
    /// - "value_type": "function" | "type" | "var_ref" | "literal"
    /// - "signatures": Vec<Val::Map> with arg names, types, variadic info (for functions)
    /// - "type_fields": Val::Map of field names and types (for types)
    /// - "doc": String documentation (if available in meta)
    /// - "meta": Original metadata (if any)
    pub fn extract_rich_metadata(&self, scope: &str, var_name: &str) -> Option<Val> {
        let (var, value) = self.get_var_metadata(scope, var_name)?;

        let mut metadata = Val::map_empty();

        // Add documentation if available
        if let Some(meta) = &var.meta {
            // Extract "doc" if it's a map with a "doc" key
            if let Val::Map(meta_map) = &meta.val
                && let Some(doc_val) = meta_map.get(&Val::from("doc"))
                && let Val::Str(doc_str) = doc_val
            {
                metadata = metadata.merge(&val!({
                    "doc": Val::from(doc_str.trim()),
                }));
            }
            // Store full metadata
            metadata = metadata.merge(&val!({
                "meta": meta.val.clone(),
            }));
        }

        // Determine value type and extract type-specific metadata
        match value {
            Value::Fn(fn_defs) => {
                // Extract function signatures for all overloads
                let signatures: Vec<Val> = fn_defs
                    .iter()
                    .map(|fn_def| {
                        let args: Vec<Val> = fn_def.args.args.iter().map(|arg| {
                        val!({
                            "name": arg.var.sym.name(),
                            "lazy": arg.lazy,
                            "type": arg.type_annotation.clone().unwrap_or("Any".to_string()),
                        })
                    }).collect();

                        val!({
                            "args": Val::Vec(args),
                            "variadic": fn_def.args.variadic,
                            "arity": fn_def.args.args.len() as i64,
                        })
                    })
                    .collect();

                metadata = metadata.merge(&val!({
                    "value_type": "function",
                    "signatures": Val::Vec(signatures),
                }));
            }
            Value::TypeDef(type_def) => {
                // Extract type information
                let mut type_meta = val!({
                    "value_type": "type",
                    "type_name": type_def.name.name(),
                });

                // Extract constructor function signatures if available
                if let Some(constructors) = &type_def.constructor_functions {
                    let constructor_sigs: Vec<Val> = constructors
                        .iter()
                        .map(|fn_def| {
                            let args: Vec<Val> = fn_def.args.args.iter().map(|arg| {
                            val!({
                                "name": arg.var.sym.name(),
                                "lazy": arg.lazy,
                                "type": arg.type_annotation.clone().unwrap_or("Any".to_string()),
                            })
                        }).collect();

                            val!({
                                "args": Val::Vec(args),
                                "variadic": fn_def.args.variadic,
                                "arity": fn_def.args.args.len() as i64,
                            })
                        })
                        .collect();

                    type_meta = type_meta.merge(&val!({
                        "constructor_signatures": Val::Vec(constructor_sigs),
                    }));
                }

                metadata = metadata.merge(&type_meta);
            }
            Value::Ref(Ref::Var(var_ref)) => {
                metadata = metadata.merge(&val!({
                    "value_type": "var_ref",
                    "ref_name": var_ref.var.sym.name(),
                }));
            }
            Value::Val(literal_val, type_annotation) => {
                metadata = metadata.merge(&val!({
                    "value_type": "literal",
                    "literal_value": literal_val.clone(),
                    "type_annotation": type_annotation.clone().unwrap_or("Any".to_string()),
                }));
            }
            _ => {
                // Other value types (FnCall, Flow, Lambda, etc.)
                metadata = metadata.merge(&val!({
                    "value_type": "other",
                }));
            }
        }

        Some(metadata)
    }

    /// Get function signature information for all overloads
    pub fn get_function_signatures(
        &self,
        scope: &str,
        var_name: &str,
    ) -> Option<Vec<FunctionSignature>> {
        let (_var, value) = self.get_var_metadata(scope, var_name)?;

        match value {
            Value::Fn(fn_defs) => {
                let signatures = fn_defs
                    .iter()
                    .map(|fn_def| {
                        let args = fn_def
                            .args
                            .args
                            .iter()
                            .map(|arg| FunctionArg {
                                name: arg.var.sym.name().to_string(),
                                lazy: arg.lazy,
                                type_annotation: arg.type_annotation.clone(),
                            })
                            .collect();

                        FunctionSignature {
                            args,
                            variadic: fn_def.args.variadic,
                            arity: fn_def.args.args.len(),
                        }
                    })
                    .collect();

                Some(signatures)
            }
            _ => None,
        }
    }
}

/// Structured function signature information
#[derive(Debug, Clone)]
pub struct FunctionSignature {
    pub args: Vec<FunctionArg>,
    pub variadic: bool,
    pub arity: usize,
}

/// Function argument information
#[derive(Debug, Clone)]
pub struct FunctionArg {
    pub name: String,
    pub lazy: bool,
    pub type_annotation: Option<String>,
}

// Additional types needed by VM
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Namespaces {
    pub namespaces: IndexMap<NsPath, Namespace>,
}

impl Default for Namespaces {
    fn default() -> Self {
        Self::new()
    }
}

impl Namespaces {
    pub fn new() -> Self {
        Self {
            namespaces: IndexMap::new(),
        }
    }

    pub fn get(&self, key: &NsPath) -> Option<&Namespace> {
        self.namespaces.get(key)
    }

    pub fn iter(&self) -> indexmap::map::Iter<'_, NsPath, Namespace> {
        self.namespaces.iter()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CoreLibrary {
    // Placeholder for core library functionality
}

// Core AST types without Hot prefix
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    // Core value types
    Val(Val, Option<String>), // Literal value with optional type info
    Ref(Ref),                 // Variable reference
    FnCall(FnCall),           // Function call
    Flow(Flow),               // Flow expression
    Fn(Vec<FnDef>),           // Function definitions (multiple arities)
    TypeDef(TypeDef),         // Type definition
    TypeImplementation(Box<TypeImplementation>), // Type implementation
    Unbound(Unbound),         // Unbound reference
    Cond(Option<String>, Box<Value>, Flow), // Conditional with optional branch label and flow
    CondDefault(Flow),        // Default conditional branch
    TemplateLiteral(TemplateLiteral), // Template literal
    Raw(Box<Value>),          // Raw (no-unwrap) value
    Do(Box<Value>),           // Lazy evaluation trigger
    VariadicExpansion(String), // Variadic expansion marker (...identifier)
    MultipleValues(Vec<Value>), // Multiple values for same variable
    Lambda(Lambda),           // Lambda function: (args) { body }
    Match(MatchExpr),         // Match expression for variant unions
    MatchArm(Box<MatchArm>),  // Single match arm (for match flow function bodies)
    MapWithSpread {
        // Map literal with spread entries: {...obj, key: val}
        base_entries: IndexMap<Val, Val>,
        spread_entries: Vec<(usize, Value)>, // (insertion_index, spread_value)
    },
    Placeholder(usize), // % placeholder, 1-based arg index (desugared to lambda param during parsing)
}

/// Match expression for pattern matching on variant unions
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MatchExpr {
    /// The value being matched
    pub value: Box<Value>,
    /// Match arms: (variant_name, optional_binding, body)
    pub arms: Vec<MatchArm>,
    /// Whether this is a match-all (execute all matching arms, collect results)
    pub match_all: bool,
    /// Result modifier for collecting results (map, vec, one)
    pub result_modifier: Option<ResultModifier>,
    /// Source location
    pub src: Option<Source>,
}

/// A single arm in a match expression
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MatchArm {
    /// Optional type name for qualified variant (e.g., "Result" in "Result.Ok")
    /// If None, uses suffix matching (deprecated behavior)
    pub type_name: Option<String>,
    /// The variant name to match (e.g., "Ok", "Err", "Circle")
    /// If None with type_name present, matches ANY variant of that type (type-level matching)
    pub variant: Option<String>,
    /// Optional literal value for value-equality matching (e.g., 200, "hello", true, null)
    /// When present, type_name and variant should be None.
    /// The subject is compared directly against this value using equality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_literal: Option<Box<Value>>,
    /// Optional binding name for the matched value (future: for destructuring)
    pub binding: Option<String>,
    /// The body expression to execute when matched
    pub body: Value,
    /// Source location
    pub src: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Ref {
    Var(VarRef), // Variable reference
    Ns(NsRef),   // Namespace reference
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VarRef {
    pub var: Var,
    pub data: Option<VarData>,
    pub src: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FnCall {
    pub function: Box<Value>,
    pub args: Vec<FnCallArg>,
    /// Optional deep path to apply to the function call result (e.g., `fn()[0].prop`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<DeepPath>,
    pub src: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FnCallArg {
    pub value: Value,
    pub lazy: bool,
    /// If true, the value should be spread as multiple arguments (e.g., `...vec`)
    #[serde(default)]
    pub spread: bool,
    pub src: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Flow {
    pub flow_type: FlowType,
    pub expressions: Vec<Value>,
    pub result_modifier: Option<ResultModifier>,
    pub src: Option<Source>,
    /// Namespace aliases declared inside this flow body (position, alias, source).
    /// Position is the expression index at which the alias was declared; it becomes
    /// active for all subsequent expressions in this flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<(usize, NsPath, NsPath)>,
}

// Allow boxing FnCall inside Val so maps can contain executable calls
impl ValBox for FnCall {
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
        if let Some(other_fc) = other.as_any().downcast_ref::<FnCall>() {
            self == other_fc
        } else {
            false
        }
    }

    fn hash(&self, _state: &mut dyn Hasher) {}

    fn to_string(&self) -> String {
        "<FnCall>".to_string()
    }

    fn compare(&self, other: &dyn ValBox) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<FnCall>()
            .map(|_| std::cmp::Ordering::Equal)
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "$boxed": "FnCall" }))
    }

    fn type_name(&self) -> &'static str {
        "FnCall"
    }
}

// Wrapper to store arbitrary AST Value inside Val::Box without requiring heavy trait bounds
#[derive(Debug, Clone)]
pub struct AstNode(pub Value);

impl ValBox for AstNode {
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
        if let Some(o) = other.as_any().downcast_ref::<AstNode>() {
            self.0 == o.0
        } else {
            false
        }
    }
    fn hash(&self, _state: &mut dyn Hasher) {}
    fn to_string(&self) -> String {
        "<AstNode>".to_string()
    }
    fn compare(&self, _other: &dyn ValBox) -> Option<Ordering> {
        None
    }
    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        // Minimal serialization to avoid heavy derives on Value
        Ok(serde_json::json!({"$ast": "lang::Value"}))
    }
    fn type_name(&self) -> &'static str {
        "AstNode"
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FlowType {
    Parallel,
    Serial,
    Cond,
    CondAll,
    SerialShort,
    ParallelShort,
    CondShort,
    CondAllShort,
    Pipe,
    Match,
    MatchAll,
    MatchShort,
    MatchAllShort,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ResultModifier {
    One,
    Map,
    Vec,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FnDef {
    pub args: FnArgs,
    pub body: Value,
    #[serde(default)]
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FnArgs {
    pub args: Vec<FnArg>,
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FnArg {
    pub var: Var,
    pub lazy: bool,
    pub type_annotation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TypeField {
    pub name: Sym,
    pub type_annotation: String,
}

/// A variant definition for tagged unions (e.g., `| Circle(Circle)` or `| Point`)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VariantDef {
    /// Variant name (e.g., "Circle", "Point")
    pub name: Sym,
    /// Optional type reference for type-reference variants (None for unit variants)
    pub type_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TypeDef {
    pub name: Sym,
    pub fields: Option<Vec<TypeField>>, // For struct-style types with inline field declarations
    pub type_alias: Option<TypeExpr>,   // For type aliases like `Fruit type "apple" | "banana"`
    pub variants: Option<Vec<VariantDef>>, // For variant unions like `| Circle(Circle) | Point`
    pub constructor_functions: Option<Vec<FnDef>>,
    pub implementations: Option<Vec<TypeImplementation>>,
    /// True for `enum open { ... }` declarations. Open enums accept new
    /// variants enrolled via the coercion arrow (`Source -> Enum.Variant`)
    /// from any package, and require a `_` default arm in `match`.
    /// Always `false` for non-enum `TypeDef`s.
    #[serde(default)]
    pub is_open: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TypeImplementation {
    pub source_type: String,
    pub target_type: String,
    pub implementation: Value,
    /// Source span pointing at the `Source -> Target` declaration. Used to
    /// produce useful "duplicate implementation" diagnostics that cite the
    /// two conflicting declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TemplateLiteral {
    pub parts: Vec<TemplatePart>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TemplatePart {
    Text(String),
    Expression(Box<Value>),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Lambda {
    pub args: FnArgs,     // Function arguments like (x, y)
    pub body: Box<Value>, // Function body (usually a Flow)
    #[serde(default)]
    pub flow_type: Option<FlowType>, // Optional flow type for flow-based lambdas: serial(x) { ... }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Unbound {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Var {
    pub sym: Sym,
    pub deep_set: Option<DeepPath>,
    pub deep_path: Option<DeepPath>,
    pub meta: Option<Meta>,
    pub src: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub val: Val,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VarData {
    pub type_annotation: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub namespaces: IndexMap<NsPath, Namespace>,
    pub current_namespace: NsPath,
}

// Custom serialization for Program to handle IndexMap with non-string keys
impl serde::Serialize for Program {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Program", 2)?;

        // Convert IndexMap to Vec for serialization
        let namespaces_vec: Vec<(NsPath, Namespace)> = self
            .namespaces
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        state.serialize_field("namespaces", &namespaces_vec)?;
        state.serialize_field("current_namespace", &self.current_namespace)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for Program {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ProgramHelper {
            namespaces: Vec<(NsPath, Namespace)>,
            current_namespace: NsPath,
        }

        let helper = ProgramHelper::deserialize(deserializer)?;
        Ok(Program {
            namespaces: helper.namespaces.into_iter().collect(),
            current_namespace: helper.current_namespace,
        })
    }
}

/// Namespace alias mapping: short alias path -> full source path
/// Example: ::http -> ::hot::http
pub type NamespaceAliases = IndexMap<NsPath, NsPath>;

/// Custom serialization for NamespaceAliases (IndexMap with non-string keys)
mod namespace_aliases_serde {
    use super::{IndexMap, NsPath};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(
        aliases: &IndexMap<NsPath, NsPath>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Convert to Vec of tuples for JSON-safe serialization
        let vec: Vec<(NsPath, NsPath)> = aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        vec.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<IndexMap<NsPath, NsPath>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec: Vec<(NsPath, NsPath)> = Vec::deserialize(deserializer)?;
        Ok(vec.into_iter().collect())
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Namespace {
    pub path: NsPath,
    pub scope: Scope,
    pub meta: Option<Meta>,
    /// Source file path for this namespace (for accurate file tracking)
    pub source_file: Option<std::path::PathBuf>,
    /// Namespace aliases defined in this namespace (e.g., ::http ::hot::http)
    #[serde(default, with = "namespace_aliases_serde")]
    pub aliases: NamespaceAliases,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub vars: IndexMap<Var, Value>,
}

// Custom serialization for Scope to handle IndexMap with non-string keys
impl serde::Serialize for Scope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Convert IndexMap to Vec for serialization
        let vars_vec: Vec<(Var, Value)> = self
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        vars_vec.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Scope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vars_vec: Vec<(Var, Value)> = Vec::deserialize(deserializer)?;
        Ok(Scope {
            vars: vars_vec.into_iter().collect(),
        })
    }
}

// Display implementations for boxable types
impl Display for Ref {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Var(var_ref) => write!(f, "VarRef({})", var_ref.var.sym),
            Ref::Ns(ns_ref) => {
                if let Some(func_name) = &ns_ref.function_name {
                    write!(f, "{}/{}", ns_ref.ns, func_name)
                } else {
                    write!(f, "{}", ns_ref.ns)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
