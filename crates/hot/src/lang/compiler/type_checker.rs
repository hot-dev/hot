// Type checker for Hot.
//
// Hot is gradually typed - it supports both dynamic typing (like JavaScript)
// and static typing (like TypeScript). The type checker validates type annotations
// when present and infers types where possible.

use crate::lang::ast::{
    Flow, FlowType, FnCall, FnDef, MatchArm, Namespace, Program, Ref, Source, Value, Var,
};
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use crate::lang::errors::{CompilerError, CompilerErrors, CompilerResult, ErrorLocation};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use std::path::PathBuf;

/// Walk a value tree and collect every `FnDef` reachable from it.
///
/// Multi-arity functions can show up either as a single `Value::Fn(Vec<FnDef>)`
/// (comma-joined arities) or as a `Value::MultipleValues([Value::Fn(...),
/// Value::Fn(...), ...])` when the same name is declared with multiple
/// top-level statements. This helper flattens both shapes into one list so
/// arity-based checks see every defined arity.
fn collect_fn_defs<'a>(value: &'a Value, out: &mut Vec<&'a FnDef>) {
    match value {
        Value::Fn(defs) => {
            for d in defs {
                out.push(d);
            }
        }
        Value::MultipleValues(values) => {
            for v in values {
                collect_fn_defs(v, out);
            }
        }
        _ => {}
    }
}

/// Recursively traverse a `Value`, invoking `visit` on every nested `Value`
/// (including the value itself). Used by `collect_known_types` to find
/// `TypeDef`s declared inside function bodies, lambda bodies, flow arms, etc.
fn walk_values<'a>(value: &'a Value, visit: &mut impl FnMut(&'a Value)) {
    visit(value);
    match value {
        Value::Fn(defs) => {
            for d in defs {
                walk_values(&d.body, visit);
            }
        }
        Value::Lambda(lambda) => walk_values(&lambda.body, visit),
        Value::Flow(flow) => {
            for expr in &flow.expressions {
                walk_values(expr, visit);
            }
        }
        Value::FnCall(call) => {
            walk_values(&call.function, visit);
            for arg in &call.args {
                walk_values(&arg.value, visit);
            }
        }
        Value::MultipleValues(values) => {
            for v in values {
                walk_values(v, visit);
            }
        }
        Value::Cond(_, pred, flow) => {
            walk_values(pred, visit);
            for expr in &flow.expressions {
                walk_values(expr, visit);
            }
        }
        Value::CondDefault(flow) => {
            for expr in &flow.expressions {
                walk_values(expr, visit);
            }
        }
        Value::Match(m) => {
            walk_values(&m.value, visit);
            for arm in &m.arms {
                walk_values(&arm.body, visit);
            }
        }
        Value::MatchArm(arm) => walk_values(&arm.body, visit),
        Value::Raw(inner) | Value::Do(inner) => walk_values(inner, visit),
        Value::TypeImplementation(ti) => walk_values(&ti.implementation, visit),
        Value::TemplateLiteral(tmpl) => {
            for part in &tmpl.parts {
                if let crate::lang::ast::TemplatePart::Expression(e) = part {
                    walk_values(e, visit);
                }
            }
        }
        _ => {}
    }
}

/// Literal types for enum-style unions (e.g., "apple" | "banana" | "orange")
#[derive(Debug, Clone, PartialEq)]
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

/// Type expressions for Hot's type system
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    // Built-in types that match Val enum
    Int,
    Dec,
    Str,
    Bool,
    Vec(Box<TypeExpr>),
    Map(Box<TypeExpr>, Box<TypeExpr>),
    Bytes,
    Byte,
    Null,

    // Literal types for enum-style unions
    Literal(LiteralType), // "apple", 42, true, null

    // Special types
    Any,                                    // Dynamic type
    Never,                                  // Bottom type
    Optional(Box<TypeExpr>),                // T?
    Union(Vec<TypeExpr>),                   // T | U
    Function(Vec<TypeExpr>, Box<TypeExpr>), // (T, U) -> V
    Record {
        fields: AHashMap<String, TypeExpr>,
        open: bool,
        type_name: Option<String>,
    }, // statically known map/record shape

    // User-defined types
    Named(String),                  // Custom type name
    Generic(String, Vec<TypeExpr>), // Generic<T, U>

    // Hot-specific type features
    Lazy(Box<TypeExpr>),     // lazy T
    Variadic(Box<TypeExpr>), // ...T
    Namespace(String),       // ::hot::math
    TypeRef(String),         // &Type references

    // Type constraints and metadata
    Constrained(Box<TypeExpr>, Vec<TypeConstraint>), // T where T: Constraint
}

/// Type constraints for enhanced type checking
#[derive(Debug, Clone, PartialEq)]
pub enum TypeConstraint {
    HasMethod(String, Vec<TypeExpr>, TypeExpr), // method(args) -> return
    Implements(String),                         // implements Trait
    CoreType,                                   // meta {core: true}
}

/// Function signature with enhanced type information
#[derive(Debug, Clone)]
pub struct FunctionSignature {
    return_type: TypeExpr,
    parameter_types: Vec<TypeExpr>,
    variadic: bool,
}

/// Type implementation for type extensions
#[derive(Debug, Clone)]
pub struct Implementation {
    // Implementation details will be added when needed
}

/// Type definition for custom types.
///
/// Tracks the metadata the type checker needs to reason about
/// `enum`-shaped types: whether the enum is `open` (extensible by
/// `Source -> Enum.Variant` arrows from any package), and the set of
/// known variant short names. Open enums grow this set as new arrows
/// are registered; closed enums freeze it at declaration time.
#[derive(Debug, Clone, Default)]
pub struct TypeDefinition {
    /// True for `enum open { ... }` declarations. Open enums require a
    /// `_` default arm at every `match` site. Always `false` for
    /// non-enum types and for closed enums.
    pub is_open: bool,
    /// Whether this `TypeDef` came from an `enum` declaration at all.
    /// Closed enums must be exhaustively matched (covered variants must
    /// equal the declared variant set); non-enum types skip that check.
    pub is_enum: bool,
    /// Whether this `TypeDef` is a struct/record declaration with inline
    /// fields (`Foo type { a: Int }`).
    pub is_struct: bool,
    /// Declared struct fields keyed by field name. Empty for non-struct types.
    pub fields: AHashMap<String, TypeExpr>,
    /// Short names of the variants known to belong to this enum. For
    /// closed enums, populated from `TypeDef.variants` at registration.
    /// For open enums, also extended each time a `Source -> Enum.X` arrow
    /// is registered with `X` as the variant short name.
    pub variants: AHashSet<String>,
    /// Payload type name for each variant (`variant_name -> type_ref`),
    /// when the variant declared an associated type like `Tri(Triangle)`.
    /// Unit variants (`Point`) and arrow-enrolled variants whose source
    /// type isn't recorded yet are absent from this map. Used by
    /// `coercion_available` to honor auto-unwrap on dispatch: a value
    /// of the variant's payload type can flow into a parameter typed
    /// as the enum without an explicit constructor call.
    pub variant_payloads: AHashMap<String, String>,
    /// Fully-qualified namespace where this type was declared
    /// (e.g. `::myapp/users`). Used to resolve bare type-name references
    /// inside `variant_payloads` to FQNs at lookup time so the
    /// implementation registry can use FQN keys without forcing the user
    /// to write fully-qualified payload types in their variant declarations.
    /// Empty for built-in primitives that don't have a real declaration site.
    pub declaring_namespace: String,
    /// True if this type was declared as a literal-union alias —
    /// `Foo type "a" | "b"` (closed) or `Foo type open "a" | "b"` (open).
    /// Distinguishes literal-union aliases from other shapes (struct,
    /// enum, plain alias to a primitive, …) when we need to reason about
    /// member accumulation or string→union coercion.
    pub is_literal_union: bool,
    /// Members of this literal union, accumulated across all declarations
    /// of the same type name in the same namespace. Empty for non-literal
    /// types. Order is insertion order (first declaration wins for
    /// duplicates); duplicates are silently deduped so re-extending with
    /// an existing member is a no-op rather than an error.
    pub literal_members: Vec<LiteralType>,
}

/// Enhanced type checking context
pub struct TypeContext {
    /// Stack of lexical scopes. The bottom (`scopes[0]`) holds globally-visible
    /// bindings (built-ins, current namespace top-level vars). Each function /
    /// lambda body push their own scope and pop it on exit. Variable lookup
    /// walks innermost-out.
    scopes: Vec<AHashMap<String, TypeExpr>>,
    functions: AHashMap<String, Vec<FunctionSignature>>,
    types: AHashMap<String, TypeDefinition>,
    implementations: AHashMap<String, Vec<Implementation>>, // Type -> [Implementation]
    current_namespace: String,
    /// Namespace aliases active in the current namespace, e.g. "::ctx" -> "::hot::ctx".
    /// Used to resolve qualified function calls written via the alias form.
    current_aliases: AHashMap<String, String>,
}

impl Default for TypeContext {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeContext {
    pub fn new() -> Self {
        let mut context = Self {
            // Start with a single global scope. Per-namespace bindings live
            // here too — `enter_namespace` clears them on entry. Function /
            // lambda body scopes are pushed on top.
            scopes: vec![AHashMap::new()],
            functions: AHashMap::new(),
            types: AHashMap::new(),
            implementations: AHashMap::new(),
            current_namespace: "::".to_string(),
            current_aliases: AHashMap::new(),
        };

        // Add built-in functions with their type signatures
        context.add_builtin_functions();
        context.add_builtin_types();
        context
    }

    /// Push a new lexical scope (function/lambda body).
    pub fn push_scope(&mut self) {
        self.scopes.push(AHashMap::new());
    }

    /// Pop the topmost lexical scope. No-op if only the global scope is left.
    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    /// Reset the global (bottom) scope and drop any pushed scopes — called
    /// when entering a new namespace so top-level bindings from the previous
    /// namespace don't leak in via short-name lookup.
    pub fn enter_namespace(&mut self, namespace: String) {
        self.scopes.clear();
        self.scopes.push(AHashMap::new());
        self.current_namespace = namespace;
    }

    fn add_builtin_functions(&mut self) {
        // Arithmetic functions - register with both qualified and unqualified names
        // for core function auto-import compatibility.
        //
        // We register two arities so that pure-Int calls preserve the Int
        // return type (without it, `slice(items, sub(len, n))` falsely
        // rejects because `sub`'s return type would be `Int | Dec`):
        //   1. `(Int, Int) -> Int` — narrow form for Int-only calls
        //   2. `(Int | Dec, Int | Dec) -> Int | Dec` — full form for mixed calls
        // `find_matching_signature` walks signatures in order, so the
        // narrower one takes priority when applicable.
        let math_sigs = vec![
            FunctionSignature {
                return_type: TypeExpr::Int,
                parameter_types: vec![TypeExpr::Int, TypeExpr::Int],
                variadic: false,
            },
            FunctionSignature {
                return_type: TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Dec]),
                parameter_types: vec![
                    TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Dec]),
                    TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Dec]),
                ],
                variadic: false,
            },
        ];
        self.functions.insert("add".to_string(), math_sigs.clone());
        self.functions
            .insert("::hot::math/add".to_string(), math_sigs.clone());
        self.functions.insert("sub".to_string(), math_sigs.clone());
        self.functions
            .insert("::hot::math/sub".to_string(), math_sigs);

        // String/Vec/Bytes concat is variadic (min 2 args). The actual stdlib signature
        // accepts Vec | Str | Bytes uniformly across all positions; we use Any here so
        // we don't reject legitimate calls just because we lost the precise overload.
        self.functions.insert(
            "concat".to_string(),
            vec![FunctionSignature {
                return_type: TypeExpr::Any,
                parameter_types: vec![TypeExpr::Any, TypeExpr::Any],
                variadic: true,
            }],
        );

        // Type checking functions
        self.functions.insert(
            "is-int".to_string(),
            vec![FunctionSignature {
                return_type: TypeExpr::Bool,
                parameter_types: vec![TypeExpr::Any], // Can check any type
                variadic: false,
            }],
        );

        // Test functions
        // assert-eq has 2-arg (expected, actual) and 3-arg (expected, actual, msg) arities
        self.functions.insert(
            "assert-eq".to_string(),
            vec![
                FunctionSignature {
                    return_type: TypeExpr::Bool,
                    parameter_types: vec![TypeExpr::Any, TypeExpr::Any],
                    variadic: false,
                },
                FunctionSignature {
                    return_type: TypeExpr::Bool,
                    parameter_types: vec![TypeExpr::Any, TypeExpr::Any, TypeExpr::Str],
                    variadic: false,
                },
            ],
        );

        // Boolean functions - support both 2-arg and 3-arg arities
        self.functions.insert(
            "if".to_string(),
            vec![
                FunctionSignature {
                    return_type: TypeExpr::Any, // Can return any type based on branches
                    parameter_types: vec![TypeExpr::Any, TypeExpr::Any], // (pred, then)
                    variadic: false,
                },
                FunctionSignature {
                    return_type: TypeExpr::Any, // Can return any type based on branches
                    parameter_types: vec![TypeExpr::Any, TypeExpr::Any, TypeExpr::Any], // (pred, then, else)
                    variadic: false,
                },
            ],
        );
    }

    fn add_builtin_types(&mut self) {
        // Add core types from hot-std
        let core_types = vec![
            "Int",
            "Dec",
            "Str",
            "Bool",
            "Vec",
            "Map",
            "Bytes",
            "Byte",
            "Null",
            "Any",
            "Namespace",
            "Var",
            "Fn",
            "Result",
            "Uuid",
        ];

        for type_name in core_types {
            self.types
                .insert(type_name.to_string(), TypeDefinition::default());
        }
    }

    /// Bind `name` in the topmost (innermost) scope.
    pub fn add_variable(&mut self, name: String, type_expr: TypeExpr) {
        let top = self
            .scopes
            .last_mut()
            .expect("TypeContext always has at least one scope");
        top.insert(name, type_expr);
    }

    /// Snapshot the current binding for `name` from the topmost scope.
    /// Used by per-arity body walkers that don't push a fresh scope.
    pub fn snapshot_variable(&self, name: &str) -> Option<TypeExpr> {
        self.scopes
            .last()
            .and_then(|scope| scope.get(name))
            .cloned()
    }

    /// Remove a variable binding from the topmost scope.
    pub fn remove_variable(&mut self, name: &str) {
        if let Some(top) = self.scopes.last_mut() {
            top.remove(name);
        }
    }

    /// Replace the active set of namespace aliases (call when entering a namespace).
    pub fn set_current_aliases(&mut self, aliases: AHashMap<String, String>) {
        self.current_aliases = aliases;
    }

    pub fn current_aliases_ref(&self) -> &AHashMap<String, String> {
        &self.current_aliases
    }

    /// Look up a namespace alias to its canonical form, e.g. "::ctx" -> "::hot::ctx".
    pub fn resolve_alias(&self, alias: &str) -> Option<&String> {
        self.current_aliases.get(alias)
    }

    pub fn add_function(&mut self, name: String, signature: FunctionSignature) {
        self.functions.entry(name).or_default().push(signature);
    }

    pub fn replace_function_signatures(
        &mut self,
        name: String,
        signatures: Vec<FunctionSignature>,
    ) {
        self.functions.insert(name, signatures);
    }

    pub fn add_type(&mut self, name: String, type_def: TypeDefinition) {
        self.types.insert(name, type_def);
    }

    /// Walk the lexical scope stack innermost-out.
    pub fn get_variable_type(&self, name: &str) -> Option<&TypeExpr> {
        for scope in self.scopes.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(t);
            }
        }
        None
    }

    pub fn get_function_signature(&self, name: &str) -> Option<&Vec<FunctionSignature>> {
        self.functions.get(name)
    }

    pub fn get_type_definition(&self, name: &str) -> Option<&TypeDefinition> {
        self.types.get(name)
    }

    pub fn get_implementations(&self, target_type: &str) -> Option<&Vec<Implementation>> {
        self.implementations.get(target_type)
    }
}

/// Type checker for Hot programs
pub struct TypeChecker {
    context: TypeContext,
    errors: CompilerErrors,
    current_file: Option<PathBuf>,
    /// Track if we're inside a pipe flow (for adjusting arity checks)
    in_pipe_flow: bool,
    /// All type names discovered in the program (for type resolution).
    ///
    /// This is populated by `collect_known_types`, indexed by SHORT name only
    /// (no namespace prefix). It is used as a permissive fallback for
    /// `looks_like_custom_type` when more specific resolution can't decide.
    known_types: AHashSet<String>,
    /// Fully-qualified type registry: `::ns/Name` → resolved `TypeExpr`.
    ///
    /// Built-in primitives declared in `::hot::type` map to their primitive
    /// `TypeExpr` (e.g. `::hot::type/Str` → `TypeExpr::Str`); user types map to
    /// `TypeExpr::Named("::ns/Name")`. This mirrors how the compiler/VM treats
    /// types — they're variables holding `Value::TypeDef`, looked up by
    /// fully-qualified name.
    qualified_types: AHashMap<String, TypeExpr>,
    /// Short name → fully-qualified name for any type declared with `core: true`
    /// metadata. Lets unqualified references like `Account` resolve when the
    /// declaration lives in another namespace, the same way the compiler
    /// auto-imports core variables.
    core_types: AHashMap<String, String>,
    /// Registered `(source_type, target_type)` coercion implementations,
    /// keyed by **fully-qualified** type names so that two distinct types
    /// in different namespaces sharing a short name (e.g. each module's
    /// own `Account`) produce independent registry entries. Populated from
    /// `Value::TypeImplementation` namespace vars and from
    /// `TypeDef.implementations` via [`Self::register_implementation`],
    /// which routes both source and target through [`Self::resolve_to_fqn`]
    /// before insertion.
    ///
    /// Dotted variant targets (`Source -> Enum.Variant`) are stored with
    /// the enum part fully-qualified (`::ns/Enum.Variant`); call sites
    /// that need the enum's short name strip the FQN with
    /// [`Self::short_type_name`].
    ///
    /// User-facing diagnostics (`ambiguous-type-implementation`,
    /// `missing-type-implementation`) shorten the keys back to short form
    /// so error messages preserve the surface syntax users wrote.
    impl_registry: AHashSet<(String, String)>,
    /// First-seen source location for each registered coercion. Used to
    /// produce a useful "duplicate implementation" diagnostic that cites
    /// both declarations when a second arrow with the same
    /// `(source, target)` short-name pair is encountered.
    impl_locations: AHashMap<(String, String), Option<ErrorLocation>>,
    /// Fully-qualified variable registry: `::ns/var-name` → inferred `TypeExpr`.
    ///
    /// Populated by the pre-pass in `check_program` so that a `Ref::Ns`
    /// reference (e.g. `::a/api-url`) and var imports (`local ::a/api-url`)
    /// can recover the imported variable's type rather than falling back to
    /// `Any`. Mirrors how the compiler/VM looks up variables in
    /// `NamespaceRegistry`.
    qualified_variables: AHashMap<String, TypeExpr>,
    /// Short name → list of fully-qualified names for any variable declared
    /// with `core: true` metadata. Multiple entries are common for function
    /// aliases (e.g. `length` is `core: true` in both `::hot::coll` and
    /// `::hot::str`); we collect them all so the function-call path can union
    /// every available signature when the type checker resolves an
    /// auto-imported call.
    core_variables: AHashMap<String, Vec<String>>,
    /// Depth counter: how many fn / lambda bodies we are currently inside.
    ///
    /// While > 0 we suppress emission of `type-mismatch` and
    /// `unresolved-type` errors because the inference paths involved
    /// (Union vs Union compatibility, dot-access value typing, generic-aware
    /// annotation parsing) have known gaps that produce false positives.
    /// `arity-mismatch` errors are always emitted — that's the whole point
    /// of walking bodies. Top-level expressions still get the full check,
    /// so nothing existing is lost.
    in_fn_body: u32,
    /// Stack of inferred match-subject types. Pushed when we descend into
    /// the body of a `fn match (s: T) { ... }` (the subject is the first
    /// parameter); popped on exit. Match arms inside the body consult the
    /// top of the stack to enforce exhaustiveness against `T`'s enum
    /// definition. A `None` entry means we're inside a match flow whose
    /// subject type couldn't be inferred (function with no annotation,
    /// untyped REPL expression, etc.) — exhaustiveness is then skipped
    /// rather than raising spurious errors.
    match_subject_stack: Vec<Option<TypeExpr>>,
    /// Fully-qualified name (`::ns/name`) → optional custom note for every
    /// definition declared with `meta { deprecated: true }` or
    /// `meta { deprecated: "note" }`. Populated during the qualified-variable
    /// pre-pass and consulted at call sites to emit `deprecated-usage`
    /// warnings.
    deprecated_defs: AHashMap<String, Option<String>>,
    /// Fully-qualified name (`::ns/name`) of the top-level definition whose
    /// body is currently being checked, if any. Used to suppress
    /// `deprecated-usage` warnings for a deprecated function's references to
    /// itself (e.g. one arity delegating to another), which would otherwise
    /// flag the stdlib definition site and surface on every downstream user.
    current_def: Option<String>,
    /// Source of the definition named by `current_def`, used as the warning
    /// location for diagnostics (like return-annotation mismatches) that
    /// have no more precise source of their own. Warnings without a
    /// project-file location are dropped by the check pipeline's project
    /// scoping, so a definition-site fallback keeps them visible.
    current_def_src: Option<Source>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeChecker {
    pub fn new() -> Self {
        Self {
            context: TypeContext::new(),
            errors: CompilerErrors::new(),
            current_file: None,
            in_pipe_flow: false,
            known_types: AHashSet::new(),
            qualified_types: AHashMap::new(),
            core_types: AHashMap::new(),
            impl_registry: AHashSet::new(),
            impl_locations: AHashMap::new(),
            qualified_variables: AHashMap::new(),
            core_variables: AHashMap::new(),
            in_fn_body: 0,
            match_subject_stack: Vec::new(),
            deprecated_defs: AHashMap::new(),
            current_def: None,
            current_def_src: None,
        }
    }

    /// Inspect a definition's `meta` for a `deprecated` flag.
    ///
    /// Returns `None` when not deprecated, `Some(None)` for a bare
    /// `deprecated: true`, and `Some(Some(note))` for `deprecated: "note"`
    /// (a custom migration message surfaced in the diagnostic).
    fn deprecated_meta_note(var: &Var) -> Option<Option<String>> {
        let meta = var.meta.as_ref()?;
        if let Val::Map(map) = &meta.val {
            for (key, value) in map.iter() {
                if let Val::Str(key_str) = key
                    && &**key_str == "deprecated"
                {
                    return match value {
                        Val::Bool(true) => Some(None),
                        Val::Str(s) => Some(Some((**s).to_string())),
                        _ => None,
                    };
                }
            }
        }
        None
    }

    /// Resolve a call's `lookup_name` to the matched deprecated definition if
    /// the callee was declared deprecated. Returns the resolved definition key
    /// (the `::ns/name` form stored in `deprecated_defs`) alongside its
    /// optional custom note. Checks the already-qualified form, the
    /// current-namespace-local form, and core auto-imported exports (which is
    /// how a bare `try-call(...)` resolves to `::hot::lang/try-call`).
    ///
    /// Returning the matched key lets callers suppress self-references: a
    /// deprecated function's own body may call itself (e.g. one arity
    /// delegating to another), and we don't want to warn on the definition.
    fn lookup_deprecated_note(&self, lookup_name: &str) -> Option<(String, Option<String>)> {
        if let Some(note) = self.deprecated_defs.get(lookup_name) {
            return Some((lookup_name.to_string(), note.clone()));
        }
        let qualified = format!("{}/{}", self.context.current_namespace, lookup_name);
        if let Some(note) = self.deprecated_defs.get(&qualified) {
            return Some((qualified, note.clone()));
        }
        if let Some(qualifieds) = self.core_variables.get(lookup_name) {
            for q in qualifieds {
                if let Some(note) = self.deprecated_defs.get(q) {
                    return Some((q.clone(), note.clone()));
                }
            }
        }
        None
    }

    /// Drain non-fatal warnings collected during `check_program` into a
    /// self-contained `CompilerErrors` (warnings + the source cache needed to
    /// render them). Errors are left untouched.
    pub fn take_warnings(&mut self) -> CompilerErrors {
        let mut out = CompilerErrors::new();
        out.warnings = std::mem::take(&mut self.errors.warnings);
        out.source_cache = self.errors.source_cache.clone();
        out
    }

    /// Convert AST Source to ErrorLocation for error reporting
    fn source_to_error_location(&self, source: &Source) -> ErrorLocation {
        ErrorLocation {
            line: source.line,
            column: source.column,
            position: source.position,
            length: source.length,
            file: source
                .file
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| self.current_file.clone()),
        }
    }

    pub fn set_source_info(&mut self, file: Option<PathBuf>, content: Option<String>) {
        self.current_file = file.clone();
        if let (Some(f), Some(c)) = (file, content) {
            self.errors.add_source(f.display().to_string(), c);
        }
    }

    /// Add a source file for error reporting
    pub fn add_source_file(&mut self, file_path: PathBuf, content: String) {
        self.errors
            .add_source(file_path.display().to_string(), content);
    }

    /// Create an error location by searching for the type name in source files
    fn create_error_location_for_type(&self, type_name: &str) -> Option<ErrorLocation> {
        // Search through all source files for the type name
        for (file_path_str, content) in &self.errors.source_cache {
            if let Some(position) = content.find(type_name) {
                let (line, column) = self.calculate_line_column(content, position);
                return Some(ErrorLocation {
                    line,
                    column,
                    position,
                    length: type_name.len(),
                    file: Some(PathBuf::from(file_path_str)),
                });
            }
        }

        // Fallback to current file if available
        self.current_file.as_ref().map(|file| ErrorLocation {
            line: 1,
            column: 1,
            position: 0,
            length: type_name.len(),
            file: Some(file.clone()),
        })
    }

    /// Create an error location by searching for function call patterns
    fn create_error_location_for_function_call(&self, func_name: &str) -> Option<ErrorLocation> {
        // Search for function call patterns like "func_name(" in source files
        let call_pattern = format!("{} ", func_name); // Look for "add " pattern
        let paren_pattern = format!("{}(", func_name); // Look for "add(" pattern

        // Prioritize files that are likely to be user code (not in pkg directories)
        let mut user_files = Vec::new();
        let mut pkg_files = Vec::new();

        for (file_path_str, content) in &self.errors.source_cache {
            if file_path_str.contains("/pkg/") {
                pkg_files.push((file_path_str, content));
            } else {
                user_files.push((file_path_str, content));
            }
        }

        // Search user files first, then pkg files
        for files in [user_files, pkg_files] {
            for (file_path_str, content) in files {
                // Try both patterns
                for pattern in [&call_pattern, &paren_pattern] {
                    if let Some(position) = content.find(pattern) {
                        let (line, column) = self.calculate_line_column(content, position);
                        return Some(ErrorLocation {
                            line,
                            column,
                            position,
                            length: func_name.len(),
                            file: Some(PathBuf::from(file_path_str)),
                        });
                    }
                }
            }
        }

        // Fallback to current file if available
        self.current_file.as_ref().map(|file| ErrorLocation {
            line: 1,
            column: 1,
            position: 0,
            length: func_name.len(),
            file: Some(file.clone()),
        })
    }

    /// Calculate line and column from position in source
    fn calculate_line_column(&self, source: &str, position: usize) -> (usize, usize) {
        let mut line = 1;
        let mut column = 1;

        for (i, ch) in source.char_indices() {
            if i >= position {
                break;
            }
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }

        (line, column)
    }

    pub fn check_program(&mut self, program: &Program) -> CompilerResult<()> {
        tracing::debug!("TypeChecker: Starting type checking");

        // First pass: collect all type names from the program
        self.collect_known_types(program);

        // Second pass: collect cross-namespace variable types so that
        // `Ref::Ns` references and var imports resolve to a real type instead
        // of falling back to `Any`. Mirrors how the compiler/VM resolve
        // `::ns/var` against the namespace registry.
        //
        // We iterate to a fixed point so chains like
        //     ::a x: Str "hi"
        //     ::b y ::a/x          // depends on ::a/x
        //     ::c z ::b/y          // depends on ::b/y
        // converge regardless of namespace iteration order. The map only ever
        // grows and the underlying inference is deterministic, so a small
        // bound on iterations is sufficient (3 covers everything realistic;
        // we cap at 5 as a safety net).
        for _ in 0..5 {
            let before = self.qualified_variables.len();
            self.collect_qualified_variables(program);
            if self.qualified_variables.len() == before {
                break;
            }
        }
        // Refresh type metadata now that top-level alias variables have been
        // inferred. Struct fields can legally use namespace-local aliases such
        // as `Session ::ai::session/Session` followed by `session: Session`;
        // resolving those fields before the alias pass would leave a bare short
        // name that can collide with unrelated packages in aggregate projects.
        self.collect_known_types(program);
        tracing::debug!(
            "TypeChecker: Discovered {} qualified variables ({} core)",
            self.qualified_variables.len(),
            self.core_variables.len(),
        );

        for (_ns_path, namespace) in &program.namespaces {
            self.check_namespace(namespace)?;
        }

        if self.errors.is_empty() {
            tracing::debug!("TypeChecker: All type checks passed");
            Ok(())
        } else {
            tracing::debug!("TypeChecker: Found {} type errors", self.errors.len());
            Err(self.errors.clone())
        }
    }

    /// Best-effort expression inference for editor features. This intentionally
    /// reuses the checker state built by `check_program` so LSP field completion
    /// sees the same row-preserved record shapes as diagnostics.
    pub fn infer_value_type_for_lsp(&mut self, value: &Value) -> TypeExpr {
        self.infer_value_type(value)
    }

    /// Collect all type names from the program for type resolution
    fn collect_known_types(&mut self, program: &Program) {
        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();
            for (var, value) in &namespace.scope.vars {
                // Top-level vars: keep the original (var, value) so we can read
                // `core: true` metadata off the var. Locally-declared types
                // inside fn bodies don't have a paired `Var` — they only carry
                // their own `TypeDef.name` — so they're handled separately.
                self.register_top_level_value(&ns_str, var, value);

                // Walk function/lambda/flow bodies for nested declarations.
                // Test files in particular often define helper types inside
                // `meta ["test"] fn () { ... }` bodies, and without this pass
                // those types resolve to "unknown" everywhere they're used.
                //
                // Skip the root visit (`walk_values` calls `visit(value)` first
                // before recursing): the root is the top-level value we just
                // registered above, and re-registering it as "nested" would
                // (a) double-account, and (b) trigger Policy-1 errors against
                // legitimate top-level open literal-union extensions.
                let mut is_root = true;
                walk_values(value, &mut |v| {
                    if is_root {
                        is_root = false;
                        return;
                    }
                    self.register_nested_value(&ns_str, v);
                });
            }
        }
        tracing::debug!(
            "TypeChecker: Discovered {} type definitions ({} core), {} implementations",
            self.qualified_types.len(),
            self.core_types.len(),
            self.impl_registry.len(),
        );
    }

    /// Walk every namespace and register the inferred type of each top-level
    /// variable in `qualified_variables`. This pre-pass exists so that
    /// `Ref::Ns(::a/api-url)` and var imports (`local-var ::a/api-url`) can
    /// recover the imported type, the same way the compiler/VM resolve those
    /// references at runtime.
    ///
    /// "Shallow" inference deliberately doesn't recurse into function /
    /// lambda bodies — those are walked later by `check_namespace` for
    /// type-mismatch checking. This pass only needs the *declared* types of
    /// top-level vars, which are cheap to determine and have no side effects.
    fn collect_qualified_variables(&mut self, program: &Program) {
        for (ns_path, namespace) in &program.namespaces {
            let ns_str = ns_path.to_string();

            // Local namespace alias map for resolving aliased Ref::Ns values
            // referenced from var-import RHSs declared in this namespace.
            let mut alias_map: AHashMap<String, String> = AHashMap::new();
            for (alias, target) in &namespace.aliases {
                alias_map.insert(alias.to_string(), target.to_string());
            }

            for (var, value) in &namespace.scope.vars {
                let var_name = var.sym.name().to_string();
                let qualified = format!("{}/{}", ns_str, var_name);

                if let Some(note) = Self::deprecated_meta_note(var) {
                    self.deprecated_defs.insert(qualified.clone(), note);
                }

                let inferred = self.shallow_infer_top_level_type(var, value, &alias_map, &ns_str);
                if !matches!(inferred, TypeExpr::Any)
                    || !self.qualified_variables.contains_key(&qualified)
                {
                    self.qualified_variables.insert(qualified.clone(), inferred);
                }

                if CoreVariableRegistry::has_core_metadata(var) {
                    let entries = self.core_variables.entry(var_name).or_default();
                    if !entries.contains(&qualified) {
                        entries.push(qualified);
                    }
                }
            }
        }
    }

    /// Determine a top-level variable's type from its declaration without
    /// recursing into function/lambda bodies. Used by
    /// `collect_qualified_variables` and exposed as a small visible surface
    /// for cross-namespace lookups.
    fn shallow_infer_top_level_type(
        &self,
        var: &Var,
        value: &Value,
        alias_map: &AHashMap<String, String>,
        ns_str: &str,
    ) -> TypeExpr {
        let inferred = match value {
            Value::Val(val, type_annot) => {
                if let Some(annotation) = type_annot {
                    if annotation == "Map" && matches!(val, Val::Map(_)) {
                        return self.apply_annotation_constraint_pure(
                            self.infer_val_type(val),
                            var.type_annotation.as_deref(),
                        );
                    }
                    self.parse_type_annotation_pure(annotation)
                } else {
                    self.infer_val_type(val)
                }
            }
            Value::Fn(fn_defs) => {
                if let Some(first) = fn_defs.first() {
                    let params: Vec<TypeExpr> = first
                        .args
                        .args
                        .iter()
                        .map(|arg| {
                            arg.type_annotation
                                .as_ref()
                                .map(|s| self.parse_type_annotation_pure(s))
                                .unwrap_or(TypeExpr::Any)
                        })
                        .collect();
                    let ret = first
                        .return_type
                        .as_ref()
                        .map(|s| self.parse_type_annotation_pure(s))
                        .unwrap_or(TypeExpr::Any);
                    TypeExpr::Function(params, Box::new(ret))
                } else {
                    TypeExpr::Any
                }
            }
            Value::Lambda(lambda) => {
                let params: Vec<TypeExpr> = lambda
                    .args
                    .args
                    .iter()
                    .map(|arg| {
                        arg.type_annotation
                            .as_ref()
                            .map(|s| self.parse_type_annotation_pure(s))
                            .unwrap_or(TypeExpr::Any)
                    })
                    .collect();
                TypeExpr::Function(params, Box::new(TypeExpr::Any))
            }
            Value::TypeDef(type_def) => {
                let short = type_def.name.name().to_string();
                let qualified = format!("{}/{}", ns_str, short);
                TypeExpr::Named(qualified)
            }
            Value::TypeImplementation(_) => {
                // Coercion functions: shape is (Source) -> Target. We don't
                // need to record a precise signature here because
                // `infer_function_call_type`'s `::hot::type/X` path handles
                // coercions via `impl_registry` directly.
                TypeExpr::Function(vec![TypeExpr::Any], Box::new(TypeExpr::Any))
            }
            Value::Ref(Ref::Ns(ns_ref)) => {
                // var imports: `local-var ::ns/var` → look up the source.
                self.lookup_qualified_ns_ref(ns_ref, alias_map)
                    .unwrap_or(TypeExpr::Any)
            }
            Value::Ref(Ref::Var(var_ref)) => {
                // Same-namespace top-level aliases (`alias original`) need to
                // preserve known record shapes when imported from another
                // namespace. `collect_qualified_variables` iterates to a fixed
                // point, so chained aliases converge as earlier entries become
                // known.
                if var_ref.var.deep_path.is_none() && var_ref.var.deep_set.is_none() {
                    let target_name = var_ref.var.sym.name();
                    if target_name != var.sym.name() {
                        let qualified = format!("{}/{}", ns_str, target_name);
                        return self
                            .qualified_variables
                            .get(&qualified)
                            .cloned()
                            .unwrap_or(TypeExpr::Any);
                    }
                }
                TypeExpr::Any
            }
            Value::MultipleValues(values) => {
                // Var imports declared as `local ::ns/var` are parsed into
                // [VarRef(local-self), Ref::Ns(target)]. Skip self-VarRefs and
                // pick the first informative entry.
                for v in values {
                    let inferred = self.shallow_infer_top_level_type(var, v, alias_map, ns_str);
                    if !matches!(inferred, TypeExpr::Any) {
                        return inferred;
                    }
                }
                TypeExpr::Any
            }
            Value::MapWithSpread { .. } => {
                TypeExpr::Map(Box::new(TypeExpr::Any), Box::new(TypeExpr::Any))
            }
            _ => TypeExpr::Any,
        };
        self.apply_annotation_constraint_pure(inferred, var.type_annotation.as_deref())
    }

    /// Resolve a `Ref::Ns` to a fully-qualified `::ns/var` key and look it up
    /// in `qualified_variables`. Honors the local alias map so writing
    /// `::a/api-url` after `::a ::test::parity::a` resolves to
    /// `::test::parity::a/api-url`.
    fn lookup_qualified_ns_ref(
        &self,
        ns_ref: &crate::lang::ast::NsRef,
        alias_map: &AHashMap<String, String>,
    ) -> Option<TypeExpr> {
        let func_name = ns_ref.function_name.as_ref()?;
        let raw_ns = ns_ref.ns.to_string();
        let resolved_ns = alias_map.get(&raw_ns).cloned().unwrap_or(raw_ns);
        let qualified = format!("{}/{}", resolved_ns, func_name);
        self.qualified_variables.get(&qualified).cloned()
    }

    /// Pure (no `&mut self`) variant of `parse_type_annotation` for use in the
    /// pre-pass where we don't want to mutate the checker. Falls through to
    /// the regular parser; we only call it for declared annotations, which
    /// are the simple cases the parser already handles deterministically.
    fn parse_type_annotation_pure(&self, annotation: &str) -> TypeExpr {
        // The mutating path is only "mutating" because it might emit errors;
        // for the pre-pass we discard those by routing through a temporary
        // checker. In practice the existing `parse_type_annotation` is
        // side-effect-free for valid annotations.
        let mut tmp = TypeChecker::new();
        tmp.qualified_types = self.qualified_types.clone();
        tmp.core_types = self.core_types.clone();
        tmp.known_types = self.known_types.clone();
        tmp.parse_type_annotation(annotation, None)
    }

    /// Register a top-level `(var, value)` pair, picking up `TypeDef`s,
    /// `TypeImplementation`s, and `core: true` metadata. Mirrors the original
    /// inline match so the per-namespace loop can stay readable.
    fn register_top_level_value(
        &mut self,
        ns_str: &str,
        var: &crate::lang::ast::Var,
        value: &Value,
    ) {
        match value {
            Value::TypeDef(type_def) => {
                let short = var.sym.name().to_string();
                let qualified = format!("{}/{}", ns_str, short);

                self.known_types.insert(short.clone());

                // Pre-register enum/literal-union metadata first so the
                // resolver below can read back the *accumulated* literal
                // members for open literal unions (a second declaration
                // contributes additional members and must update the
                // resolved expression to reflect the union).
                self.register_enum_metadata(&short, ns_str, type_def, var.src.as_ref());

                let resolved = if ns_str == "::hot::type"
                    && let Some(primitive) = self.try_resolve_builtin_type(&short)
                {
                    primitive
                } else if let Some(td) = self.context.types.get(&short)
                    && td.is_literal_union
                {
                    // Build the resolved type from the *accumulated*
                    // literal members so re-declarations of an open
                    // literal union update the union's shape. For closed
                    // literal unions this is identical to the original
                    // alias (one declaration, no accumulation).
                    Self::literal_members_to_type_expr(&td.literal_members)
                } else if let Some(alias) = &type_def.type_alias {
                    // Mixed/non-literal aliases (e.g. `Foo type Int | Str`)
                    // fall through here.
                    Self::ast_type_to_type_expr(alias)
                } else {
                    TypeExpr::Named(qualified.clone())
                };

                self.qualified_types.insert(qualified.clone(), resolved);

                if CoreVariableRegistry::has_core_metadata(var) {
                    self.core_types.insert(short, qualified);
                }

                if let Some(impls) = &type_def.implementations {
                    for ti in impls {
                        self.register_implementation(
                            &ti.source_type,
                            &ti.target_type,
                            ti.src.as_ref(),
                            ns_str,
                            true,
                        );
                    }
                }
            }
            Value::TypeImplementation(ti) => {
                self.register_implementation(
                    &ti.source_type,
                    &ti.target_type,
                    ti.src.as_ref(),
                    ns_str,
                    true,
                );
            }
            _ => {}
        }
    }

    /// Enforce exhaustiveness/default-arm rules for a `match` flow.
    ///
    /// - `enum open` subjects must include a `_` default arm. The variant
    ///   set is unbounded (any package may enroll new variants via
    ///   `Source -> Enum.X` arrows), so the type checker can't prove the
    ///   match handles every future case.
    /// - Closed `enum` subjects must either name every declared variant
    ///   or include a `_` default arm.
    ///
    /// When the subject type can't be tied to a declared enum (e.g.
    /// matches on `Int`/`Str` literals, on `Any`, on the result of a
    /// function call inside a non-`fn match` standalone match flow),
    /// no error is emitted - existing behavior is preserved.
    fn check_match_exhaustiveness(&mut self, flow: &Flow) {
        let Some(subject_type) = self.match_subject_stack.last().cloned().flatten() else {
            return;
        };
        let arms: Vec<&MatchArm> = flow
            .expressions
            .iter()
            .filter_map(|e| match e {
                Value::MatchArm(arm) => Some(&**arm),
                _ => None,
            })
            .collect();
        let location = flow
            .src
            .as_ref()
            .map(|src| self.source_to_error_location(src));
        self.check_match_exhaustiveness_for(&subject_type, &arms, location);
    }

    /// Same exhaustiveness logic as `check_match_exhaustiveness` but driven
    /// by an explicit subject type and arm list. Used by standalone
    /// `Value::Match(MatchExpr)` (where the subject is part of the AST node
    /// rather than the implicit first parameter of a `fn match`) and by
    /// match-flow checking via `check_match_exhaustiveness`.
    fn check_match_exhaustiveness_for(
        &mut self,
        subject_type: &TypeExpr,
        arms: &[&MatchArm],
        location: Option<ErrorLocation>,
    ) {
        let Some(short_name) = Self::type_expr_short_name(subject_type) else {
            return;
        };

        let (is_enum, is_open, declared_variants) = match self.context.types.get(&short_name) {
            Some(td) if td.is_enum => (true, td.is_open, td.variants.clone()),
            _ => return,
        };
        if !is_enum {
            return;
        }

        let mut has_default = false;
        let mut has_type_level_cover = false;
        let mut covered: AHashSet<String> = AHashSet::new();
        for arm in arms {
            let is_default =
                arm.type_name.is_none() && arm.variant.is_none() && arm.value_literal.is_none();
            if is_default {
                has_default = true;
                continue;
            }
            if arm.variant.is_none()
                && arm.value_literal.is_none()
                && let Some(type_name) = &arm.type_name
                && Self::short_type_name(type_name) == short_name
            {
                has_type_level_cover = true;
                continue;
            }
            if let Some(variant) = &arm.variant {
                covered.insert(variant.clone());
            }
        }

        if is_open {
            if !has_default {
                self.errors.add(CompilerError::OpenEnumMatchMissingDefault {
                    enum_name: short_name,
                    location,
                });
            }
            return;
        }

        if has_default || has_type_level_cover {
            return;
        }
        let missing: Vec<String> = declared_variants
            .iter()
            .filter(|v| !covered.contains(*v))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let mut sorted = missing;
            sorted.sort();
            self.errors.add(CompilerError::NonExhaustiveMatch {
                enum_name: short_name,
                missing_variants: sorted,
                location,
            });
        }
    }

    /// Register the metadata the type checker needs to reason about a
    /// `TypeDef` declaration: enum variants/payloads, the declaring
    /// namespace, openness, and (for literal-union aliases) accumulated
    /// members across re-declarations. Used by both the pre-pass
    /// (`register_top_level_value`) and the main check pass
    /// (`process_type_definition`).
    ///
    /// Behavior when the short name already has an entry:
    /// - For an **open enum** declaration we merge variant sets so that
    ///   variants enrolled via `Source -> Enum.X` arrows from earlier in
    ///   the pass aren't lost.
    /// - For a **closed enum** declaration we **replace** the variant set
    ///   with what's declared. Two unrelated enums in different namespaces
    ///   that happen to share a short name (e.g. core `Result` vs a local
    ///   `Result enum { OkVal, ErrVal }` in a test file) would otherwise
    ///   merge into a bogus combined variant set that breaks exhaustiveness
    ///   checking everywhere. Last-write-wins is order-dependent but
    ///   mirrors how short-name lookups already resolve elsewhere.
    /// - For an **open literal union** in the same namespace we
    ///   accumulate (and dedup) members across declarations. A different
    ///   namespace with the same short name resets the accumulator so
    ///   unrelated `Fruit` types stay independent.
    ///
    /// `src` is the source span of the declaration (typically the
    /// `Var.src` from the wrapping `Var`); it's used for diagnostic
    /// locations on `OpenLiteralUnionMismatch` errors. May be `None`
    /// when called from contexts that don't carry a `Var`.
    fn register_enum_metadata(
        &mut self,
        short: &str,
        ns_str: &str,
        type_def: &crate::lang::ast::TypeDef,
        src: Option<&Source>,
    ) {
        let is_enum = type_def.variants.is_some();
        let is_struct = type_def.fields.is_some();
        let mut struct_fields = AHashMap::new();
        if let Some(fields) = &type_def.fields {
            for field in fields {
                struct_fields.insert(
                    field.name.name().to_string(),
                    self.ast_type_to_type_expr_in_namespace(&field.type_expr, ns_str),
                );
            }
        }
        let mut variants = AHashSet::new();
        let mut payloads: AHashMap<String, String> = AHashMap::new();
        if let Some(vs) = &type_def.variants {
            for v in vs {
                let name = v.name.name().to_string();
                variants.insert(name.clone());
                if let Some(payload) = &v.type_ref {
                    payloads.insert(name, payload.clone());
                }
            }
        }

        // Literal-union accumulation: detect literal-union aliases, then
        // accumulate members across multiple top-level declarations of the
        // same short name in the same namespace. Diagnose openness
        // mismatches and the `open`-on-non-literal misuse before mutating
        // the entry so the user gets a clean error.
        let lit_members = type_def
            .type_alias
            .as_ref()
            .map(Self::extract_literal_members)
            .unwrap_or_default();
        let alias_is_literal_union = !lit_members.is_empty();
        let alias_present = type_def.type_alias.is_some();

        if type_def.is_open && alias_present && !alias_is_literal_union {
            self.errors.add(CompilerError::OpenLiteralUnionMismatch {
                type_name: short.to_string(),
                message: "the `open` modifier only applies to literal unions \
                     (e.g. `\"a\" | \"b\"` or `1 | 2 | 3`); this alias is not a literal union"
                    .to_string(),
                location: src.map(|s| self.source_to_error_location(s)),
            });
        }

        // `context.types` is keyed by short name and is global across
        // namespaces, so two distinct types named `Fruit` in different
        // namespaces share the same entry. We only consider the existing
        // entry "the same logical type" when its declaring namespace
        // matches the current one; cross-namespace short-name collisions
        // are treated as independent declarations (consistent with the
        // long-standing behavior for variants/payloads above).
        let same_ns_existing = self
            .context
            .types
            .get(short)
            .map(|td| td.declaring_namespace == ns_str || td.declaring_namespace.is_empty())
            .unwrap_or(false);

        // Pre-check openness mismatch *before* taking the entry mutably
        // so the diagnostic uses the previous (existing) state. Only
        // applies within the same namespace — a different module
        // declaring its own `Fruit` is its own type.
        if alias_is_literal_union
            && same_ns_existing
            && let Some(existing) = self.context.types.get(short)
            && existing.is_literal_union
            && existing.is_open != type_def.is_open
        {
            let msg = if existing.is_open {
                format!(
                    "this declaration is closed but `{}` was already declared as `open`; \
                     drop `open` from the original or add `open` here",
                    short
                )
            } else {
                format!(
                    "this declaration uses `open` but `{}` was already declared as closed; \
                     add `open` to the original or drop it here",
                    short
                )
            };
            self.errors.add(CompilerError::OpenLiteralUnionMismatch {
                type_name: short.to_string(),
                message: msg,
                location: src.map(|s| self.source_to_error_location(s)),
            });
        }

        {
            let entry = self.context.types.entry(short.to_string()).or_default();
            entry.is_open = entry.is_open || type_def.is_open;
            entry.is_enum = entry.is_enum || is_enum;
            if is_struct {
                entry.is_struct = true;
                entry.fields = struct_fields.clone();
            }
            // Remember where this type was declared so the FQN resolver can
            // namespace-qualify bare payload references (`Triangle` in
            // `Tri(Triangle)`) the same way the user-facing checks do.
            // First-writer wins — re-registration during the second pass
            // shouldn't clobber the original declaring namespace.
            if entry.declaring_namespace.is_empty() {
                entry.declaring_namespace = ns_str.to_string();
            }

            if alias_is_literal_union {
                // If the previous entry came from a different namespace,
                // treat this as a fresh registration: drop the prior literal
                // state so members from the unrelated type don't bleed into
                // this one's accumulator.
                if !same_ns_existing {
                    entry.literal_members.clear();
                    entry.is_open = type_def.is_open;
                    entry.declaring_namespace = ns_str.to_string();
                }
                entry.is_literal_union = true;
                // Accumulate members in declaration order, deduping. This
                // makes re-extending with an existing literal a no-op rather
                // than an error, which matches the user's intuition (an
                // extension that includes already-known members is harmless).
                for lit in &lit_members {
                    if !entry.literal_members.contains(lit) {
                        entry.literal_members.push(lit.clone());
                    }
                }
            }

            if is_enum && !type_def.is_open {
                entry.variants = variants.clone();
                entry.variant_payloads = payloads.clone();
            } else {
                for v in &variants {
                    entry.variants.insert(v.clone());
                }
                for (k, v) in &payloads {
                    entry.variant_payloads.insert(k.clone(), v.clone());
                }
            }
        }

        let qualified = format!("{}/{}", ns_str, short);
        let entry = self.context.types.entry(qualified).or_default();
        entry.is_open = entry.is_open || type_def.is_open;
        entry.is_enum = entry.is_enum || is_enum;
        entry.is_struct = entry.is_struct || is_struct;
        if is_struct {
            entry.fields = struct_fields;
        }
        if entry.declaring_namespace.is_empty() {
            entry.declaring_namespace = ns_str.to_string();
        }
        if alias_is_literal_union {
            entry.is_literal_union = true;
            for lit in lit_members {
                if !entry.literal_members.contains(&lit) {
                    entry.literal_members.push(lit);
                }
            }
        }
        if is_enum && !type_def.is_open {
            entry.variants = variants;
            entry.variant_payloads = payloads;
        } else {
            for v in variants {
                entry.variants.insert(v);
            }
            for (k, v) in payloads {
                entry.variant_payloads.insert(k, v);
            }
        }
    }

    /// Build a resolved `TypeExpr` from an accumulated literal-member set.
    /// Single-member sets resolve to a bare `Literal(…)`; multi-member
    /// sets to a `Union([Literal(…), …])`. Empty sets shouldn't occur in
    /// practice (an entry isn't marked `is_literal_union` until at least
    /// one member has been recorded), but for robustness we collapse the
    /// empty case to `Never`.
    fn literal_members_to_type_expr(members: &[LiteralType]) -> TypeExpr {
        match members {
            [] => TypeExpr::Never,
            [only] => TypeExpr::Literal(only.clone()),
            many => TypeExpr::Union(
                many.iter()
                    .map(|lit| TypeExpr::Literal(lit.clone()))
                    .collect(),
            ),
        }
    }

    /// Extract literal members from a type-alias `TypeExpr` if it's a
    /// pure literal union (`Literal | Literal | …`) or a single literal.
    /// Returns an empty vec for any non-literal-union shape, signalling
    /// "not a literal-union alias" to the caller.
    fn extract_literal_members(alias: &crate::lang::ast::TypeExpr) -> Vec<LiteralType> {
        use crate::lang::ast as a;
        fn convert(lit: &a::LiteralType) -> LiteralType {
            match lit {
                a::LiteralType::Str(s) => LiteralType::Str(s.clone()),
                a::LiteralType::Int(n) => LiteralType::Int(*n),
                a::LiteralType::Dec(d) => LiteralType::Dec(d.clone()),
                a::LiteralType::Bool(b) => LiteralType::Bool(*b),
                a::LiteralType::Null => LiteralType::Null,
            }
        }
        match alias {
            a::TypeExpr::Literal(lit) => vec![convert(lit)],
            a::TypeExpr::Union(members) => {
                let mut out = Vec::with_capacity(members.len());
                for m in members {
                    if let a::TypeExpr::Literal(lit) = m {
                        out.push(convert(lit));
                    } else {
                        // Mixed shape (e.g. `"a" | Int`) is not a pure
                        // literal union; bail out so we don't mark this
                        // entry as a literal-union accumulator.
                        return Vec::new();
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// Register `TypeDef`s and `TypeImplementation`s found while walking
    /// inside function/lambda/flow bodies. Unlike `register_top_level_value`,
    /// there's no paired `Var` here, so we use the type's own `name` and
    /// don't auto-import via `core: true` (locally scoped types are intended
    /// to stay local).
    fn register_nested_value(&mut self, ns_str: &str, value: &Value) {
        match value {
            Value::TypeDef(type_def) => {
                let short = type_def.name.name().to_string();
                let qualified = format!("{}/{}", ns_str, short);
                self.known_types.insert(short.clone());

                if !self.qualified_types.contains_key(&qualified) {
                    self.qualified_types
                        .insert(qualified, TypeExpr::Named(short.clone()));
                }

                // Open literal-union *extensions* must live at top level.
                // A literal-union TypeDef found inside a function body
                // that targets an existing top-level type would otherwise
                // silently extend that type from a non-top-level scope,
                // which makes the membership set hard to reason about.
                // Reject the construct with a clear diagnostic; users who
                // want a fresh local literal alias can keep it closed.
                if type_def.is_open
                    && type_def.type_alias.is_some()
                    && !Self::extract_literal_members(type_def.type_alias.as_ref().unwrap())
                        .is_empty()
                {
                    self.errors.add(CompilerError::OpenLiteralUnionMismatch {
                        type_name: short.clone(),
                        message: "open literal-union extensions must be at top level; \
                                  declare or extend the union outside any function body"
                            .to_string(),
                        location: None,
                    });
                }

                self.register_enum_metadata(&short, ns_str, type_def, None);

                if let Some(impls) = &type_def.implementations {
                    for ti in impls {
                        // Nested arrows mutate the global implementation
                        // registry, so they're a hidden global side effect
                        // (same concern as open literal-union extensions:
                        // any declaration with cross-function effects must
                        // be top-level). Reject the construct and skip
                        // registration to keep the registry clean.
                        let location = ti
                            .src
                            .as_ref()
                            .map(|src| self.source_to_error_location(src));
                        self.errors.add(CompilerError::NestedTypeImplementation {
                            source_type: ti.source_type.clone(),
                            target_type: ti.target_type.clone(),
                            location,
                        });
                    }
                }
            }
            Value::TypeImplementation(ti) => {
                let location = ti
                    .src
                    .as_ref()
                    .map(|src| self.source_to_error_location(src));
                self.errors.add(CompilerError::NestedTypeImplementation {
                    source_type: ti.source_type.clone(),
                    target_type: ti.target_type.clone(),
                    location,
                });
            }
            _ => {}
        }
    }

    /// If a `Value` is a bare literal, return its corresponding `LiteralType`
    /// wrapped in a `TypeExpr`. Used for contextual inference at call sites so
    /// that `pick-fruit("apple")` typechecks against `"apple" | "banana"` even
    /// though `"apple"` is also a valid `Str`.
    fn literal_type_of_value(value: &Value) -> Option<TypeExpr> {
        match value {
            Value::Val(val, _) => match val {
                Val::Str(s) => Some(TypeExpr::Literal(LiteralType::Str(s.to_string()))),
                Val::Int(n) => Some(TypeExpr::Literal(LiteralType::Int(*n))),
                Val::Bool(b) => Some(TypeExpr::Literal(LiteralType::Bool(*b))),
                Val::Null => Some(TypeExpr::Literal(LiteralType::Null)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Convert the parser's `ast::TypeExpr` to the type-checker's `TypeExpr`.
    /// The two enums cover the same conceptual ground but were defined
    /// independently; this bridge lets us reuse alias info that the parser
    /// has already resolved (e.g. `Fruit type "a" | "b"` arrives here as a
    /// `Union([Literal(Str("a")), Literal(Str("b"))])` and we want it as the
    /// equivalent type-checker shape).
    fn ast_type_to_type_expr(expr: &crate::lang::ast::TypeExpr) -> TypeExpr {
        use crate::lang::ast as a;
        match expr {
            a::TypeExpr::Any => TypeExpr::Any,
            a::TypeExpr::Builtin(b) => match b {
                a::BuiltinType::Str => TypeExpr::Str,
                a::BuiltinType::Int => TypeExpr::Int,
                a::BuiltinType::Dec => TypeExpr::Dec,
                a::BuiltinType::Bool => TypeExpr::Bool,
                a::BuiltinType::Null => TypeExpr::Null,
                a::BuiltinType::Vec => TypeExpr::Vec(Box::new(TypeExpr::Any)),
                a::BuiltinType::Map => {
                    TypeExpr::Map(Box::new(TypeExpr::Any), Box::new(TypeExpr::Any))
                }
            },
            a::TypeExpr::Simple(name) | a::TypeExpr::Named(name) => {
                // Stay symbolic; the resolver will look it up against
                // `qualified_types` when needed for compatibility checks.
                TypeExpr::Named(name.clone())
            }
            a::TypeExpr::Generic(name, params) => match (name.as_str(), params.as_slice()) {
                ("Vec", [inner]) => TypeExpr::Vec(Box::new(Self::ast_type_to_type_expr(inner))),
                ("Map", [k, v]) => TypeExpr::Map(
                    Box::new(Self::ast_type_to_type_expr(k)),
                    Box::new(Self::ast_type_to_type_expr(v)),
                ),
                _ => TypeExpr::Generic(
                    name.clone(),
                    params.iter().map(Self::ast_type_to_type_expr).collect(),
                ),
            },
            a::TypeExpr::Union(variants) => {
                TypeExpr::Union(variants.iter().map(Self::ast_type_to_type_expr).collect())
            }
            a::TypeExpr::Optional(inner) => {
                TypeExpr::Optional(Box::new(Self::ast_type_to_type_expr(inner)))
            }
            a::TypeExpr::Literal(lit) => TypeExpr::Literal(match lit {
                a::LiteralType::Str(s) => LiteralType::Str(s.clone()),
                a::LiteralType::Int(n) => LiteralType::Int(*n),
                a::LiteralType::Dec(d) => LiteralType::Dec(d.clone()),
                a::LiteralType::Bool(b) => LiteralType::Bool(*b),
                a::LiteralType::Null => LiteralType::Null,
            }),
        }
    }

    fn ast_type_to_type_expr_in_namespace(
        &self,
        expr: &crate::lang::ast::TypeExpr,
        ns_str: &str,
    ) -> TypeExpr {
        use crate::lang::ast as a;
        match expr {
            a::TypeExpr::Simple(name) | a::TypeExpr::Named(name) => {
                if let Some(builtin) = self.try_resolve_builtin_type(name) {
                    return builtin;
                }

                if name.starts_with("::") {
                    return self
                        .try_resolve_namespaced_type(name)
                        .unwrap_or_else(|| TypeExpr::Named(name.clone()));
                }

                let local_qualified = format!("{}/{}", ns_str, name);
                if let Some(alias_target) = self.qualified_variables.get(&local_qualified) {
                    return alias_target.clone();
                }

                if let Some(local_type) = self.qualified_types.get(&local_qualified) {
                    return local_type.clone();
                }

                if let Some(core_qualified) = self.core_types.get(name)
                    && let Some(core_type) = self.qualified_types.get(core_qualified)
                {
                    return core_type.clone();
                }

                TypeExpr::Named(name.clone())
            }
            a::TypeExpr::Generic(name, params) => match (name.as_str(), params.as_slice()) {
                ("Vec", [inner]) => TypeExpr::Vec(Box::new(
                    self.ast_type_to_type_expr_in_namespace(inner, ns_str),
                )),
                ("Map", [k, v]) => TypeExpr::Map(
                    Box::new(self.ast_type_to_type_expr_in_namespace(k, ns_str)),
                    Box::new(self.ast_type_to_type_expr_in_namespace(v, ns_str)),
                ),
                _ => TypeExpr::Generic(
                    name.clone(),
                    params
                        .iter()
                        .map(|param| self.ast_type_to_type_expr_in_namespace(param, ns_str))
                        .collect(),
                ),
            },
            a::TypeExpr::Union(variants) => TypeExpr::Union(
                variants
                    .iter()
                    .map(|variant| self.ast_type_to_type_expr_in_namespace(variant, ns_str))
                    .collect(),
            ),
            a::TypeExpr::Optional(inner) => TypeExpr::Optional(Box::new(
                self.ast_type_to_type_expr_in_namespace(inner, ns_str),
            )),
            _ => Self::ast_type_to_type_expr(expr),
        }
    }

    /// Normalize a type name to its short form (no namespace prefix). Used
    /// for user-facing diagnostic strings; the implementation registry is
    /// keyed by FQN — see [`Self::resolve_to_fqn`].
    fn short_type_name(name: &str) -> String {
        name.rsplit('/').next().unwrap_or(name).to_string()
    }

    /// Resolve a (possibly bare) type name to its fully-qualified form,
    /// preserving any `Enum.Variant` suffix. Resolution order:
    ///
    ///   1. **Already qualified** — names containing `/` are returned as-is.
    ///   2. **Dotted variant** — `Enum.Variant` resolves `Enum` recursively
    ///      and re-attaches `.Variant`, so the registry can key
    ///      `Source -> ::myapp/Shape.Tri` consistently.
    ///   3. **Core types** — short names registered in `core_types` resolve
    ///      to the namespace where the `core: true` declaration lives.
    ///   4. **Current namespace** — `format!("{}/{}", ns, name)` if the
    ///      result appears in `qualified_types` (i.e. the type was declared
    ///      in the namespace currently being checked).
    ///   5. **Built-in primitives** — `Str`, `Int`, etc. resolve to
    ///      `::hot::type/Foo`, matching the canonical names used in
    ///      `add_builtin_types`.
    ///   6. **Fallback** — the bare name. Keeps the registry keyed
    ///      consistently with the prior short-name behavior when a name
    ///      can't be resolved (test-only types, generic placeholders, etc.),
    ///      so we never silently drop a coercion arrow.
    ///
    /// `ns` is the namespace context to use for relative resolution — when
    /// resolving a payload type referenced inside a `TypeDef`, pass the
    /// type's `declaring_namespace`; when resolving a call-site argument,
    /// pass `self.context.current_namespace`.
    fn resolve_to_fqn(&self, name: &str, ns: &str) -> String {
        if name.is_empty() {
            return name.to_string();
        }
        if name.contains('/') {
            return name.to_string();
        }
        if let Some((enum_part, variant)) = name.split_once('.') {
            let enum_fqn = self.resolve_to_fqn(enum_part, ns);
            return format!("{}.{}", enum_fqn, variant);
        }
        if let Some(fqn) = self.core_types.get(name) {
            return fqn.clone();
        }
        if !ns.is_empty() {
            let candidate = format!("{}/{}", ns, name);
            if self.qualified_types.contains_key(&candidate) {
                return candidate;
            }
        }
        if Self::is_primitive_type_name(name) {
            return format!("::hot::type/{}", name);
        }
        // Fallback: keep the name as-is. This preserves correct lookups
        // for names the resolver can't fully qualify yet (e.g. generic
        // placeholders, types declared mid-file before they appear in
        // `qualified_types`) — both the writer and reader paths share
        // the same fallback so they still agree on the registry key.
        name.to_string()
    }

    /// FQN-resolve a `TypeExpr`'s short name in `ns`. Returns `None` for
    /// shapes (`Vec`/`Map`/`Function`) and `Any`-typed inputs that have no
    /// meaningful single-type identity to look up in the registry.
    fn type_expr_to_fqn(&self, expr: &TypeExpr, ns: &str) -> Option<String> {
        let short = Self::type_expr_short_name(expr)?;
        Some(self.resolve_to_fqn(&short, ns))
    }

    /// Record a `Source -> Target` coercion implementation in the registry,
    /// keyed by *fully-qualified* source/target names so two namespaces that
    /// happen to share a short name (e.g. each declares its own `Account`)
    /// register independent arrows. `ns_str` is the namespace the arrow
    /// was declared in — used to resolve bare type references via
    /// [`Self::resolve_to_fqn`].
    ///
    /// `src` is the AST source span of the new arrow, used for diagnostics
    /// when a duplicate is detected. When `is_top_level` is false (arrows
    /// declared inside function/lambda bodies), duplicates are silently
    /// merged - lexically-scoped local types in different functions can
    /// legitimately share short names like `Date -> Str` without that
    /// being an ambiguity at the use site.
    fn register_implementation(
        &mut self,
        source: &str,
        target: &str,
        src: Option<&Source>,
        ns_str: &str,
        is_top_level: bool,
    ) {
        let s = self.resolve_to_fqn(source, ns_str);
        let t = self.resolve_to_fqn(target, ns_str);
        let key = (s, t);

        let new_loc = src.map(|src| self.source_to_error_location(src));

        if !self.impl_registry.insert(key.clone()) {
            // Duplicate: arrow with this (source, target) short-name pair
            // was already registered. Build candidate strings citing both
            // declarations so the user can find them.
            let prev_loc = self.impl_locations.get(&key).and_then(|loc| loc.clone());

            // Idempotent re-registration: the type-checker pre-pass visits
            // the same top-level `Value::TypeImplementation` twice (once via
            // `register_top_level_value`, then again as `walk_values` re-
            // emits the root). If both registrations point at the same
            // source span, treat it as a no-op rather than a true duplicate.
            let same_position = match (&prev_loc, &new_loc) {
                (Some(a), Some(b)) => a.position == b.position,
                (None, None) => true,
                _ => false,
            };
            if same_position {
                return;
            }

            // Nested arrows can shadow local types and may be re-walked when
            // the same function body is processed multiple times; treat
            // duplicates as a silent no-op there.
            if !is_top_level {
                return;
            }

            let format_loc = |loc: &Option<ErrorLocation>| -> String {
                match loc {
                    Some(l) => match &l.file {
                        Some(f) => format!("{}:{}:{}", f.display(), l.line, l.column),
                        None => format!("line {}, column {}", l.line, l.column),
                    },
                    None => "<unknown location>".to_string(),
                }
            };

            let candidates = vec![
                format!("first declaration: {}", format_loc(&prev_loc)),
                format!("duplicate declaration: {}", format_loc(&new_loc)),
            ];

            // Display short names in diagnostics so users see the form
            // they wrote (`Triangle`) rather than the registry's FQN form
            // (`::myapp/Triangle`); the FQN keys still distinguish
            // collisions internally. Dotted-variant targets keep their
            // `Enum.Variant` shape — only the enum part is shortened.
            let target_short = match key.1.split_once('.') {
                Some((enum_part, variant)) => {
                    format!("{}.{}", Self::short_type_name(enum_part), variant)
                }
                None => Self::short_type_name(&key.1),
            };
            self.errors.add(CompilerError::AmbiguousTypeImplementation {
                source_type: Self::short_type_name(&key.0),
                target_type: target_short,
                candidates,
                location: new_loc,
            });
            return;
        }

        // First registration — remember its source for future diagnostics.
        self.impl_locations.insert(key.clone(), new_loc);

        // If the target is a dotted variant (`Enum.Variant`), enroll the
        // variant in the enum's known-variant set. For open enums this is
        // the primary mechanism by which new variants become visible to
        // exhaustiveness checking and to `is-type` queries. For closed
        // enums we silently ignore — variants there are fixed at the
        // `enum { ... }` declaration.
        //
        // The enum part of the FQN target needs to come back to the short
        // name to look the type up in `context.types` (which is keyed by
        // short name).
        if let Some((enum_fqn, variant_name)) = key.1.split_once('.')
            && let enum_short = Self::short_type_name(enum_fqn)
            && let Some(td) = self.context.types.get_mut(&enum_short)
            && td.is_open
        {
            td.variants.insert(variant_name.to_string());
            // Record the source as the variant's payload type so that
            // auto-unwrap on dispatch (`coercion_available`) can flow a
            // value of `key.0` into a parameter typed as `enum_name`.
            td.variant_payloads
                .entry(variant_name.to_string())
                .or_insert_with(|| key.0.clone());
        }
    }

    /// Best-effort short name for a `TypeExpr`, used for implementation lookup.
    /// Returns `None` for shapes (Vec, Map, Function, Union, ...) where a
    /// coercion check doesn't apply.
    fn type_expr_short_name(expr: &TypeExpr) -> Option<String> {
        let name = match expr {
            TypeExpr::Named(n) => n.as_str(),
            TypeExpr::Str => "Str",
            TypeExpr::Int => "Int",
            TypeExpr::Dec => "Dec",
            TypeExpr::Bool => "Bool",
            TypeExpr::Bytes => "Bytes",
            TypeExpr::Null => "Null",
            TypeExpr::Any => return None,
            TypeExpr::Optional(inner) => return Self::type_expr_short_name(inner),
            TypeExpr::Record {
                type_name: Some(name),
                ..
            } => name.as_str(),
            _ => return None,
        };
        Some(Self::short_type_name(name))
    }

    /// Whether `name` refers to one of the language's primitive types.
    /// Primitive ↔ primitive coercions are always allowed (the runtime has
    /// dedicated implementations for every pair) so we don't require a
    /// registered implementation.
    ///
    /// **Use only on trusted strings** — i.e. names produced by the compiler
    /// itself, such as the suffix of a `::hot::type/X` qualified path. For
    /// names derived from a user-written `TypeExpr` (which may have been
    /// declared in a namespace, e.g. `::myapp/Str`), use
    /// [`Self::is_primitive_type_expr`] instead — `short_type_name` would
    /// strip the namespace and falsely match a user-declared type that shares
    /// a primitive's short name.
    fn is_primitive_type_name(name: &str) -> bool {
        matches!(
            name,
            "Str" | "Int" | "Dec" | "Bool" | "Bytes" | "Byte" | "Null" | "Any" | "Vec" | "Map"
        )
    }

    /// Whether `expr` is *structurally* one of the language's primitive
    /// types. This is the safe variant of [`Self::is_primitive_type_name`]
    /// for type-inference inputs: it inspects the `TypeExpr` variant
    /// directly, so a user-declared `::myapp/Str type { ... }` (which is a
    /// `TypeExpr::Named`, not `TypeExpr::Str`) does not get misidentified as
    /// the built-in primitive `Str`.
    ///
    /// `Optional<T>` is treated as primitive iff `T` is primitive. `Union`
    /// and `Constrained` are intentionally **not** unwrapped — both can
    /// include user-declared variants, and the existing call sites that
    /// allow primitive-↔-primitive coercion already short-circuit before
    /// reaching them via `type_expr_short_name`.
    fn is_primitive_type_expr(expr: &TypeExpr) -> bool {
        match expr {
            TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Dec
            | TypeExpr::Bool
            | TypeExpr::Bytes
            | TypeExpr::Byte
            | TypeExpr::Null
            | TypeExpr::Any
            | TypeExpr::Vec(_)
            | TypeExpr::Map(_, _) => true,
            TypeExpr::Optional(inner) => Self::is_primitive_type_expr(inner),
            _ => false,
        }
    }

    /// Last-resort implicit coercion check at function boundaries: returns
    /// true if there's a directly-registered `actual -> expected` arrow
    /// (no chaining, no transitive lookup). Used by overload resolution
    /// after the normal `types_are_compatible` check fails — letting
    /// `f(triangle)` find `f fn (s: Shape)` when `Triangle -> Shape.Tri`
    /// is in scope without forcing the caller to write `Shape.Tri(triangle)`
    /// explicitly.
    ///
    /// Only direct arrows are considered: implicit coercion intentionally
    /// doesn't follow chains like `A -> B` then `B -> C`. If `A -> C`
    /// isn't declared, the call site must be explicit.
    ///
    /// Also accepts the case where the source short name is a single
    /// arrow target into the expected enum, so `f(geom_tri)` (where
    /// `geom_tri: Geom.Tri`) can flow into `f fn (s: Geom)` without
    /// requiring an explicit `f(Geom(geom_tri))`.
    ///
    /// Lookups now run on FQN keys (see `register_implementation` and
    /// `resolve_to_fqn`), so two namespaces declaring distinct types with
    /// the same short name don't share registry entries. The short-name
    /// fallback inside `resolve_to_fqn` keeps unresolvable names looking
    /// up the same way for both writers and readers.
    ///
    /// New positive branches added here must still gate on
    /// [`Self::is_primitive_type_expr`] of the original `TypeExpr` rather
    /// than `is_primitive_type_name` of the short name — a user type that
    /// happens to share a primitive's short name should not silently
    /// coerce as if it were the built-in primitive.
    fn coercion_available(&self, actual: &TypeExpr, expected: &TypeExpr) -> bool {
        // Implicit coercion is intended for user-defined types (custom
        // structs, enum variant enrollment via `Source -> Enum.Variant`,
        // etc.). Primitive ↔ primitive arrows like `Str -> Int` exist in
        // `hot-std` purely to back the *explicit* constructor calls
        // (`Int(some_str)`); allowing them to fire implicitly at any
        // call site would silently turn `add("oops", 5)` into a parse
        // attempt and defeat the whole point of static typing.
        //
        // We therefore short-circuit when both sides are structurally
        // primitive. Coercions involving any user-declared type still
        // fall through to the registry lookup below.
        if Self::is_primitive_type_expr(actual) && Self::is_primitive_type_expr(expected) {
            return false;
        }

        let ns = self.context.current_namespace.as_str();
        let Some(src_fqn) = self.type_expr_to_fqn(actual, ns) else {
            return false;
        };
        // Expected may be a dotted variant target like `Shape.Tri`; otherwise
        // resolve normally.
        let target_fqn = match expected {
            TypeExpr::Named(n) => self.resolve_to_fqn(n, ns),
            other => match self.type_expr_to_fqn(other, ns) {
                Some(s) => s,
                None => return false,
            },
        };
        if src_fqn == target_fqn {
            return false;
        }
        // Direct arrow lookup (covers both `Source -> Target` and dotted
        // variant targets like `Source -> Enum.Variant` since both are
        // stored under their resolved target FQN).
        if self
            .impl_registry
            .contains(&(src_fqn.clone(), target_fqn.clone()))
        {
            return true;
        }
        // For variant-shaped enums, also accept `actual -> Enum` when the
        // target is `Enum.Variant` (so a value typed as the variant input
        // can flow into an enum-typed parameter without explicit wrapping).
        if let Some((enum_fqn, _variant)) = target_fqn.split_once('.')
            && self
                .impl_registry
                .contains(&(src_fqn.clone(), enum_fqn.to_string()))
        {
            return true;
        }
        // Variant-to-enum: if `src` is `Foo.Bar` and `expected` is `Foo`
        // (or any enum that already declares `Bar` as a variant), accept
        // the call. The runtime stores variants and enums in the same
        // tagged-map shape so no actual conversion is needed.
        // The enum lookup goes through `context.types` which is keyed by
        // short name, so we shorten just for that probe.
        if let Some((src_enum_fqn, src_variant)) = src_fqn.split_once('.')
            && src_enum_fqn == target_fqn
            && let Some(td) = self.context.types.get(&Self::short_type_name(src_enum_fqn))
            && (td.is_open || td.variants.contains(src_variant))
        {
            return true;
        }
        // Payload-to-enum: if `expected` is an enum and `src` is the
        // declared payload type of one of its variants, accept the call.
        // The runtime applies the variant's wrap function at the
        // `CallUserFunction` opcode boundary (see `lookup_typed_param_signature`
        // and `apply_coercion_plan` in `vm.rs`) so the callee receives
        // the properly tagged enum value. Test coverage:
        // `hot/test/docs/coercion-implicit.hot::test-runtime-coerces-to-variant`.
        //
        // Payloads in `variant_payloads` are stored as the user wrote them
        // (often bare); resolve each through the enum's *own* declaring
        // namespace so a type defined in the same module as the enum
        // matches even when `coercion_available` is called from a
        // different namespace.
        let target_enum_short = Self::short_type_name(&target_fqn);
        if let Some(td) = self.context.types.get(&target_enum_short)
            && td.is_enum
        {
            let enum_ns = td.declaring_namespace.as_str();
            if td
                .variant_payloads
                .values()
                .any(|payload| self.resolve_to_fqn(payload, enum_ns) == src_fqn)
            {
                return true;
            }
        }
        false
    }

    fn check_namespace(&mut self, namespace: &Namespace) -> CompilerResult<()> {
        let ns_path = namespace.path.to_string();
        // Reset the global scope so previous namespaces' top-level bindings
        // can't leak in via short-name lookup. Cross-namespace access still
        // works through `qualified_variables` and the core variable registry.
        self.context.enter_namespace(ns_path.clone());

        // Snapshot namespace aliases declared at the top of this namespace so we can
        // resolve qualified function calls written via the alias form (e.g. `::ctx/get`
        // when `::ctx ::hot::ctx` was declared).
        let mut alias_map: AHashMap<String, String> = AHashMap::new();
        for (alias, target) in &namespace.aliases {
            alias_map.insert(alias.to_string(), target.to_string());
        }
        self.context.set_current_aliases(alias_map);

        // First pass: collect function signatures for all arities.
        //
        // A function may be defined in two ways that both produce multiple arities:
        //   1. Comma-joined arities in a single `fn` form → Value::Fn(Vec<FnDef>).
        //   2. Repeated top-level `name fn (...) { ... }` declarations → the parser
        //      collapses these into a Value::MultipleValues containing several
        //      Value::Fn entries (interleaved with self-Var refs).
        // Both shapes need to be merged so arity dispatch works.
        // Aggregate signatures by qualified name first. The parser may emit the
        // same logical function as several distinct `Var` entries (different
        // `src` positions hash differently in `IndexMap<Var, Value>`), so a
        // single qualified name can map to multiple `(var, value)` pairs that
        // each contribute one or more arities. We collect them all before
        // registering so later entries don't clobber earlier ones.
        let mut sigs_by_name: ahash::AHashMap<String, Vec<FunctionSignature>> =
            ahash::AHashMap::new();

        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.name().to_string();
            let qualified_name = format!("{}/{}", ns_path, var_name);

            let mut all_fn_defs: Vec<&crate::lang::ast::FnDef> = Vec::new();
            collect_fn_defs(value, &mut all_fn_defs);

            if all_fn_defs.is_empty() {
                continue;
            }

            let entry = sigs_by_name.entry(qualified_name).or_default();
            for fn_def in all_fn_defs {
                let mut parameter_types = Vec::new();
                for arg in &fn_def.args.args {
                    if let Some(type_annotation) = &arg.type_annotation {
                        parameter_types.push(self.parse_type_annotation(type_annotation, None));
                    } else {
                        parameter_types.push(TypeExpr::Any);
                    }
                }

                entry.push(FunctionSignature {
                    return_type: TypeExpr::Any,
                    parameter_types,
                    variadic: fn_def.args.variadic,
                });
            }
        }

        for (qualified_name, signatures) in sigs_by_name {
            self.context
                .replace_function_signatures(qualified_name, signatures);
        }

        // Second pass: infer types for all variables
        for (var, value) in &namespace.scope.vars {
            let var_name = var.sym.name().to_string();

            // Track the qualified name of the definition whose body we're about
            // to walk so the deprecation check can suppress self-references.
            self.current_def = Some(format!("{}/{}", ns_path, var_name));
            self.current_def_src = var.src.clone();
            let inferred_type = self.infer_value_type(value);
            let inferred_type = if let Some(annotation) = &var.type_annotation {
                let constraint = self.parse_type_annotation(annotation, var.src.as_ref());
                self.warn_annotation_mismatch(
                    &var_name,
                    annotation,
                    &constraint,
                    &inferred_type,
                    var.src.as_ref(),
                );
                self.apply_annotation_constraint(inferred_type, &constraint)
            } else {
                inferred_type
            };
            self.current_def = None;
            self.current_def_src = None;
            self.context.add_variable(var_name, inferred_type);
        }

        Ok(())
    }

    /// Infer the type of a Value
    fn infer_value_type(&mut self, value: &Value) -> TypeExpr {
        match value {
            Value::Val(val, type_info) => {
                if let Some(ann) = type_info {
                    if ann == "Map" && matches!(val, Val::Map(_)) {
                        return self.infer_val_type(val);
                    }
                    self.parse_type_annotation(ann, None)
                } else {
                    self.infer_val_type(val)
                }
            }

            Value::Ref(hot_ref) => match hot_ref {
                Ref::Var(var_ref) => {
                    let var_name = &var_ref.var.sym.name();
                    let base = self
                        .context
                        .get_variable_type(var_name)
                        .cloned()
                        // Fall back to the core variable registry so
                        // auto-imported core symbols (declared with
                        // `meta {core: true}` in another namespace) resolve to
                        // their qualified type instead of `Any`. If multiple
                        // namespaces declare the same core name, take the
                        // first registered match — short-name variable
                        // shadowing across modules is rare; ambiguous cases
                        // would be flagged by the resolver pass.
                        .or_else(|| {
                            self.core_variables
                                .get(*var_name)
                                .and_then(|qualifieds| qualifieds.first())
                                .and_then(|qualified| {
                                    self.qualified_variables.get(qualified).cloned()
                                })
                        })
                        .unwrap_or(TypeExpr::Any);
                    if let Some(path) = &var_ref.var.deep_path {
                        let location = var_ref
                            .src
                            .as_ref()
                            .or(var_ref.var.src.as_ref())
                            .map(|s| self.source_to_error_location(s));
                        self.resolve_deep_path_type(&base, path, location.as_ref())
                    } else {
                        base
                    }
                }
                Ref::Ns(ns_ref) => {
                    // Resolve `::alias/name` against the current namespace
                    // alias map, then look up the qualified variable type
                    // populated during `collect_qualified_variables`.
                    self.lookup_qualified_ns_ref(ns_ref, self.context.current_aliases_ref())
                        .unwrap_or(TypeExpr::Any)
                }
            },

            Value::FnCall(fn_call) => self.infer_function_call_type(fn_call),

            Value::Flow(flow) => self.infer_flow_type(flow),

            Value::Fn(fn_defs) => {
                // Function definitions - infer function type from signature
                self.infer_function_definition_type(fn_defs)
            }

            Value::TemplateLiteral(_) => TypeExpr::Str,

            Value::TypeDef(type_def) => {
                // Process type definition and validate generic parameters
                self.process_type_definition(type_def)
            }

            Value::TypeImplementation(type_impl) => {
                // Process type implementation and validate it
                self.process_type_implementation(type_impl)
            }

            Value::Lambda(lambda) => {
                // Walk the lambda body for type-checking side effects, with the
                // lambda's parameters bound in a fresh lexical scope.
                let mut parameter_types = Vec::new();
                self.context.push_scope();
                for arg in &lambda.args.args {
                    let param_type = if let Some(ann) = &arg.type_annotation {
                        self.parse_type_annotation(ann, None)
                    } else {
                        TypeExpr::Any
                    };
                    parameter_types.push(param_type.clone());
                    let name = arg.var.sym.name().to_string();
                    self.context.add_variable(name, param_type);
                }

                self.in_fn_body += 1;
                let saved_pipe = self.in_pipe_flow;
                self.in_pipe_flow = false;
                let _body_type = self.infer_value_type(&lambda.body);
                self.in_pipe_flow = saved_pipe;
                self.in_fn_body -= 1;

                self.context.pop_scope();

                TypeExpr::Function(parameter_types, Box::new(TypeExpr::Any))
            }

            Value::Cond(_label, predicate, body) => {
                // A cond branch: walk both predicate and body for side-effect checks.
                let _ = self.infer_value_type(predicate);
                self.infer_flow_type(body)
            }

            Value::CondDefault(body) => self.infer_flow_type(body),

            Value::Match(match_expr) => {
                let subject_type = self.infer_value_type(&match_expr.value);
                let mut last_type = TypeExpr::Any;
                for arm in &match_expr.arms {
                    last_type = self.infer_value_type(&arm.body);
                }
                if !match_expr.match_all {
                    let arm_refs: Vec<&MatchArm> = match_expr.arms.iter().collect();
                    let location = match_expr
                        .src
                        .as_ref()
                        .map(|s| self.source_to_error_location(s));
                    self.check_match_exhaustiveness_for(&subject_type, &arm_refs, location);
                }
                if match_expr.match_all {
                    TypeExpr::Map(Box::new(TypeExpr::Str), Box::new(TypeExpr::Any))
                } else {
                    last_type
                }
            }

            Value::MultipleValues(values) => {
                // `MultipleValues` covers two distinct cases:
                //   (a) Local variable assignment inside a function body, where
                //       the parser emits `[Value::Ref(Var{name,..}), expr]`.
                //       Without binding `name → infer(expr)`, every later
                //       reference to the local would resolve to `Any` and we'd
                //       miss type errors on the other side of the assignment.
                //   (b) Multi-arity declarations / repeated top-level
                //       assignments — just walk each value for side-effects.
                if values.len() == 2
                    && let Value::Ref(Ref::Var(var_ref)) = &values[0]
                    && var_ref.var.deep_path.is_none()
                    && var_ref.var.deep_set.is_none()
                {
                    let inferred = self.infer_value_type(&values[1]);
                    let assigned = if let Some(ann) = var_ref.var.type_annotation.as_ref() {
                        let constraint = self.parse_type_annotation(ann, None);
                        self.warn_annotation_mismatch(
                            var_ref.var.sym.name(),
                            ann,
                            &constraint,
                            &inferred,
                            var_ref.src.as_ref().or(var_ref.var.src.as_ref()),
                        );
                        self.apply_annotation_constraint(inferred, &constraint)
                    } else {
                        inferred
                    };
                    let name = var_ref.var.sym.name().to_string();
                    self.context.add_variable(name, assigned.clone());
                    assigned
                } else {
                    let mut last_type = TypeExpr::Any;
                    for v in values {
                        last_type = self.infer_value_type(v);
                    }
                    last_type
                }
            }

            // Other value types default to Any for now
            Value::MapWithSpread { .. } => {
                TypeExpr::Map(Box::new(TypeExpr::Any), Box::new(TypeExpr::Any))
            }
            _ => TypeExpr::Any,
        }
    }

    /// Infer the type of a Val
    #[allow(clippy::only_used_in_recursion)]
    fn infer_val_type(&self, val: &Val) -> TypeExpr {
        match val {
            Val::Null => TypeExpr::Null,
            Val::Bool(_) => TypeExpr::Bool,
            Val::Int(_) => TypeExpr::Int,
            Val::Dec(_) => TypeExpr::Dec,
            Val::Str(_) => TypeExpr::Str,
            Val::Byte(_) => TypeExpr::Byte,
            Val::Bytes(_) => TypeExpr::Bytes,
            Val::Vec(elements) => {
                if elements.is_empty() {
                    TypeExpr::Vec(Box::new(TypeExpr::Any))
                } else {
                    let element_type = self.infer_val_type(&elements[0]);
                    TypeExpr::Vec(Box::new(element_type))
                }
            }
            Val::Map(_) => self.infer_record_type(val),
            Val::Box(_) => TypeExpr::Any, // Boxed values are dynamic
        }
    }

    fn infer_record_type(&self, val: &Val) -> TypeExpr {
        let Val::Map(map) = val else {
            return TypeExpr::Map(Box::new(TypeExpr::Any), Box::new(TypeExpr::Any));
        };

        let mut fields = AHashMap::new();
        for (key, value) in map.iter() {
            let Val::Str(field_name) = key else {
                return TypeExpr::Map(Box::new(TypeExpr::Any), Box::new(TypeExpr::Any));
            };
            fields.insert(field_name.to_string(), self.infer_val_type(value));
        }

        TypeExpr::Record {
            fields,
            open: false,
            type_name: None,
        }
    }

    /// True when a type is definite enough to participate in the
    /// annotation-mismatch warning: a concrete primitive, literal, or
    /// container. `Any`, unresolved `Named` types, functions, and other
    /// inference-weak shapes never participate, so the check stays
    /// quiet at the cost of missing some mismatches.
    fn type_is_definite(t: &TypeExpr) -> bool {
        match t {
            TypeExpr::Null
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Dec
            | TypeExpr::Str
            | TypeExpr::Byte
            | TypeExpr::Bytes
            | TypeExpr::Literal(_)
            | TypeExpr::Vec(_)
            | TypeExpr::Map(_, _)
            | TypeExpr::Record { .. } => true,
            TypeExpr::Optional(inner) => Self::type_is_definite(inner),
            _ => false,
        }
    }

    /// Warn when an annotation names a type the value can never be
    /// (`annotation-mismatch`). Annotations are not enforced at runtime,
    /// so a definite mismatch means either the annotation or the value is
    /// wrong. Conservative by construction: both sides must be definite
    /// (see `type_is_definite`), a union warns only when *no* member is
    /// compatible, and `All<...>` flow-shape annotations are exempt (the
    /// inferred type already reflects the collected shape they describe).
    fn warn_annotation_mismatch(
        &mut self,
        name: &str,
        annotation: &str,
        constraint: &TypeExpr,
        inferred: &TypeExpr,
        src: Option<&Source>,
    ) {
        let base = annotation.split('<').next().unwrap_or(annotation).trim();
        if base == "All" || base.ends_with("/All") {
            return;
        }
        if !Self::type_is_definite(constraint) {
            return;
        }
        let compatible = match inferred {
            TypeExpr::Union(members) => members
                .iter()
                .any(|m| !Self::type_is_definite(m) || self.types_are_compatible(constraint, m)),
            _ => {
                if !Self::type_is_definite(inferred) {
                    return;
                }
                self.types_are_compatible(constraint, inferred)
            }
        };
        if compatible {
            return;
        }
        let location = src.map(|s| self.source_to_error_location(s));
        self.errors.add_warning(CompilerError::AnnotationMismatch {
            name: name.to_string(),
            annotation: annotation.to_string(),
            inferred: inferred.to_string(),
            location,
        });
    }

    fn apply_annotation_constraint(&self, actual: TypeExpr, constraint: &TypeExpr) -> TypeExpr {
        if matches!(constraint, TypeExpr::Map(_, _)) {
            return constraint.clone();
        }
        match &actual {
            TypeExpr::Record { .. } if self.types_are_compatible(constraint, &actual) => {
                self.record_with_constraint(actual, constraint)
            }
            TypeExpr::Any | TypeExpr::Map(_, _) => constraint.clone(),
            _ => constraint.clone(),
        }
    }

    fn apply_annotation_constraint_pure(
        &self,
        actual: TypeExpr,
        annotation: Option<&str>,
    ) -> TypeExpr {
        let Some(annotation) = annotation else {
            return actual;
        };
        let constraint = self.parse_type_annotation_pure(annotation);
        if matches!(constraint, TypeExpr::Map(_, _)) {
            return constraint;
        }
        match &actual {
            TypeExpr::Record { .. } if self.types_are_compatible(&constraint, &actual) => {
                self.record_with_constraint(actual, &constraint)
            }
            TypeExpr::Any | TypeExpr::Map(_, _) => constraint,
            _ => constraint,
        }
    }

    fn record_with_constraint(&self, actual: TypeExpr, constraint: &TypeExpr) -> TypeExpr {
        let TypeExpr::Record {
            fields,
            open,
            type_name,
        } = actual
        else {
            return actual;
        };

        let constrained_name = match constraint {
            TypeExpr::Named(name) => Some(name.clone()),
            _ => type_name,
        };

        TypeExpr::Record {
            fields,
            open,
            type_name: constrained_name,
        }
    }

    fn named_type_definition(&self, name: &str) -> Option<&TypeDefinition> {
        let short = Self::short_type_name(name);
        self.context
            .types
            .get(name)
            .or_else(|| self.context.types.get(&short))
    }

    pub fn known_fields_for_type(&self, base: &TypeExpr) -> Option<AHashMap<String, TypeExpr>> {
        match base {
            TypeExpr::Record { fields, .. } => Some(fields.clone()),
            TypeExpr::Named(name) => self
                .named_type_definition(name)
                .filter(|td| td.is_struct)
                .map(|td| td.fields.clone()),
            TypeExpr::Optional(inner) => self.known_fields_for_type(inner),
            _ => None,
        }
    }

    /// Refine a type by walking a deep-path access (e.g. `event.data.fn` or
    /// `items[0]`).
    ///
    /// We only know the static structure of generic containers (`Vec<T>`,
    /// `Map<K, V>`); for everything else we conservatively return `Any` so the
    /// access succeeds without a false-positive type error. This is also the
    /// right answer for `Map<Any, Any>` — every key access returns `Any`.
    fn resolve_deep_path_type(
        &mut self,
        base: &TypeExpr,
        path: &crate::lang::ast::DeepPath,
        location: Option<&ErrorLocation>,
    ) -> TypeExpr {
        use crate::lang::ast::DeepPath;
        match path {
            DeepPath::Index(_) | DeepPath::DynamicIndex(_) => self.element_type(base),
            DeepPath::Key(key) => self.value_type(base, key, location),
            DeepPath::Append => base.clone(),
            DeepPath::Chain(head, tail) => {
                let head_type = self.resolve_deep_path_type(base, head, location);
                self.resolve_deep_path_type(&head_type, tail, location)
            }
        }
    }

    /// Element type for an indexed access. `Vec<T>` → `T`, anything else →
    /// `Any` (we don't model tuples or struct field access yet).
    fn element_type(&self, base: &TypeExpr) -> TypeExpr {
        match base {
            TypeExpr::Vec(inner) => (**inner).clone(),
            TypeExpr::Optional(inner) => self.element_type(inner),
            TypeExpr::Union(variants) => {
                let resolved: Vec<_> = variants.iter().map(|v| self.element_type(v)).collect();
                TypeExpr::Union(resolved)
            }
            _ => TypeExpr::Any,
        }
    }

    fn runtime_metadata_field_type(key: &str) -> Option<TypeExpr> {
        match key {
            "$type" => Some(TypeExpr::Str),
            "$val" => Some(TypeExpr::Any),
            _ => None,
        }
    }

    /// Value type for a keyed access. `Map<K, V>` → `V`; closed known
    /// records and struct types expose their declared/preserved fields.
    fn value_type(
        &mut self,
        base: &TypeExpr,
        key: &str,
        location: Option<&ErrorLocation>,
    ) -> TypeExpr {
        if let Some(metadata_type) = Self::runtime_metadata_field_type(key) {
            return metadata_type;
        }

        match base {
            TypeExpr::Map(_, val) => (**val).clone(),
            TypeExpr::Record { fields, open, .. } => {
                if let Some(field_type) = fields.get(key) {
                    field_type.clone()
                } else {
                    if !open {
                        self.add_unknown_field_warning("<record>", key, location.cloned());
                    }
                    TypeExpr::Any
                }
            }
            TypeExpr::Named(name) => {
                if let Some(type_def) = self.named_type_definition(name) {
                    if type_def.is_enum && (type_def.is_open || type_def.variants.contains(key)) {
                        return base.clone();
                    }

                    if type_def.is_struct {
                        if let Some(field_type) = type_def.fields.get(key) {
                            return field_type.clone();
                        }
                        self.add_unknown_field_warning(name, key, location.cloned());
                    }
                }
                TypeExpr::Any
            }
            TypeExpr::Optional(inner) => {
                let inner_type = self.value_type(inner, key, location);
                TypeExpr::Optional(Box::new(inner_type))
            }
            TypeExpr::Union(variants) => {
                if variants
                    .iter()
                    .any(|v| matches!(v, TypeExpr::Any | TypeExpr::Map(_, _)))
                {
                    return TypeExpr::Any;
                }
                let resolved: Vec<_> = variants
                    .iter()
                    .map(|v| self.value_type(v, key, location))
                    .collect();
                TypeExpr::Union(resolved)
            }
            _ => TypeExpr::Any,
        }
    }

    fn add_unknown_field_warning(
        &mut self,
        type_name: &str,
        field_name: &str,
        location: Option<ErrorLocation>,
    ) {
        self.errors.add_warning(CompilerError::UnknownField {
            type_name: Self::short_type_name(type_name),
            field_name: field_name.to_string(),
            location,
        });
    }

    /// Check if two types are compatible (for assignment, parameter passing, etc.)
    #[allow(clippy::only_used_in_recursion)]
    pub fn types_are_compatible(&self, expected: &TypeExpr, actual: &TypeExpr) -> bool {
        match (expected, actual) {
            // Any is compatible with everything
            (TypeExpr::Any, _) | (_, TypeExpr::Any) => true,

            // Exact matches
            (TypeExpr::Null, TypeExpr::Null) => true,
            (TypeExpr::Bool, TypeExpr::Bool) => true,
            (TypeExpr::Int, TypeExpr::Int) => true,
            (TypeExpr::Dec, TypeExpr::Dec) => true,
            (TypeExpr::Str, TypeExpr::Str) => true,
            (TypeExpr::Byte, TypeExpr::Byte) => true,
            (TypeExpr::Bytes, TypeExpr::Bytes) => true,

            // Numeric compatibility: Dec can accept Int
            (TypeExpr::Dec, TypeExpr::Int) => true,

            // Literal type compatibility
            // Two literals match if they have the same value
            (TypeExpr::Literal(expected_lit), TypeExpr::Literal(actual_lit)) => {
                expected_lit == actual_lit
            }

            // Literal types are subtypes of their base types
            // e.g., "apple" is compatible with Str, 42 is compatible with Int
            (TypeExpr::Str, TypeExpr::Literal(LiteralType::Str(_))) => true,
            (TypeExpr::Int, TypeExpr::Literal(LiteralType::Int(_))) => true,
            (TypeExpr::Dec, TypeExpr::Literal(LiteralType::Dec(_))) => true,
            (TypeExpr::Dec, TypeExpr::Literal(LiteralType::Int(_))) => true, // Int literals also compatible with Dec
            (TypeExpr::Bool, TypeExpr::Literal(LiteralType::Bool(_))) => true,
            (TypeExpr::Null, TypeExpr::Literal(LiteralType::Null)) => true,

            // Optional types: Optional<T> accepts T, Null, or another Optional
            // whose inner type is compatible. The "Optional vs Optional" case
            // matters in practice: passing a `Str?` parameter to another `Str?`
            // parameter is the canonical pattern for forwarded nullable args
            // and was previously rejected ("expected Str?, got Str?").
            (TypeExpr::Optional(expected_inner), TypeExpr::Optional(actual_inner)) => {
                self.types_are_compatible(expected_inner, actual_inner)
            }
            (TypeExpr::Optional(inner), actual) => {
                self.types_are_compatible(inner, actual) || matches!(actual, TypeExpr::Null)
            }
            (expected, TypeExpr::Optional(inner)) => {
                // Anything that already accepts Null can absorb an Optional;
                // otherwise an Optional<T> can flow into a T parameter only
                // when the caller has guaranteed non-nullness elsewhere. We
                // unwrap and check the inner — accepting the call lets us
                // catch real type errors (Optional<Int> into Str) without
                // blocking the common `if-not-null then forward` pattern.
                self.types_are_compatible(expected, inner)
            }

            // Vec compatibility
            (TypeExpr::Vec(expected_elem), TypeExpr::Vec(actual_elem)) => {
                // If expected element is Any, accept any Vec type
                if matches!(**expected_elem, TypeExpr::Any) {
                    return true;
                }
                self.types_are_compatible(expected_elem, actual_elem)
            }

            // Map compatibility
            (TypeExpr::Map(expected_key, expected_val), TypeExpr::Map(actual_key, actual_val)) => {
                // If expected key/value are Any, accept any Map type
                if matches!(**expected_key, TypeExpr::Any)
                    && matches!(**expected_val, TypeExpr::Any)
                {
                    return true;
                }
                self.types_are_compatible(expected_key, actual_key)
                    && self.types_are_compatible(expected_val, actual_val)
            }

            // Known records can flow to Map slots. Field key types are strings,
            // and values are accepted when every known value fits the map's
            // declared value type (Map<Any, Any> accepts all records).
            (TypeExpr::Map(expected_key, expected_val), TypeExpr::Record { fields, .. }) => {
                self.types_are_compatible(expected_key, &TypeExpr::Str)
                    && (matches!(**expected_val, TypeExpr::Any)
                        || fields
                            .values()
                            .all(|field_type| self.types_are_compatible(expected_val, field_type)))
            }
            (TypeExpr::Record { .. }, TypeExpr::Map(k, v))
                if matches!(**k, TypeExpr::Any) && matches!(**v, TypeExpr::Any) =>
            {
                true
            }

            // Struct constraints are structural: a known record satisfies a
            // struct when it provides every declared field with a compatible
            // type. Extra record fields are deliberately preserved elsewhere.
            (TypeExpr::Named(expected_name), TypeExpr::Record { fields, .. }) => {
                if let Some(type_def) = self.named_type_definition(expected_name)
                    && type_def.is_struct
                {
                    type_def.fields.iter().all(|(field_name, expected_type)| {
                        fields.get(field_name).is_some_and(|actual_type| {
                            self.types_are_compatible(expected_type, actual_type)
                        })
                    })
                } else {
                    false
                }
            }
            (
                TypeExpr::Record {
                    fields: expected_fields,
                    ..
                },
                TypeExpr::Record { fields, .. },
            ) => expected_fields.iter().all(|(field_name, expected_type)| {
                fields.get(field_name).is_some_and(|actual_type| {
                    self.types_are_compatible(expected_type, actual_type)
                })
            }),

            // Map literal `{...}` infers as `Map<Any, Any>`. Without further
            // annotation we have no way to verify it satisfies a struct shape
            // like `BoxConf` or `HttpRequestOpts`, so allow the call through —
            // mismatches will surface at runtime when the field accessors fail.
            // The reverse direction (Named → Map<Any, Any>) covers wrappers
            // like `::hot::store/Map` whose declared signature is
            // intentionally a Map but whose inferred type is a Named alias.
            (TypeExpr::Named(_), TypeExpr::Map(k, v))
            | (TypeExpr::Map(k, v), TypeExpr::Named(_))
                if matches!(**k, TypeExpr::Any) && matches!(**v, TypeExpr::Any) =>
            {
                true
            }

            // Vec<Any> behaves the same way: an unannotated `[...]` infers as
            // `Vec<Any>` and we should let it flow into Named container types
            // (and vice versa) without complaint.
            (TypeExpr::Named(_), TypeExpr::Vec(elem))
            | (TypeExpr::Vec(elem), TypeExpr::Named(_))
                if matches!(**elem, TypeExpr::Any) =>
            {
                true
            }

            // Literal types are compatible with their underlying primitive.
            // Literal unions (`Fruit = "apple" | "banana"`, `HttpMethod = "GET"
            // | ...`) need to accept any Str/Int value because we can't
            // statically prove what string a variable holds at the call site.
            (TypeExpr::Literal(LiteralType::Str(_)), TypeExpr::Str) => true,
            (TypeExpr::Literal(LiteralType::Int(_)), TypeExpr::Int) => true,
            (TypeExpr::Literal(LiteralType::Dec(_)), TypeExpr::Dec) => true,
            (TypeExpr::Literal(LiteralType::Bool(_)), TypeExpr::Bool) => true,

            // Union types: the actual type is compatible with the expected union
            // if it matches any variant.  Standard subtyping rule.
            //
            // The dual case (actual is itself a union) needs to come FIRST so we
            // can split the actual union and check each variant against the
            // expected. Without this, `Union<Int, Dec>` is not even compatible
            // with itself, because the `(Union, _)` rule below would test
            // `compat(Int, Union<Int, Dec>)` and `compat(Dec, Union<Int, Dec>)`
            // — neither side has a rule for `(non-union, Union(_))`.
            (expected, TypeExpr::Union(actual_variants)) => actual_variants
                .iter()
                .all(|variant| self.types_are_compatible(expected, variant)),

            (TypeExpr::Union(variants), actual) => variants
                .iter()
                .any(|variant| self.types_are_compatible(variant, actual)),

            // Named type "Fn" is compatible with any function type
            (TypeExpr::Named(name), TypeExpr::Function(_, _)) if name == "Fn" => true,

            // Type constructors (`::hot::type/Str`, `::hot::type/Int`, …)
            // double as coercion functions. When we infer one of these as a
            // value (e.g. passing `Str` as the second arg to `map-vals`), it
            // should satisfy a `Fn` parameter the same way a lambda would.
            // We accept any registered Named type here because user-defined
            // types are also callable via their constructor.
            (TypeExpr::Named(name), TypeExpr::Named(actual))
                if name == "Fn"
                    && (actual == "Fn"
                        || actual.starts_with("::hot::type/")
                        || self.qualified_types.contains_key(actual)
                        || self.context.types.contains_key(actual)
                        || self.core_types.contains_key(actual)) =>
            {
                true
            }

            // Function types - check parameter and return type compatibility
            (
                TypeExpr::Function(expected_params, expected_return),
                TypeExpr::Function(actual_params, actual_return),
            ) => {
                // Check parameter count matches
                if expected_params.len() != actual_params.len() {
                    return false;
                }
                // Check all parameter types are compatible (contravariant - actual can accept broader types)
                let params_compatible = expected_params
                    .iter()
                    .zip(actual_params.iter())
                    .all(|(exp, act)| self.types_are_compatible(act, exp));
                // Check return type is compatible (covariant - actual can return narrower type)
                let return_compatible = self.types_are_compatible(expected_return, actual_return);
                params_compatible && return_compatible
            }

            // Named types — a constructor like `Xml({...})` infers as
            // `Named("Xml")` (short), while a function annotation like
            // `: ::hot::xml/Xml` lands as `Named("::hot::xml/Xml")` (qualified).
            // They denote the same type, so accept short ↔ qualified matches
            // when one form's tail equals the other.
            (TypeExpr::Named(expected_name), TypeExpr::Named(actual_name)) => {
                if expected_name == actual_name {
                    return true;
                }
                let expected_short = expected_name.rsplit('/').next().unwrap_or(expected_name);
                let actual_short = actual_name.rsplit('/').next().unwrap_or(actual_name);
                expected_short == actual_short
            }

            // Generic types (simplified - check base type and parameters)
            (
                TypeExpr::Generic(expected_name, expected_params),
                TypeExpr::Generic(actual_name, actual_params),
            ) => {
                expected_name == actual_name
                    && expected_params.len() == actual_params.len()
                    && expected_params
                        .iter()
                        .zip(actual_params.iter())
                        .all(|(exp, act)| self.types_are_compatible(exp, act))
            }

            // Lazy types: lazy T is compatible with T
            (TypeExpr::Lazy(inner), actual) => self.types_are_compatible(inner, actual),
            (expected, TypeExpr::Lazy(inner)) => self.types_are_compatible(expected, inner),

            // Variadic types: ...T is compatible with Vec<T>
            (TypeExpr::Variadic(inner), TypeExpr::Vec(vec_inner)) => {
                self.types_are_compatible(inner, vec_inner)
            }
            (TypeExpr::Vec(vec_inner), TypeExpr::Variadic(inner)) => {
                self.types_are_compatible(vec_inner, inner)
            }

            // Namespace types (always compatible for now)
            (TypeExpr::Namespace(_), TypeExpr::Namespace(_)) => true,

            // Type references (simplified)
            (TypeExpr::TypeRef(expected_ref), TypeExpr::TypeRef(actual_ref)) => {
                expected_ref == actual_ref
            }

            // Constrained types: check the inner type
            (TypeExpr::Constrained(inner, _), actual) => self.types_are_compatible(inner, actual),
            (expected, TypeExpr::Constrained(inner, _)) => {
                self.types_are_compatible(expected, inner)
            }

            // Never type is compatible with nothing (except Any)
            (TypeExpr::Never, _) | (_, TypeExpr::Never) => false,

            // Default: incompatible
            _ => false,
        }
    }

    /// Extract function name from a function call
    fn extract_function_name(&self, fn_call: &FnCall) -> Option<String> {
        match &*fn_call.function {
            Value::Ref(Ref::Var(var_ref)) => Some(var_ref.var.sym.name().to_string()),
            Value::Ref(Ref::Ns(ns_ref)) => {
                // For namespace references, construct the full qualified name
                let ns_str = ns_ref
                    .ns
                    .0
                    .iter()
                    .map(|part| match part {
                        crate::lang::ast::NsPathPart::Sym(sym) => sym.name(),
                    })
                    .collect::<Vec<_>>()
                    .join("::");
                if let Some(function_name) = &ns_ref.function_name {
                    // Function signatures are registered with `/` between namespace
                    // and function name (e.g. `::hot::ctx/get`), matching Hot's
                    // surface syntax. Use the same separator here so lookup hits.
                    Some(format!("::{}/{}", ns_str, function_name))
                } else {
                    Some(format!("::{}", ns_str))
                }
            }
            _ => None,
        }
    }

    /// Infer the return type of a function call
    fn infer_function_call_type(&mut self, fn_call: &FnCall) -> TypeExpr {
        // Check for type conversion calls like `::hot::type/Str(value)`. These
        // are constructor calls that double as coercion functions; if the
        // source type doesn't have a registered `SourceType -> TargetType`
        // implementation, the call should fail with a `MissingTypeImplementation`
        // error rather than silently coercing.
        if let Some(func_name) = self.extract_function_name(fn_call)
            && let Some(target_type) = func_name.strip_prefix("::hot::type/")
            // Only treat `::hot::type/X` as a coercion when X is actually a
            // registered type. The same namespace also exposes plain helper
            // functions like `is-type` and `is-str` whose names start lowercase
            // — those should fall through to normal function-signature lookup.
            && (Self::is_primitive_type_name(target_type)
                || self.qualified_types.contains_key(&func_name))
            && let Some(first_arg) = fn_call.args.first()
        {
            let source_type = self.infer_value_type(&first_arg.value);

            // Resolve the source type to a short name we can look up in the
            // implementation registry. For shapes like Vec/Map/Function or an
            // unresolved `Any` we skip the check entirely — there's no
            // meaningful source type to complain about.
            if let Some(source_short) = Self::type_expr_short_name(&source_type) {
                let target_short = Self::short_type_name(target_type);
                // FQN-resolved keys for the registry probe — short names
                // are kept for the diagnostic payload below so users see
                // the form they wrote.
                let ns = self.context.current_namespace.as_str();
                let source_fqn = self.resolve_to_fqn(&source_short, ns);
                // `target_type` here is the suffix after `::hot::type/`, so
                // its FQN is exactly that path. Reusing `resolve_to_fqn`
                // would re-prefix correctly via the primitive branch.
                let target_fqn = format!("::hot::type/{}", target_short);

                // Identity coercion (Str(some_str)) and primitive ↔ primitive
                // coercions are always allowed; the runtime has built-in
                // conversions for every primitive pair without an explicit
                // implementation declaration.
                //
                // Both checks are guarded on `is_primitive_type_expr(&source_type)`
                // rather than `is_primitive_type_name(&source_short)` so a
                // user-declared type whose short name shadows a primitive
                // (e.g. `::myapp/Str type { ... }`) is *not* silently treated
                // as the built-in `Str` — it would still need an explicit
                // `::myapp/Str -> Int` arrow to coerce. The target-side check
                // is name-based because `target_short` always comes from the
                // trusted `::hot::type/...` path suffix here.
                let source_is_primitive = Self::is_primitive_type_expr(&source_type);
                let always_allowed = source_is_primitive
                    && (source_short == target_short
                        || Self::is_primitive_type_name(&target_short));

                if !always_allowed && !self.impl_registry.contains(&(source_fqn, target_fqn)) {
                    self.errors
                        .errors
                        .push(CompilerError::MissingTypeImplementation {
                            source_type: source_short,
                            target_type: target_short,
                            location: fn_call
                                .src
                                .as_ref()
                                .map(|s| self.source_to_error_location(s)),
                        });
                }
            }

            // Return the target type for successful conversions
            return self.parse_type_annotation(target_type, None);
        }
        // Continue with regular function call type inference
        let function_name = self.extract_function_name(fn_call);

        if let Some(func_name) = function_name {
            // Resolve a leading namespace alias if the call is written as `::alias/fn`.
            // E.g. with `::ctx ::hot::ctx` declared, `::ctx/get` becomes `::hot::ctx/get`.
            let resolved_name = self.resolve_aliased_function_name(&func_name);
            let lookup_name = resolved_name.as_deref().unwrap_or(&func_name);

            // Deprecation: if the callee was declared `meta { deprecated: ... }`,
            // emit a non-fatal warning at the call site. Surfaced by both
            // `hot check` and the LSP; never fails compilation.
            if let Some((def_name, note)) = self.lookup_deprecated_note(lookup_name) {
                // Skip the deprecated definition's own body referencing
                // itself (e.g. one arity delegating to another). Without this
                // guard the stdlib definition of a deprecated helper like
                // `try-call` flags its own self-call, surfacing a warning that
                // every downstream user would see even though they never wrote
                // the deprecated call.
                let is_self_reference = self.current_def.as_deref() == Some(def_name.as_str());
                if !is_self_reference {
                    let location = fn_call
                        .src
                        .as_ref()
                        .map(|s| self.source_to_error_location(s));
                    self.errors.add_warning(CompilerError::DeprecatedUsage {
                        name: lookup_name.to_string(),
                        note,
                        location,
                    });
                }
            }

            // Constructor calls: `TypeName({...})` returns a value of the
            // declared type. The TypeDef registry is keyed by qualified name,
            // so try both the unqualified `lookup_name` (matched by short
            // suffix) and the explicit `current_ns/Name` form so locally
            // declared types resolve without an alias.
            if let Some(t) = self.qualified_types.get(lookup_name).cloned() {
                // Walk arguments for side-effect type-checking, then return
                // the constructed type instead of falling through to the
                // generic function-signature path (which doesn't know about
                // type constructors and would hand back `Any`).
                let mut arg_types = Vec::new();
                for arg in &fn_call.args {
                    arg_types.push(self.infer_value_type(&arg.value));
                }
                if let Some(first_arg_type) = arg_types.first()
                    && matches!(first_arg_type, TypeExpr::Record { .. })
                    && self.types_are_compatible(&t, first_arg_type)
                {
                    return self.apply_annotation_constraint(first_arg_type.clone(), &t);
                }
                return t;
            }
            let qualified_lookup = format!("{}/{}", self.context.current_namespace, lookup_name);
            if let Some(t) = self.qualified_types.get(&qualified_lookup).cloned() {
                let mut arg_types = Vec::new();
                for arg in &fn_call.args {
                    arg_types.push(self.infer_value_type(&arg.value));
                }
                if let Some(first_arg_type) = arg_types.first()
                    && matches!(first_arg_type, TypeExpr::Record { .. })
                    && self.types_are_compatible(&t, first_arg_type)
                {
                    return self.apply_annotation_constraint(first_arg_type.clone(), &t);
                }
                return t;
            }

            // Try to find function signature:
            // 1. First try the (possibly alias-resolved) name
            // 2. Then try qualified name using current namespace (for local functions)
            // 3. Finally try the core variable registry (auto-imported
            //    `meta {core: true}` functions defined in another namespace)
            let signatures = self
                .context
                .get_function_signature(lookup_name)
                .cloned()
                .or_else(|| {
                    let qualified_name =
                        format!("{}/{}", self.context.current_namespace, lookup_name);
                    self.context
                        .get_function_signature(&qualified_name)
                        .cloned()
                })
                .or_else(|| {
                    // Auto-imported core functions: collect signatures from
                    // every namespace that exports `lookup_name` as a core
                    // function. The runtime dispatches dynamically across
                    // these aliases (e.g. `length` lives in both
                    // `::hot::coll` and `::hot::str`), so the type checker
                    // accepts the call when ANY available signature matches.
                    self.core_variables
                        .get(lookup_name)
                        .map(|qualifieds| {
                            let mut combined = Vec::new();
                            for qualified in qualifieds {
                                if let Some(sigs) = self.context.get_function_signature(qualified) {
                                    combined.extend(sigs.iter().cloned());
                                }
                            }
                            combined
                        })
                        .filter(|s| !s.is_empty())
                });

            if let Some(signatures) = signatures {
                // Validate argument types against parameter types for all arities
                // Find the first matching arity signature
                let matching_sig = self.find_matching_signature(fn_call, lookup_name, &signatures);

                if let Some(sig) = matching_sig {
                    return sig.return_type.clone();
                }

                // No matching signature found - report error
                // This will be handled by validate_function_call_arguments
                self.validate_function_call_arguments(fn_call, lookup_name, &signatures);

                // Return first signature's return type as fallback
                if let Some(first_sig) = signatures.first() {
                    return first_sig.return_type.clone();
                }
            } else {
                // Even when we don't know the signature, walk the args so nested
                // function calls inside argument expressions still get type-checked.
                for arg in &fn_call.args {
                    let _ = self.infer_value_type(&arg.value);
                }
            }
        }

        // Default return type for unknown functions
        TypeExpr::Any
    }

    /// If `func_name` begins with a namespace alias declared in the current namespace
    /// (e.g. `::ctx/get` where `::ctx ::hot::ctx` is in scope), return the canonical
    /// form (`::hot::ctx/get`). Returns None when no alias applies.
    fn resolve_aliased_function_name(&self, func_name: &str) -> Option<String> {
        if !func_name.starts_with("::") {
            return None;
        }
        let slash = func_name.find('/')?;
        let ns_part = &func_name[..slash];
        let fn_part = &func_name[slash + 1..];
        let canonical = self.context.resolve_alias(ns_part)?;
        Some(format!("{}/{}", canonical, fn_part))
    }

    /// Infer the type of a function definition.
    ///
    /// Walks each arity's body so that calls inside the body are type-checked
    /// (arity, alias resolution, argument inference). Parameters are pushed into
    /// the variable context for the duration of body inference and restored on exit
    /// — a poor-man's lexical scope sufficient for shadowed parameter names.
    fn infer_function_definition_type(&mut self, fn_defs: &[crate::lang::ast::FnDef]) -> TypeExpr {
        if fn_defs.is_empty() {
            return TypeExpr::Any;
        }

        let mut first_param_types: Option<Vec<TypeExpr>> = None;
        let mut first_return_type: Option<TypeExpr> = None;

        for fn_def in fn_defs {
            // Extract parameter types from type annotations.
            let mut parameter_types = Vec::new();

            // Push a fresh lexical scope for this arity's body. Parameters
            // (and any vars introduced by the body) live in this scope and
            // are dropped on exit, so they don't leak into sibling arities or
            // surrounding code.
            self.context.push_scope();

            for arg in &fn_def.args.args {
                let param_type = if let Some(type_annotation) = &arg.type_annotation {
                    self.parse_type_annotation(type_annotation, None)
                } else {
                    TypeExpr::Any
                };
                parameter_types.push(param_type.clone());

                let name = arg.var.sym.name().to_string();
                self.context.add_variable(name, param_type);
            }

            // Walk the body for type-checking side effects (arity, alias resolution,
            // nested call validation). The body's inferred type would be the return
            // type, but we don't propagate it: the inference plumbing for return
            // types is incomplete and would produce noisy false positives, so we
            // continue to report Any for the function's return type as the
            // pre-walk implementation did.
            //
            // Clear `in_pipe_flow` while inside the body — pipe context only applies
            // to the immediate pipe-stage call, never to a nested fn/lambda body.
            self.in_fn_body += 1;
            let saved_pipe = self.in_pipe_flow;
            self.in_pipe_flow = false;

            // If the body is a `match` flow, the implicit match subject is
            // the function's first parameter. Push its type so the arm
            // walk can enforce exhaustiveness against the corresponding
            // enum. We push `None` for non-match bodies and for match
            // bodies whose first parameter type we can't determine, so
            // exhaustiveness checking is naturally skipped in those cases.
            let pushed_subject = matches!(
                &fn_def.body,
                Value::Flow(f) if matches!(
                    f.flow_type,
                    FlowType::Match | FlowType::MatchShort
                )
            );
            if pushed_subject {
                let subject_type = parameter_types.first().cloned();
                self.match_subject_stack.push(subject_type);
            }

            let body_type = self.infer_value_type(&fn_def.body);

            // Declared return types get the same definite-mismatch warning
            // as binding annotations. Body inference is weak for anything
            // ending in a call (Any), which `warn_annotation_mismatch`
            // skips, so this only fires on statically certain bodies.
            if let Some(ret_ann) = &fn_def.return_type {
                let constraint = self.parse_type_annotation(ret_ann, None);
                let def_name = self
                    .current_def
                    .clone()
                    .unwrap_or_else(|| "function".to_string());
                let src = fn_def
                    .args
                    .args
                    .first()
                    .and_then(|a| a.var.src.clone())
                    .or_else(|| self.current_def_src.clone());
                self.warn_annotation_mismatch(
                    &def_name,
                    ret_ann,
                    &constraint,
                    &body_type,
                    src.as_ref(),
                );
            }

            if pushed_subject {
                self.match_subject_stack.pop();
            }
            self.in_pipe_flow = saved_pipe;
            self.in_fn_body -= 1;

            self.context.pop_scope();

            let return_type = TypeExpr::Any;

            if first_param_types.is_none() {
                first_param_types = Some(parameter_types);
                first_return_type = Some(return_type);
            }
        }

        TypeExpr::Function(
            first_param_types.unwrap_or_default(),
            Box::new(first_return_type.unwrap_or(TypeExpr::Any)),
        )
    }

    /// Process a type implementation and validate it
    ///
    /// Type implementations like `Nanosecond -> Int fn(nanosecond) { ... }` have their
    /// function argument and return types filled in by the parser from source_type and target_type.
    /// This function validates the types and adds parameters to the context for body type-checking.
    fn process_type_implementation(
        &mut self,
        type_impl: &crate::lang::ast::TypeImplementation,
    ) -> TypeExpr {
        let source_type = &type_impl.source_type;
        let target_type = &type_impl.target_type;

        // Parse the expected types from the implementation declaration
        let expected_source_type = self.parse_type_annotation(source_type, None);
        let expected_return_type = self.parse_type_annotation(target_type, None);

        // Extract the function definition from the implementation
        // The parser has already filled in inferred types from source_type/target_type
        if let Value::Fn(fn_defs) = &type_impl.implementation
            && let Some(fn_def) = fn_defs.first()
        {
            // Best-effort source location for InvalidImplementation diagnostics:
            // the first parameter's var span typically falls within the
            // `Source -> Target fn (param) { ... }` declaration, so editors
            // can highlight the problematic implementation.
            let impl_location = fn_def
                .args
                .args
                .first()
                .and_then(|a| a.var.src.as_ref())
                .map(|s| self.source_to_error_location(s));

            // Build parameter types from annotations (now always present due to parser inference)
            let mut parameter_types = Vec::new();
            for (i, arg) in fn_def.args.args.iter().enumerate() {
                let param_type = if let Some(type_annotation) = &arg.type_annotation {
                    let declared_type = self.parse_type_annotation(type_annotation, None);

                    // For the first argument, validate it's compatible with source_type
                    if i == 0 && !self.types_are_compatible(&declared_type, &expected_source_type) {
                        let arg_location = arg
                            .var
                            .src
                            .as_ref()
                            .map(|s| self.source_to_error_location(s))
                            .or_else(|| impl_location.clone());
                        self.errors.errors.push(CompilerError::InvalidImplementation {
                            source_type: source_type.clone(),
                            target_type: target_type.clone(),
                            reason: format!(
                                "Implementation parameter type {} is not compatible with source type {}",
                                declared_type, expected_source_type
                            ),
                            location: arg_location,
                        });
                    }
                    declared_type
                } else {
                    TypeExpr::Any
                };

                // Add parameter to context for body type-checking
                let var_name = arg.var.sym.name().to_string();
                self.context.add_variable(var_name, param_type.clone());
                parameter_types.push(param_type);
            }

            // Get the return type from annotation (now always present due to parser inference)
            let declared_return_type = if let Some(return_type_annotation) = &fn_def.return_type {
                let declared = self.parse_type_annotation(return_type_annotation, None);

                // Validate declared return type matches target_type
                if !self.types_are_compatible(&declared, &expected_return_type) {
                    self.errors
                        .errors
                        .push(CompilerError::InvalidImplementation {
                            source_type: source_type.clone(),
                            target_type: target_type.clone(),
                            reason: format!(
                                "Implementation declares return type {}, but should return {}",
                                declared, expected_return_type
                            ),
                            location: impl_location.clone(),
                        });
                }
                declared
            } else {
                expected_return_type.clone()
            };

            // Type-check the function body and verify it matches the expected return type
            let body_type = self.infer_value_type(&fn_def.body);
            if !self.types_are_compatible(&declared_return_type, &body_type) {
                self.errors
                    .errors
                    .push(CompilerError::InvalidImplementation {
                        source_type: source_type.clone(),
                        target_type: target_type.clone(),
                        reason: format!(
                            "Implementation body returns {}, but should return {}",
                            body_type, declared_return_type
                        ),
                        location: impl_location.clone(),
                    });
            }

            // Return the function type
            return TypeExpr::Function(parameter_types, Box::new(declared_return_type));
        }

        // Fallback for non-function implementations
        self.infer_value_type(&type_impl.implementation)
    }

    /// Process a type definition and validate generic parameters.
    ///
    /// Looks for references to undeclared generic type parameters (single
    /// uppercase letters like `T`) in:
    ///   - struct field type annotations,
    ///   - constructor function parameter annotations.
    ///
    /// Hot doesn't yet support generic type parameters on struct definitions,
    /// so any reference to one is an error. Once `TypeDef` carries declared
    /// generics, the heuristic in `is_undeclared_generic_type` should consult
    /// that list rather than treating every single-uppercase name as undeclared.
    fn process_type_definition(&mut self, type_def: &crate::lang::ast::TypeDef) -> TypeExpr {
        let type_name = type_def.name.name().to_string();

        // Validate inline struct fields (e.g. `Foo type { value: T }`).
        if let Some(fields) = &type_def.fields {
            for field in fields {
                if self.is_undeclared_generic_type(&field.type_annotation) {
                    self.errors.errors.push(CompilerError::InvalidGenericArity {
                        type_name: type_name.clone(),
                        expected: 0,
                        actual: 1,
                        location: self.create_error_location_for_type(&type_name),
                    });
                }
            }
        }

        // Validate constructor function signatures (e.g. `Foo type fn (x: T) {...}`).
        if let Some(constructor_functions) = &type_def.constructor_functions {
            for constructor in constructor_functions {
                for arg in &constructor.args.args {
                    if let Some(type_annotation) = &arg.type_annotation
                        && self.is_undeclared_generic_type(type_annotation)
                    {
                        self.errors.errors.push(CompilerError::InvalidGenericArity {
                            type_name: type_name.clone(),
                            expected: 0,
                            actual: 1,
                            location: self.create_error_location_for_type(&type_name),
                        });
                    }
                }
            }
        }

        // Capture enum metadata so `match` exhaustiveness can be enforced.
        // The pre-pass may have already registered this enum (and even
        // enrolled open-enum variants from arrow declarations); merging
        // here preserves those.
        let ns_for_enum = self.context.current_namespace.clone();
        // No `Var.src` available at this call site (the pre-pass already
        // produced a located diagnostic if applicable, so passing `None`
        // here just avoids duplicating the error).
        self.register_enum_metadata(&type_name, &ns_for_enum, type_def, None);

        TypeExpr::Named(type_name)
    }

    /// Check if a type annotation references an undeclared generic parameter
    fn is_undeclared_generic_type(&self, type_annotation: &str) -> bool {
        // Simple heuristic: single uppercase letter likely indicates a generic type parameter
        // In a more sophisticated system, we'd track declared generic parameters
        type_annotation.len() == 1 && type_annotation.chars().next().unwrap().is_ascii_uppercase()
    }

    /// Parse and resolve a type annotation using proper lexical scoping
    fn parse_type_annotation(&mut self, annotation: &str, source: Option<&Source>) -> TypeExpr {
        let annotation = annotation.trim();

        // Handle optional types (e.g., "Int?" -> Optional(Int))
        if let Some(base_type) = annotation.strip_suffix('?') {
            let inner_type = self.parse_type_annotation(base_type, source);
            return TypeExpr::Optional(Box::new(inner_type));
        }

        // Handle union types with pipe syntax (e.g., "Int | Dec" -> Union([Int, Dec]))
        if annotation.contains(" | ") {
            let parts: Vec<&str> = annotation.split(" | ").collect();
            let type_exprs: Vec<TypeExpr> = parts
                .iter()
                .map(|part| self.parse_type_annotation(part.trim(), source))
                .collect();
            return TypeExpr::Union(type_exprs);
        }

        // Handle generic types with angle bracket syntax (e.g., "Vec<Str>" -> Vec(Str))
        if let Some(open_bracket) = annotation.find('<')
            && let Some(close_bracket) = annotation.rfind('>')
        {
            let base_name = &annotation[..open_bracket];
            let params_str = &annotation[open_bracket + 1..close_bracket];

            // Handle common generic types
            match base_name {
                "Vec" => {
                    let inner_type = self.parse_type_annotation(params_str, source);
                    return TypeExpr::Vec(Box::new(inner_type));
                }
                "Map" => {
                    // Parse Map<Key, Value>
                    let parts: Vec<&str> = params_str.split(',').map(|s| s.trim()).collect();
                    if parts.len() == 2 {
                        let key_type = self.parse_type_annotation(parts[0], source);
                        let value_type = self.parse_type_annotation(parts[1], source);
                        return TypeExpr::Map(Box::new(key_type), Box::new(value_type));
                    }
                }
                _ => {
                    // Other generic types - return as Named for now
                    return TypeExpr::Named(annotation.to_string());
                }
            }
        }

        // Use proper type resolution instead of hard-coded matching
        self.resolve_type_annotation(annotation, source)
    }

    /// Resolve a type annotation using the same lexical scoping as the compiler/VM
    fn resolve_type_annotation(&mut self, type_name: &str, source: Option<&Source>) -> TypeExpr {
        // 1. Check if it's a simple built-in type (no error generation for these)
        if let Some(builtin_type) = self.try_resolve_builtin_type(type_name) {
            return builtin_type;
        }

        // 2. Try lexical scope - local types in current namespace
        if let Some(local_type) = self.try_resolve_local_type(type_name) {
            return local_type;
        }

        // 3. Try core types from hot-std (using same resolution as compiler)
        if let Some(core_type) = self.try_resolve_core_type(type_name) {
            return core_type;
        }

        // 3b. Dotted variant annotations like `Shape.Tri`: a variant is
        //     statically the same shape as its enum (a tagged map). Resolve
        //     `Enum.Variant` to whatever `Enum` resolves to so user-written
        //     annotations - and the synthesized return types of bodyless
        //     `Source -> Enum.Variant` arrows - typecheck without forcing
        //     us to register every variant as its own top-level type.
        if let Some((enum_name, variant_name)) = type_name.split_once('.')
            && !enum_name.is_empty()
            && !variant_name.is_empty()
            && !variant_name.contains('.')
        {
            // Only accept the dotted form if the enum actually exists and
            // the named variant is one we know about (declared, or enrolled
            // via an arrow). Otherwise fall through so the user gets an
            // UnresolvedType error at the original type_name.
            let known_enum = self
                .context
                .types
                .get(enum_name)
                .filter(|td| td.is_enum && (td.is_open || td.variants.contains(variant_name)))
                .is_some();
            if known_enum
                && let Some(enum_type) = self
                    .try_resolve_local_type(enum_name)
                    .or_else(|| self.try_resolve_core_type(enum_name))
                    .or_else(|| self.try_resolve_namespaced_type(enum_name))
            {
                return enum_type;
            }
        }

        // 4. Try fully qualified namespaced types
        if let Some(namespaced_type) = self.try_resolve_namespaced_type(type_name) {
            return namespaced_type;
        }

        // 5. Complex type expressions from hot-std (unions, generics, namespaced
        //    forms) are accepted permissively — `parse_type_annotation` already
        //    handles the structural cases above and only the leaf names land
        //    here. Anything that looks structural but slipped through is
        //    treated as `Any` rather than reported.
        if self.is_complex_type_expression(type_name) {
            tracing::debug!("Allowing complex type expression: {}", type_name);
            return TypeExpr::Any;
        }

        // Simple, unresolved name — emit an UnresolvedType error so type
        // annotations like `: NoSuchType` are caught at compile time. Suppress
        // inside function bodies (see `in_fn_body` docs) until the body-walk
        // inference paths are fully reliable.
        if self.in_fn_body == 0 {
            let location = source
                .map(|s| self.source_to_error_location(s))
                .or_else(|| self.create_error_location_for_type(type_name));
            self.errors.errors.push(CompilerError::UnresolvedType {
                type_name: type_name.to_string(),
                namespace: self.context.current_namespace.clone(),
                message: format!("Type '{}' is not defined", type_name),
                location,
            });
        }
        TypeExpr::Any
    }

    /// Try to resolve simple built-in types (Int, Str, etc.)
    fn try_resolve_builtin_type(&self, type_name: &str) -> Option<TypeExpr> {
        // Handle both simple names and Builtin(...) syntax
        let actual_type_name = if type_name.starts_with("Builtin(") && type_name.ends_with(")") {
            // Extract type from Builtin(TypeName) format
            &type_name[8..type_name.len() - 1] // Remove "Builtin(" and ")"
        } else {
            type_name
        };

        match actual_type_name {
            "Int" => Some(TypeExpr::Int),
            "Dec" => Some(TypeExpr::Dec),
            "Str" => Some(TypeExpr::Str),
            "Bool" => Some(TypeExpr::Bool),
            "Bytes" => Some(TypeExpr::Bytes),
            "Byte" => Some(TypeExpr::Byte),
            "Null" => Some(TypeExpr::Null),
            "Any" => Some(TypeExpr::Any),
            // Bare collection names mean "any element/value type" — write `Map<K, V>`
            // explicitly only when the caller wants a constrained generic.
            "Vec" => Some(TypeExpr::Vec(Box::new(TypeExpr::Any))),
            "Map" => Some(TypeExpr::Map(
                Box::new(TypeExpr::Any),
                Box::new(TypeExpr::Any),
            )),
            _ => None,
        }
    }

    /// Try to resolve a bare type name against the types declared in the
    /// current namespace. This mirrors the compiler/VM rule that an unqualified
    /// reference is interpreted relative to the calling namespace before any
    /// global lookup is attempted.
    fn try_resolve_local_type(&self, type_name: &str) -> Option<TypeExpr> {
        // Anything qualified is handled by `try_resolve_namespaced_type`; bare
        // names with separators aren't local references.
        if type_name.contains("::") || type_name.contains('/') {
            return None;
        }
        let qualified = format!("{}/{}", self.context.current_namespace, type_name);
        self.qualified_types.get(&qualified).cloned().or_else(|| {
            // Fall back to the in-pass types registered by `process_type_definition`
            // (e.g. types defined later in the same file that haven't been
            // pre-collected yet).
            if self.context.types.contains_key(type_name) {
                Some(TypeExpr::Named(type_name.to_string()))
            } else {
                None
            }
        })
    }

    /// Try to resolve a bare type name as a core (auto-imported) type from any
    /// namespace, the same way the compiler treats `core: true` variables.
    fn try_resolve_core_type(&self, type_name: &str) -> Option<TypeExpr> {
        if type_name.contains("::") || type_name.contains('/') {
            return None;
        }
        let qualified = self.core_types.get(type_name)?;
        self.qualified_types.get(qualified).cloned()
    }

    /// Try to resolve a fully qualified type name. Honors namespace aliases
    /// declared at the top of the current namespace (e.g. `::ctx ::hot::ctx`),
    /// then looks the canonical name up in the qualified-type registry built
    /// from the program's `TypeDef` declarations.
    fn try_resolve_namespaced_type(&self, type_name: &str) -> Option<TypeExpr> {
        if !type_name.starts_with("::") {
            return None;
        }

        // Split into namespace and short name so we can rewrite the namespace
        // through any active alias before lookup.
        let canonical = if let Some(slash_idx) = type_name.rfind('/') {
            let (ns_part, short_part) = type_name.split_at(slash_idx);
            let resolved_ns = self
                .context
                .resolve_alias(ns_part)
                .map(|s| s.as_str())
                .unwrap_or(ns_part);
            format!("{}{}", resolved_ns, short_part)
        } else {
            type_name.to_string()
        };

        if let Some(t) = self.qualified_types.get(&canonical) {
            return Some(t.clone());
        }

        // Belt-and-suspenders: when the program is compiled without `hot-std`
        // (e.g. an isolated unit test of the type checker), the registry won't
        // contain `::hot::type/<Primitive>`. Fall back to the hard-coded
        // primitive list so those references still unify with the bare names.
        if let Some(short) = canonical.strip_prefix("::hot::type/")
            && let Some(builtin) = self.try_resolve_builtin_type(short)
        {
            return Some(builtin);
        }

        // Unknown qualified types: stay permissive (current behaviour) rather
        // than emitting an error — type resolution across all packages isn't
        // fully wired up yet, and false positives are worse than missed errors.
        Some(TypeExpr::Named(canonical))
    }

    /// Check if a type name represents a complex type expression that should be allowed
    fn is_complex_type_expression(&self, type_name: &str) -> bool {
        // Detect complex type expressions from hot-std that use special syntax
        // Be more specific to avoid false positives
        // Debug-format patterns:
        type_name.contains("Union(")
            || type_name.contains("Generic(")
            || type_name.contains("Builtin(")
            || type_name.contains("[")
            // New display format patterns:
            || type_name.contains(" | ")  // Union types: "Int | Dec"
            || type_name.contains('<')    // Generic types: "Vec<Str>"
            || type_name.starts_with("::") // Namespaced types
            // Allow PascalCase identifiers that are likely custom types from hot-std
            || self.looks_like_custom_type(type_name)
        // Note: We intentionally do NOT include Named() expressions here
        // because simple Named("SomeType") should be validated as regular types
    }

    /// Check if a type name is a known type from the program (including hot-std)
    fn looks_like_custom_type(&self, type_name: &str) -> bool {
        // Check against types discovered during the first pass of type checking
        self.known_types.contains(type_name)
    }

    /// Find matching signature for a function call from available arities
    fn find_matching_signature<'a>(
        &mut self,
        fn_call: &FnCall,
        _func_name: &str,
        signatures: &'a [FunctionSignature],
    ) -> Option<&'a FunctionSignature> {
        // The pipe context only applies to THIS call (whose first arg is the
        // piped value) — not to nested calls inside its argument expressions.
        // Capture the flag for our own arity math, then clear it so recursive
        // `infer_value_type` walks don't inflate nested call arities.
        let in_pipe_flow = self.in_pipe_flow;
        self.in_pipe_flow = false;
        let explicit_args = fn_call.args.len();
        let call_arity = if in_pipe_flow {
            explicit_args + 1 // Add 1 for the piped value
        } else {
            explicit_args
        };

        // Try to find a signature that matches the arity
        let result = (|| {
            for sig in signatures {
                let sig_arity = sig.parameter_types.len();

                // Check if arity matches
                // For variadic functions, the minimum arity is (params - 1) since the variadic param can be empty
                let arity_matches = if sig.variadic {
                    let min_arity = if sig_arity > 0 { sig_arity - 1 } else { 0 };
                    call_arity >= min_arity
                } else {
                    call_arity == sig_arity
                };

                if arity_matches {
                    // Check if argument types are compatible.
                    // When in pipe flow, we skip the first parameter (the piped value)
                    // and check the explicit arguments against remaining parameters.
                    //
                    // Note the canonical argument order is
                    // `types_are_compatible(expected, actual)`.
                    let mut all_args_compatible = true;
                    let param_offset = if in_pipe_flow { 1 } else { 0 };

                    for (i, arg) in fn_call.args.iter().enumerate() {
                        if let Some(expected_type) = sig.parameter_types.get(i + param_offset) {
                            let actual_type = self.infer_value_type(&arg.value);
                            if !self.types_are_compatible(expected_type, &actual_type)
                                && !self.coercion_available(&actual_type, expected_type)
                            {
                                all_args_compatible = false;
                                break;
                            }
                        }
                    }

                    if all_args_compatible {
                        return Some(sig);
                    }
                }
            }
            None
        })();

        self.in_pipe_flow = in_pipe_flow;
        result
    }

    /// Validate function call arguments against parameter types (for multiple arities)
    fn validate_function_call_arguments(
        &mut self,
        fn_call: &FnCall,
        func_name: &str,
        signatures: &[FunctionSignature],
    ) {
        // The pipe context only applies to THIS call (whose first arg is the
        // piped value) — not to nested calls inside its argument expressions.
        // Capture the flag for our own arity math, then clear it so recursive
        // `infer_value_type` walks don't inflate nested call arities.
        let in_pipe_flow = self.in_pipe_flow;
        self.in_pipe_flow = false;
        let explicit_args = fn_call.args.len();
        let call_arity = if in_pipe_flow {
            explicit_args + 1 // Add 1 for the piped value
        } else {
            explicit_args
        };

        // Check if any signature matches the arity
        // For variadic functions, minimum arity is (params - 1) since the variadic param can be empty
        let matching_arity_sigs: Vec<&FunctionSignature> = signatures
            .iter()
            .filter(|sig| {
                if sig.variadic {
                    let min_arity = if !sig.parameter_types.is_empty() {
                        sig.parameter_types.len() - 1
                    } else {
                        0
                    };
                    call_arity >= min_arity
                } else {
                    call_arity == sig.parameter_types.len()
                }
            })
            .collect();

        if matching_arity_sigs.is_empty() {
            // No signature with matching arity found
            let available_arities: Vec<String> = signatures
                .iter()
                .map(|sig| {
                    if sig.variadic {
                        // Show minimum arity (params - 1) for variadic
                        let min = if !sig.parameter_types.is_empty() {
                            sig.parameter_types.len() - 1
                        } else {
                            0
                        };
                        format!("{}+", min)
                    } else {
                        sig.parameter_types.len().to_string()
                    }
                })
                .collect();

            let message = if signatures.len() == 1 {
                let sig = &signatures[0];
                let expected = if sig.variadic && !sig.parameter_types.is_empty() {
                    sig.parameter_types.len() - 1
                } else {
                    sig.parameter_types.len()
                };
                if sig.variadic {
                    format!(
                        "Function '{}' expects at least {} arguments, but {} were provided",
                        func_name, expected, call_arity
                    )
                } else {
                    format!(
                        "Function '{}' expects {} arguments, but {} were provided",
                        func_name, expected, call_arity
                    )
                }
            } else {
                format!(
                    "Function '{}' expects {} arguments, but {} were provided. Available arities: {}",
                    func_name,
                    available_arities.join(" or "),
                    call_arity,
                    available_arities.join(", ")
                )
            };

            self.errors.errors.push(CompilerError::ArityMismatch {
                func_name: func_name.to_string(),
                expected: signatures[0].parameter_types.len(),
                actual: call_arity,
                message,
                location: fn_call
                    .src
                    .as_ref()
                    .map(|s| self.source_to_error_location(s))
                    .or_else(|| self.create_error_location_for_function_call(func_name)),
            });
            self.in_pipe_flow = in_pipe_flow;
            return;
        }

        // Check type compatibility for matching arity signatures.
        // When in pipe flow, we skip the first parameter (the piped value).
        //
        // Strategy: walk every matching-arity signature and remember the
        // per-signature type errors. If any signature accepts the call we
        // bail out cleanly. Otherwise we fall through and report errors
        // against the "best" candidate (the signature with the fewest
        // mismatches), which keeps error output sane for overloaded
        // builtins like `add` that register both a narrow `(Int, Int)`
        // form and a broader `(Int|Dec, Int|Dec)` form — picking a
        // wildly different overload would just produce noise.
        let param_offset = if in_pipe_flow { 1 } else { 0 };
        let mut best_errors: Option<Vec<(usize, TypeExpr, TypeExpr)>> = None;
        for sig in matching_arity_sigs {
            let mut type_errors = Vec::new();

            for (i, arg) in fn_call.args.iter().enumerate() {
                if let Some(expected_type) = sig.parameter_types.get(i + param_offset) {
                    let actual_type = self.infer_value_type(&arg.value);
                    // Check if actual type is compatible with expected (e.g., Int is compatible with Int | Str | Bool)
                    let mut compatible = self.types_are_compatible(expected_type, &actual_type);

                    // For literal arguments (e.g. `pick-fruit("apple")`), also
                    // check the precise literal type. This lets a `Str` literal
                    // satisfy a literal-union parameter like
                    // `"apple" | "banana" | "orange"` without admitting arbitrary
                    // strings.
                    if !compatible && let Some(lit_type) = Self::literal_type_of_value(&arg.value) {
                        compatible = self.types_are_compatible(expected_type, &lit_type);
                    }

                    // Last-resort: implicit coercion via a directly-registered
                    // `actual -> expected` arrow.
                    if !compatible && self.coercion_available(&actual_type, expected_type) {
                        compatible = true;
                    }

                    if !compatible {
                        type_errors.push((i, expected_type.clone(), actual_type));
                    }
                }
            }

            if type_errors.is_empty() {
                // Found a matching signature with compatible types
                self.in_pipe_flow = in_pipe_flow;
                return;
            }

            // Track the closest-matching signature so we can report against
            // it if no overload ends up accepting the call.
            match &best_errors {
                Some(existing) if existing.len() <= type_errors.len() => {}
                _ => best_errors = Some(type_errors),
            }
        }

        // No overload accepted the call. Suppress when walking inside a fn /
        // lambda body (see `in_fn_body` docs) to avoid surfacing latent
        // inference gaps as false positives. Arity errors above are still
        // emitted regardless.
        if self.in_fn_body == 0
            && let Some(type_errors) = best_errors
        {
            for (i, expected_type, actual_type) in type_errors {
                let location = fn_call.args[i]
                    .src
                    .as_ref()
                    .map(|s| self.source_to_error_location(s))
                    .or_else(|| {
                        fn_call
                            .src
                            .as_ref()
                            .map(|s| self.source_to_error_location(s))
                    })
                    .or_else(|| self.create_error_location_for_function_call(func_name));

                self.errors.errors.push(CompilerError::TypeMismatch {
                    expected: format!("{}", expected_type),
                    actual: format!("{}", actual_type),
                    var_name: None,
                    message: format!(
                        "Type mismatch in function '{}' argument {}: expected {}, got {}",
                        func_name,
                        i + 1 + param_offset, // Adjust argument number for pipe context
                        expected_type,
                        actual_type
                    ),
                    location,
                });
            }
        }

        self.in_pipe_flow = in_pipe_flow;
    }

    /// Walk flow expressions that may use the `var_name expr` pairing
    /// convention (e.g. `parallel|map { a 10  b 20  c add(a, 5) }`).
    ///
    /// The parser doesn't fold these into `MultipleValues` for parallel-style
    /// flows, so each token-pair lands as two adjacent `Value` entries: the
    /// first a bare `Var` reference, the second its bound value. We pair them
    /// here, infer the bound expression, and seed the inference context so
    /// subsequent expressions referencing those names get the right type.
    /// Returns the per-binding result types (or per-expression if no pairing
    /// is detected) so the caller can wrap them in a `Vec` / `Map`.
    /// Build the result type for a flow given its modifier.
    /// `default_map` is true when the flow's natural default is a Map keyed by
    /// branch name (parallel, cond-all, match-all); false when the default is
    /// a single value.
    fn flow_result_type(
        &self,
        modifier: Option<&crate::lang::ast::ResultModifier>,
        result_types: Vec<TypeExpr>,
        default_map: bool,
    ) -> TypeExpr {
        use crate::lang::ast::ResultModifier;

        let union_or_single = |types: Vec<TypeExpr>| {
            if types.len() == 1 {
                types[0].clone()
            } else if types.is_empty() {
                TypeExpr::Null
            } else {
                TypeExpr::Union(types)
            }
        };

        match modifier {
            Some(ResultModifier::Map) => TypeExpr::Map(
                Box::new(TypeExpr::Str),
                Box::new(union_or_single(result_types)),
            ),
            Some(ResultModifier::Vec) => TypeExpr::Vec(Box::new(union_or_single(result_types))),
            // One takes the single final value. Both callers are parallel
            // flows, where the final binding is deterministic, so its type
            // is the result type.
            Some(ResultModifier::One) => result_types.last().cloned().unwrap_or(TypeExpr::Null),
            None => {
                if default_map {
                    TypeExpr::Map(
                        Box::new(TypeExpr::Str),
                        Box::new(union_or_single(result_types)),
                    )
                } else {
                    union_or_single(result_types)
                }
            }
        }
    }

    fn walk_paired_bindings(&mut self, expressions: &[Value]) -> Vec<TypeExpr> {
        let mut result_types = Vec::new();
        let mut i = 0;
        while i < expressions.len() {
            let cur = &expressions[i];
            let next = expressions.get(i + 1);
            if let (Value::Ref(Ref::Var(var_ref)), Some(value)) = (cur, next)
                && var_ref.var.deep_path.is_none()
                && var_ref.var.deep_set.is_none()
                && !matches!(
                    value,
                    Value::Ref(Ref::Var(v)) if v.var.deep_path.is_none()
                        && v.var.deep_set.is_none()
                )
            {
                let inferred = self.infer_value_type(value);
                let name = var_ref.var.sym.name().to_string();
                self.context.add_variable(name, inferred.clone());
                result_types.push(inferred);
                i += 2;
                continue;
            }
            result_types.push(self.infer_value_type(cur));
            i += 1;
        }
        result_types
    }

    /// Infer the type of a flow expression
    fn infer_flow_type(&mut self, flow: &Flow) -> TypeExpr {
        if flow.expressions.is_empty() {
            return TypeExpr::Null;
        }

        match flow.flow_type {
            FlowType::Serial => {
                // Walk every expression so calls earlier in the body are type-checked,
                // not just whichever happens to be the return value.
                let mut last_type = TypeExpr::Null;
                for expr in &flow.expressions {
                    last_type = self.infer_value_type(expr);
                }
                last_type
            }
            FlowType::Parallel => {
                // Parallel flow bodies pair consecutive expressions as
                // `var_name expr` bindings (e.g. `a 10  b 20  c add(a, 5)`).
                // Walking them with `pair_bind` keeps later expressions in
                // the body able to type-check against earlier bindings.
                let result_types = self.walk_paired_bindings(&flow.expressions);
                self.flow_result_type(flow.result_modifier.as_ref(), result_types, true)
            }
            FlowType::Cond => {
                // Conditional flow returns a union of possible result types
                let result_types: Vec<TypeExpr> = flow
                    .expressions
                    .iter()
                    .skip(1) // Skip the condition
                    .map(|expr| self.infer_value_type(expr))
                    .collect();

                if result_types.is_empty() {
                    TypeExpr::Null
                } else if result_types.len() == 1 {
                    TypeExpr::Optional(Box::new(result_types[0].clone()))
                } else {
                    TypeExpr::Union(result_types)
                }
            }
            FlowType::CondAll => {
                // CondAll flow is similar to Cond but all conditions must be checked
                let result_types: Vec<TypeExpr> = flow
                    .expressions
                    .iter()
                    .map(|expr| self.infer_value_type(expr))
                    .collect();

                TypeExpr::Union(result_types)
            }
            FlowType::Pipe => {
                // Pipe flow returns the type of the last expression in the chain
                // For each expression after the first, the piped value is prepended as an argument
                // We need to set in_pipe_flow flag so arity checks account for the extra argument

                // Process first expression (initial value) without pipe context
                let mut last_type = if let Some(first_expr) = flow.expressions.first() {
                    self.infer_value_type(first_expr)
                } else {
                    TypeExpr::Null
                };

                // Process remaining expressions with pipe context
                self.in_pipe_flow = true;
                for expr in flow.expressions.iter().skip(1) {
                    last_type = self.infer_value_type(expr);
                }
                self.in_pipe_flow = false;

                // Return the type of the last expression (already computed above)
                last_type
            }
            // Handle short form flows
            FlowType::SerialShort => {
                let mut last_type = TypeExpr::Null;
                for expr in &flow.expressions {
                    last_type = self.infer_value_type(expr);
                }
                last_type
            }
            FlowType::ParallelShort => {
                let result_types = self.walk_paired_bindings(&flow.expressions);
                self.flow_result_type(flow.result_modifier.as_ref(), result_types, true)
            }
            FlowType::CondShort => {
                let result_types: Vec<TypeExpr> = flow
                    .expressions
                    .iter()
                    .skip(1)
                    .map(|expr| self.infer_value_type(expr))
                    .collect();
                TypeExpr::Optional(Box::new(TypeExpr::Union(result_types)))
            }
            FlowType::CondAllShort => {
                let result_types: Vec<TypeExpr> = flow
                    .expressions
                    .iter()
                    .map(|expr| self.infer_value_type(expr))
                    .collect();
                TypeExpr::Union(result_types)
            }
            // Match flows are similar to Cond flows for typing
            FlowType::Match | FlowType::MatchShort => {
                let result_types: Vec<TypeExpr> = flow
                    .expressions
                    .iter()
                    .map(|expr| self.infer_value_type(expr))
                    .collect();
                self.check_match_exhaustiveness(flow);
                if result_types.is_empty() {
                    TypeExpr::Null
                } else if result_types.len() == 1 {
                    TypeExpr::Optional(Box::new(result_types[0].clone()))
                } else {
                    TypeExpr::Union(result_types)
                }
            }
            FlowType::MatchAll | FlowType::MatchAllShort => {
                for expr in &flow.expressions {
                    let _ = self.infer_value_type(expr);
                }
                TypeExpr::Map(Box::new(TypeExpr::Str), Box::new(TypeExpr::Any))
            }
        }
    }
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::Int => write!(f, "Int"),
            TypeExpr::Dec => write!(f, "Dec"),
            TypeExpr::Str => write!(f, "Str"),
            TypeExpr::Bool => write!(f, "Bool"),
            TypeExpr::Vec(inner) => write!(f, "Vec<{}>", inner),
            TypeExpr::Map(key, value) => write!(f, "Map<{}, {}>", key, value),
            TypeExpr::Bytes => write!(f, "Bytes"),
            TypeExpr::Byte => write!(f, "Byte"),
            TypeExpr::Null => write!(f, "Null"),
            TypeExpr::Literal(lit) => write!(f, "{}", lit),
            TypeExpr::Any => write!(f, "Any"),
            TypeExpr::Never => write!(f, "Never"),
            TypeExpr::Optional(inner) => write!(f, "{}?", inner),
            TypeExpr::Union(types) => {
                let type_strs: Vec<String> = types.iter().map(|t| t.to_string()).collect();
                write!(f, "{}", type_strs.join(" | "))
            }
            TypeExpr::Function(params, return_type) => {
                let param_strs: Vec<String> = params.iter().map(|t| t.to_string()).collect();
                write!(f, "({}) -> {}", param_strs.join(", "), return_type)
            }
            TypeExpr::Record { fields, open, .. } => {
                let mut field_strs: Vec<String> = fields
                    .iter()
                    .map(|(name, field_type)| format!("{}: {}", name, field_type))
                    .collect();
                field_strs.sort();
                if *open {
                    field_strs.push("...".to_string());
                }
                write!(f, "{{{}}}", field_strs.join(", "))
            }
            TypeExpr::Named(name) => write!(f, "{}", name),
            TypeExpr::Generic(name, params) => {
                let param_strs: Vec<String> = params.iter().map(|t| t.to_string()).collect();
                write!(f, "{}<{}>", name, param_strs.join(", "))
            }
            TypeExpr::Lazy(inner) => write!(f, "lazy {}", inner),
            TypeExpr::Variadic(inner) => write!(f, "...{}", inner),
            TypeExpr::Namespace(ns) => write!(f, "{}", ns),
            TypeExpr::TypeRef(type_ref) => write!(f, "&{}", type_ref),
            TypeExpr::Constrained(inner, constraints) => {
                write!(f, "{}", inner)?;
                if !constraints.is_empty() {
                    write!(f, " where ")?;
                    let constraint_strs: Vec<String> = constraints
                        .iter()
                        .map(|c| match c {
                            TypeConstraint::HasMethod(name, args, ret) => {
                                let arg_strs: Vec<String> =
                                    args.iter().map(|t| t.to_string()).collect();
                                format!("{}({}) -> {}", name, arg_strs.join(", "), ret)
                            }
                            TypeConstraint::Implements(trait_name) => {
                                format!("implements {}", trait_name)
                            }
                            TypeConstraint::CoreType => "core".to_string(),
                        })
                        .collect();
                    write!(f, "{}", constraint_strs.join(", "))?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_val_type_inference() {
        let checker = TypeChecker::new();

        assert_eq!(checker.infer_val_type(&Val::Int(42)), TypeExpr::Int);
        assert_eq!(checker.infer_val_type(&Val::from("hello")), TypeExpr::Str);
        assert_eq!(checker.infer_val_type(&Val::Bool(true)), TypeExpr::Bool);
        assert_eq!(checker.infer_val_type(&Val::Null), TypeExpr::Null);
    }

    #[test]
    fn test_type_compatibility() {
        let checker = TypeChecker::new();

        // Any is compatible with everything
        assert!(checker.types_are_compatible(&TypeExpr::Any, &TypeExpr::Int));
        assert!(checker.types_are_compatible(&TypeExpr::Int, &TypeExpr::Any));

        // Exact matches
        assert!(checker.types_are_compatible(&TypeExpr::Int, &TypeExpr::Int));
        assert!(checker.types_are_compatible(&TypeExpr::Str, &TypeExpr::Str));

        // Numeric compatibility
        assert!(checker.types_are_compatible(&TypeExpr::Dec, &TypeExpr::Int));
        assert!(!checker.types_are_compatible(&TypeExpr::Int, &TypeExpr::Dec));

        // Optional types
        let optional_int = TypeExpr::Optional(Box::new(TypeExpr::Int));
        assert!(checker.types_are_compatible(&optional_int, &TypeExpr::Int));
        assert!(checker.types_are_compatible(&optional_int, &TypeExpr::Null));
    }

    #[test]
    fn test_literal_type_compatibility() {
        let checker = TypeChecker::new();

        // Literal types match if values are equal
        let apple = TypeExpr::Literal(LiteralType::Str("apple".to_string()));
        let apple2 = TypeExpr::Literal(LiteralType::Str("apple".to_string()));
        let banana = TypeExpr::Literal(LiteralType::Str("banana".to_string()));

        assert!(checker.types_are_compatible(&apple, &apple2));
        assert!(!checker.types_are_compatible(&apple, &banana));

        // Literal types are subtypes of their base types
        assert!(checker.types_are_compatible(&TypeExpr::Str, &apple));
        assert!(checker.types_are_compatible(&TypeExpr::Str, &banana));

        let forty_two = TypeExpr::Literal(LiteralType::Int(42));
        assert!(checker.types_are_compatible(&TypeExpr::Int, &forty_two));
        assert!(checker.types_are_compatible(&TypeExpr::Dec, &forty_two)); // Int literal compatible with Dec

        let pi = TypeExpr::Literal(LiteralType::Dec("3.14".to_string()));
        assert!(checker.types_are_compatible(&TypeExpr::Dec, &pi));
        assert!(!checker.types_are_compatible(&TypeExpr::Int, &pi)); // Dec literal not compatible with Int

        let true_lit = TypeExpr::Literal(LiteralType::Bool(true));
        let false_lit = TypeExpr::Literal(LiteralType::Bool(false));
        assert!(checker.types_are_compatible(&TypeExpr::Bool, &true_lit));
        assert!(checker.types_are_compatible(&TypeExpr::Bool, &false_lit));
        assert!(!checker.types_are_compatible(&true_lit, &false_lit));

        let null_lit = TypeExpr::Literal(LiteralType::Null);
        assert!(checker.types_are_compatible(&TypeExpr::Null, &null_lit));
    }

    #[test]
    fn test_literal_union_types() {
        let checker = TypeChecker::new();

        // Union of literals (e.g., "apple" | "banana" | "orange")
        let apple = TypeExpr::Literal(LiteralType::Str("apple".to_string()));
        let banana = TypeExpr::Literal(LiteralType::Str("banana".to_string()));
        let orange = TypeExpr::Literal(LiteralType::Str("orange".to_string()));
        let grape = TypeExpr::Literal(LiteralType::Str("grape".to_string()));

        let fruit_union = TypeExpr::Union(vec![apple.clone(), banana.clone(), orange.clone()]);

        // Literal in union is compatible
        assert!(checker.types_are_compatible(&fruit_union, &apple));
        assert!(checker.types_are_compatible(&fruit_union, &banana));
        assert!(checker.types_are_compatible(&fruit_union, &orange));

        // Literal not in union is not compatible
        assert!(!checker.types_are_compatible(&fruit_union, &grape));

        // Mixed union with literals and base types
        let mixed_union = TypeExpr::Union(vec![
            TypeExpr::Literal(LiteralType::Str("invalid".to_string())),
            TypeExpr::Int,
        ]);

        let invalid = TypeExpr::Literal(LiteralType::Str("invalid".to_string()));
        assert!(checker.types_are_compatible(&mixed_union, &invalid));
        assert!(checker.types_are_compatible(&mixed_union, &TypeExpr::Int));
        // Str is accepted because the literal "invalid" widens to Str at the
        // call site - we cannot statically prove the runtime string value.
        assert!(checker.types_are_compatible(&mixed_union, &TypeExpr::Str));
    }

    #[test]
    fn test_duplicate_arrow_short_name_emits_ambiguous_error() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // Two arrows with the same short-name (source, target) pair must
        // surface an `AmbiguousTypeImplementation` (`ambiguous-type-implementation`), citing both
        // declarations. The variant target form here exercises the dotted
        // target name machinery introduced in Stage 1/2.
        let source = r#"
        Triangle type { base: Dec, height: Dec }
        Shape enum open { }
        Triangle -> Shape.Tri
        Triangle -> Shape.Tri
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let result = checker.check_program(&program);

        // The check must fail and the error must specifically be the
        // duplicate-arrow diagnostic — not a generic missing-type error.
        assert!(result.is_err(), "expected duplicate-arrow error");
        let errors = result.unwrap_err();
        let has_ambiguous = errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::AmbiguousTypeImplementation {
                    source_type, target_type, ..
                } if source_type == "Triangle" && target_type == "Shape.Tri"
            )
        });
        assert!(
            has_ambiguous,
            "expected AmbiguousTypeImplementation for Triangle -> Shape.Tri, got: {:?}",
            errors.errors
        );
    }

    #[test]
    fn test_deprecated_call_emits_warning_not_error() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // A `core: true` deprecated function is auto-imported, mirroring how
        // `::hot::lang/try-call` resolves from a bare `try-call(...)` call.
        let source = r#"
        old-fn meta { core: true, deprecated: true } fn (): Int { 1 }
        caller fn (): Int { old-fn() }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let result = checker.check_program(&program);

        // Deprecation is a warning: it must never fail the check.
        assert!(
            result.is_ok(),
            "deprecated usage must not fail compilation: {:?}",
            result
        );
        let has_warning = checker.errors.warnings.iter().any(|w| {
            matches!(
                w,
                CompilerError::DeprecatedUsage { name, note, .. }
                    if name.contains("old-fn") && note.is_none()
            )
        });
        assert!(
            has_warning,
            "expected DeprecatedUsage warning, got: {:?}",
            checker.errors.warnings
        );
    }

    #[test]
    fn test_annotation_mismatch_warns_on_definite_conflict() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // Binding, collect-all single-value flow, and fn return type each
        // get the annotation-mismatch warning when both sides are definite.
        let source = r#"
        plain: Int "hello"

        flow-fn fn (): Str {
            single: Int parallel {
                a 1
                b "hello"
            }
            single
        }

        bad-return fn (): Int { "hello" }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let result = checker.check_program(&program);
        assert!(
            result.is_ok(),
            "annotation mismatches must warn, not fail: {:?}",
            result
        );

        let names: Vec<&str> = checker
            .errors
            .warnings
            .iter()
            .filter_map(|w| match w {
                CompilerError::AnnotationMismatch { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        for expected in ["plain", "single", "bad-return"] {
            assert!(
                names.iter().any(|n| n.contains(expected)),
                "expected annotation-mismatch for `{}`, got: {:?}",
                expected,
                checker.errors.warnings
            );
        }
    }

    #[test]
    fn test_annotation_mismatch_stays_quiet_when_uncertain_or_compatible() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // Compatible values, All flow-shape annotations, inference-weak
        // values (call results), and partially compatible unions must not
        // warn.
        let source = r#"
        ok-int: Int 42
        ok-dec: Dec 1

        collected fn (): Map {
            data: All<Map> parallel {
                a 1
                b 2
            }
            data
        }

        last-is-int fn (): Int {
            v: Int parallel {
                a "hello"
                b 2
            }
            v
        }

        maybe fn (flag: Bool): Str {
            branchy: Str cond {
                flag => { "yes" }
                => { "no" }
            }
            branchy
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let result = checker.check_program(&program);
        assert!(result.is_ok(), "no errors expected: {:?}", result);

        let mismatches: Vec<_> = checker
            .errors
            .warnings
            .iter()
            .filter(|w| matches!(w, CompilerError::AnnotationMismatch { .. }))
            .collect();
        assert!(
            mismatches.is_empty(),
            "expected no annotation-mismatch warnings, got: {:?}",
            mismatches
        );
    }

    #[test]
    fn test_deprecated_call_carries_custom_note() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        old-fn meta { core: true, deprecated: "use new-fn instead" } fn (): Int { 1 }
        caller fn (): Int { old-fn() }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let note = checker.errors.warnings.iter().find_map(|w| match w {
            CompilerError::DeprecatedUsage { name, note, .. } if name.contains("old-fn") => {
                Some(note.clone())
            }
            _ => None,
        });
        assert_eq!(
            note,
            Some(Some("use new-fn instead".to_string())),
            "expected custom deprecation note, got: {:?}",
            checker.errors.warnings
        );
    }

    #[test]
    fn test_non_deprecated_call_emits_no_warning() {
        use crate::lang::parser::parse_hot;

        let source = r#"
        ok-fn meta { core: true } fn (): Int { 1 }
        caller fn (): Int { ok-fn() }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        assert!(
            checker.errors.warnings.is_empty(),
            "non-deprecated call must not warn, got: {:?}",
            checker.errors.warnings
        );
    }

    #[test]
    fn test_deprecated_self_reference_emits_no_warning() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // A deprecated function whose own body references itself (here one
        // arity delegating to another) must not flag its own definition. This
        // mirrors the stdlib `try-call` whose 1-arg arity calls the 2-arg one;
        // otherwise the warning would surface on every project loading stdlib.
        let source = r#"
        old-fn
        meta { core: true, deprecated: true }
        fn
        (a: Int, b: Int): Int { a },
        (a: Int): Int { old-fn(a, 0) }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_warning = checker.errors.warnings.iter().any(
            |w| matches!(w, CompilerError::DeprecatedUsage { name, .. } if name.contains("old-fn")),
        );
        assert!(
            !has_warning,
            "deprecated definition's self-reference must not warn, got: {:?}",
            checker.errors.warnings
        );
    }

    #[test]
    fn test_deprecated_external_call_still_warns_after_self_guard() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // The self-reference guard must not suppress warnings from a *different*
        // definition calling the deprecated function.
        let source = r#"
        old-fn
        meta { core: true, deprecated: true }
        fn
        (a: Int, b: Int): Int { a },
        (a: Int): Int { old-fn(a, 0) }
        caller fn (): Int { old-fn(1) }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_warning = checker.errors.warnings.iter().any(
            |w| matches!(w, CompilerError::DeprecatedUsage { name, .. } if name.contains("old-fn")),
        );
        assert!(
            has_warning,
            "external deprecated call must still warn, got: {:?}",
            checker.errors.warnings
        );
    }

    #[test]
    fn test_open_enum_match_without_default_is_error() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        Shape enum open { Circle, Rectangle }
        describe fn match (s: Shape): Str {
            Shape.Circle => { "circle" }
            Shape.Rectangle => { "rect" }
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_open_err = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenEnumMatchMissingDefault { enum_name, .. }
                    if enum_name == "Shape"
            )
        });
        assert!(
            has_open_err,
            "expected OpenEnumMatchMissingDefault for Shape, got: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_open_enum_match_with_default_is_ok() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        Shape enum open { Circle, Rectangle }
        describe fn match (s: Shape): Str {
            Shape.Circle => { "circle" }
            Shape.Rectangle => { "rect" }
            _ => { "other" }
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_match_err = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenEnumMatchMissingDefault { .. }
                    | CompilerError::NonExhaustiveMatch { .. }
            )
        });
        assert!(
            !has_match_err,
            "open-enum match with `_` arm should not error: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_closed_enum_non_exhaustive_match_is_error() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        Direction enum { Up, Down, Left, Right }
        describe fn match (d: Direction): Str {
            Direction.Up => { "up" }
            Direction.Down => { "down" }
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_nonexh = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::NonExhaustiveMatch { enum_name, missing_variants, .. }
                    if enum_name == "Direction"
                        && missing_variants.iter().any(|v| v == "Left")
                        && missing_variants.iter().any(|v| v == "Right")
            )
        });
        assert!(
            has_nonexh,
            "expected NonExhaustiveMatch citing Left/Right, got: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_closed_enum_exhaustive_match_is_ok() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        Direction enum { Up, Down, Left, Right }
        describe fn match (d: Direction): Str {
            Direction.Up => { "up" }
            Direction.Down => { "down" }
            Direction.Left => { "left" }
            Direction.Right => { "right" }
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_match_err = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenEnumMatchMissingDefault { .. }
                    | CompilerError::NonExhaustiveMatch { .. }
            )
        });
        assert!(
            !has_match_err,
            "exhaustive closed-enum match should not error: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_closed_enum_match_with_default_is_ok() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        Direction enum { Up, Down, Left, Right }
        describe fn match (d: Direction): Str {
            Direction.Up => { "up" }
            _ => { "other" }
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_nonexh = checker
            .errors
            .errors
            .iter()
            .any(|e| matches!(e, CompilerError::NonExhaustiveMatch { .. }));
        assert!(
            !has_nonexh,
            "closed-enum match with `_` arm should not error: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_open_enum_variant_arrow_enrolls_for_match() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // An arrow `Triangle -> Shape.Tri` should enroll `Tri` in the
        // open enum's variant set. The match below covers only `Tri`,
        // but because the enum is open the type checker still requires
        // a `_` arm. Without one we expect OpenEnumMatchMissingDefault.
        let source = r#"
        Triangle type { base: Dec, height: Dec }
        Shape enum open { }
        Triangle -> Shape.Tri
        describe fn match (s: Shape): Str {
            Shape.Tri => { "triangle" }
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_open_err = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenEnumMatchMissingDefault { enum_name, .. }
                    if enum_name == "Shape"
            )
        });
        assert!(
            has_open_err,
            "expected OpenEnumMatchMissingDefault for open enum extended via ->, got: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_distinct_arrow_targets_do_not_conflict() {
        use crate::lang::parser::parse_hot;

        // Different variant targets on the same source type must NOT be
        // flagged as duplicates - `Shape.Tri` and `Drawable.Tri` are
        // distinct keys in the implementation registry.
        let source = r#"
        Triangle type { base: Dec, height: Dec }
        Shape enum open { }
        Drawable enum open { }
        Triangle -> Shape.Tri
        Triangle -> Drawable.Tri
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        // Other diagnostics may surface (e.g. for unrelated checks), but the
        // ambiguous-implementation error specifically must not.
        let _ = checker.check_program(&program);
        let bad = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                crate::lang::errors::CompilerError::AmbiguousTypeImplementation { .. }
            )
        });
        assert!(
            !bad,
            "distinct arrow targets must not produce an ambiguous-implementation error: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_is_primitive_type_expr_distinguishes_named_shadow() {
        // Built-in primitive variants are primitive.
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Str));
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Int));
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Dec));
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Bool));
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Bytes));
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Null));
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Any));

        // `Optional<primitive>` is also primitive.
        assert!(TypeChecker::is_primitive_type_expr(&TypeExpr::Optional(
            Box::new(TypeExpr::Str)
        )));

        // A user-declared type that shadows a primitive's short name is NOT
        // primitive — this is the whole point of the helper.
        assert!(!TypeChecker::is_primitive_type_expr(&TypeExpr::Named(
            "::myapp/Str".to_string()
        )));
        // Even a bare unqualified `Named("Str")` is not primitive (the parser
        // produces `TypeExpr::Str` for the genuine primitive).
        assert!(!TypeChecker::is_primitive_type_expr(&TypeExpr::Named(
            "Str".to_string()
        )));
        // Plain user types are not primitive.
        assert!(!TypeChecker::is_primitive_type_expr(&TypeExpr::Named(
            "User".to_string()
        )));

        // The legacy name-based check, by contrast, would happily accept the
        // shadowed name — confirming the helpers really do diverge.
        assert!(TypeChecker::is_primitive_type_name("Str"));
    }

    #[test]
    fn test_literal_type_display() {
        assert_eq!(
            format!("{}", LiteralType::Str("apple".to_string())),
            "\"apple\""
        );
        assert_eq!(format!("{}", LiteralType::Int(42)), "42");
        assert_eq!(format!("{}", LiteralType::Dec("3.14".to_string())), "3.14");
        assert_eq!(format!("{}", LiteralType::Bool(true)), "true");
        assert_eq!(format!("{}", LiteralType::Bool(false)), "false");
        assert_eq!(format!("{}", LiteralType::Null), "null");
    }

    #[test]
    fn test_open_literal_union_accumulates_members() {
        use crate::lang::parser::parse_hot;

        // Two top-level declarations of `Fruit` in the same namespace
        // — the second extends the first. The accumulated member set
        // should contain all four members.
        let source = r#"
        ::test::lit ns
        Fruit type open "apple" | "banana"
        Fruit type open | "kiwi"
        Fruit type open "mango" | "pineapple"
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);

        assert!(
            checker.errors.errors.is_empty(),
            "open-union accumulation should not produce errors: {:?}",
            checker.errors.errors
        );

        let entry = checker
            .context
            .types
            .get("Fruit")
            .expect("Fruit type entry");
        assert!(entry.is_literal_union, "Fruit must be a literal union");
        assert!(entry.is_open, "Fruit must be open");
        let members: Vec<&str> = entry
            .literal_members
            .iter()
            .map(|lit| match lit {
                LiteralType::Str(s) => s.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            members,
            vec!["apple", "banana", "kiwi", "mango", "pineapple"],
            "members should accumulate in declaration order"
        );
    }

    #[test]
    fn test_open_literal_union_dedupes_members() {
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::lit ns
        Fruit type open "apple" | "banana"
        Fruit type open | "apple"
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);

        assert!(
            checker.errors.errors.is_empty(),
            "duplicate member should be silently deduped, not error: {:?}",
            checker.errors.errors
        );
        let entry = checker.context.types.get("Fruit").expect("Fruit");
        assert_eq!(
            entry.literal_members.len(),
            2,
            "duplicate member must not be added twice"
        );
    }

    #[test]
    fn test_closed_then_open_literal_union_is_error() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::lit ns
        Fruit type "apple" | "banana"
        Fruit type open | "kiwi"
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_mismatch = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenLiteralUnionMismatch { type_name, .. }
                    if type_name == "Fruit"
            )
        });
        assert!(
            has_mismatch,
            "extending a closed union with an open declaration must emit OpenLiteralUnionMismatch: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_open_then_closed_literal_union_is_error() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::lit ns
        Fruit type open "apple" | "banana"
        Fruit type "kiwi"
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_mismatch = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenLiteralUnionMismatch { type_name, .. }
                    if type_name == "Fruit"
            )
        });
        assert!(
            has_mismatch,
            "re-declaring an open union as closed must emit OpenLiteralUnionMismatch: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_cross_namespace_same_short_name_is_independent() {
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        // Two `Fruit` types in different namespaces — one closed, one
        // open. They are unrelated; the closed↔open mismatch check must
        // NOT fire across namespace boundaries.
        let source = r#"
        ::test::a ns
        Fruit type "apple" | "banana"

        ::test::b ns
        Fruit type open | "kiwi"
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_mismatch = checker
            .errors
            .errors
            .iter()
            .any(|e| matches!(e, CompilerError::OpenLiteralUnionMismatch { .. }));
        assert!(
            !has_mismatch,
            "different namespaces sharing a short name must not trigger the mismatch check: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_open_modifier_on_non_literal_alias_is_error() {
        // Note: this test exercises type-checker semantics directly. The
        // parser's `open` detection requires a leading `|` or literal
        // token, so `Foo type open Int` doesn't reach the type checker
        // through the parser. We construct the AST manually to verify
        // the type-checker guard still fires if a future parser path
        // (or programmatic AST construction) lands here.
        use crate::lang::ast::{Sym, TypeDef, TypeExpr as AstTy};
        use crate::lang::errors::CompilerError;

        let bad = TypeDef {
            name: Sym::String("Foo".to_string()),
            fields: None,
            // `open` modifier with a non-literal alias (using a literal
            // we then wrap in something else would work — here we use a
            // single bool literal mixed with a non-literal stand-in).
            // To keep the assert focused, supply a `Named` alias which
            // is unambiguously not a literal union.
            type_alias: Some(AstTy::Named("Int".to_string())),
            variants: None,
            constructor_functions: None,
            implementations: None,
            is_open: true,
        };

        let mut checker = TypeChecker::new();
        checker.register_enum_metadata("Foo", "::test::lit", &bad, None);
        let has_misuse = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenLiteralUnionMismatch { type_name, message, .. }
                    if type_name == "Foo" && message.contains("only applies to literal unions")
            )
        });
        assert!(
            has_misuse,
            "open modifier on non-literal alias must emit OpenLiteralUnionMismatch: {:?}",
            checker.errors.errors
        );

        // Sanity: also verify the entry was NOT marked as a literal union.
        let entry = checker.context.types.get("Foo").expect("Foo entry");
        assert!(
            !entry.is_literal_union,
            "non-literal alias must not be marked as literal-union despite `open`"
        );
        assert!(entry.literal_members.is_empty());
    }

    #[test]
    fn test_extract_literal_members_pure_union() {
        use crate::lang::ast::{LiteralType as AstLit, TypeExpr as AstTy};

        let alias = AstTy::Union(vec![
            AstTy::Literal(AstLit::Str("a".to_string())),
            AstTy::Literal(AstLit::Str("b".to_string())),
        ]);
        let members = TypeChecker::extract_literal_members(&alias);
        assert_eq!(members.len(), 2);
        assert!(matches!(&members[0], LiteralType::Str(s) if s == "a"));
        assert!(matches!(&members[1], LiteralType::Str(s) if s == "b"));
    }

    #[test]
    fn test_extract_literal_members_single_literal() {
        use crate::lang::ast::{LiteralType as AstLit, TypeExpr as AstTy};

        let alias = AstTy::Literal(AstLit::Int(42));
        let members = TypeChecker::extract_literal_members(&alias);
        assert_eq!(members.len(), 1);
        assert!(matches!(&members[0], LiteralType::Int(42)));
    }

    #[test]
    fn test_extract_literal_members_mixed_returns_empty() {
        use crate::lang::ast::{LiteralType as AstLit, TypeExpr as AstTy};

        // `"a" | Int` is not a pure literal union — must return empty
        // so the caller treats the alias as a non-literal type.
        let alias = AstTy::Union(vec![
            AstTy::Literal(AstLit::Str("a".to_string())),
            AstTy::Named("Int".to_string()),
        ]);
        let members = TypeChecker::extract_literal_members(&alias);
        assert!(
            members.is_empty(),
            "mixed alias must not be treated as literal union"
        );
    }

    #[test]
    fn test_open_literal_union_compatibility_matches_closed() {
        // Sanity check: the accumulated open-union TypeExpr produces the
        // same `types_are_compatible` answers as the equivalent closed
        // union. Open literal unions should not introduce any new
        // leniency or strictness in subtype checks — they share the
        // same shape (`Union<Literal<...>>`) and the same compatibility
        // code path.
        use crate::lang::parser::parse_hot;

        let open_src = r#"
        ::test::lit ns
        Fruit type open "apple" | "banana"
        Fruit type open | "kiwi"
        "#;
        let closed_src = r#"
        ::test::lit ns
        Fruit type "apple" | "banana" | "kiwi"
        "#;

        let mut open_checker = TypeChecker::new();
        let _ = open_checker.check_program(&parse_hot(open_src).expect("parse open"));
        let mut closed_checker = TypeChecker::new();
        let _ = closed_checker.check_program(&parse_hot(closed_src).expect("parse closed"));

        let open_ty = open_checker
            .qualified_types
            .get("::test::lit/Fruit")
            .cloned()
            .or_else(|| open_checker.qualified_types.get("Fruit").cloned())
            .expect("open Fruit must be registered");
        let closed_ty = closed_checker
            .qualified_types
            .get("::test::lit/Fruit")
            .cloned()
            .or_else(|| closed_checker.qualified_types.get("Fruit").cloned())
            .expect("closed Fruit must be registered");

        // Probe a mix of in-set and out-of-set literals; the answers
        // should be identical for both checkers.
        let probes = [
            TypeExpr::Literal(LiteralType::Str("apple".to_string())),
            TypeExpr::Literal(LiteralType::Str("kiwi".to_string())),
            TypeExpr::Literal(LiteralType::Str("grape".to_string())),
            TypeExpr::Str,
            TypeExpr::Int,
        ];
        for probe in probes.iter() {
            assert_eq!(
                open_checker.types_are_compatible(&open_ty, probe),
                closed_checker.types_are_compatible(&closed_ty, probe),
                "open and closed literal unions must agree on compatibility for {:?}",
                probe
            );
        }
    }

    #[test]
    fn test_open_literal_union_nested_extension_is_error() {
        // Open literal-union extensions must live at top level. A
        // `Foo type open | "x"` declaration found inside a function
        // body must trigger OpenLiteralUnionMismatch so the
        // accumulated member set stays trivially auditable from the
        // top-level scope alone.
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::lit ns
        Fruit type open "apple" | "banana"
        nested fn () {
          Fruit type open | "kiwi"
          "apple"
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_nested = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::OpenLiteralUnionMismatch { type_name, message, .. }
                    if type_name == "Fruit" && message.contains("top level")
            )
        });
        assert!(
            has_nested,
            "nested open literal-union extension must emit OpenLiteralUnionMismatch: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_nested_arrow_inside_function_is_error() {
        // Type-implementation arrows mutate the global impl_registry, so
        // declaring one inside a function body is a hidden global side
        // effect. Mirror the top-level-only rule used for open literal-
        // union extensions and reject the construct with
        // `NestedTypeImplementation` so users see one clear diagnostic
        // instead of confusing cross-test impl pollution at runtime.
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::nested-arrow ns
        Date type { year: Int, month: Int, day: Int }
        offender fn () {
          Date -> Str fn (d: Date): Str {
            `${d.year}-${d.month}-${d.day}`
          }
          Str(Date({year: 2025, month: 1, day: 1}))
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_nested = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::NestedTypeImplementation { source_type, target_type, .. }
                    if source_type == "Date" && target_type == "Str"
            )
        });
        assert!(
            has_nested,
            "nested `Date -> Str` arrow must emit NestedTypeImplementation: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_nested_arrow_attached_to_inline_typedef_is_error() {
        // Even when the arrow is part of a `type { ... } impl { ... }` block
        // declared inside a function, it still leaks globally. Reject it.
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::nested-arrow-inline ns
        offender fn () {
          Score type {
            value: Int
          }

          Score -> Int fn (s: Score): Int {
            s.value
          }

          Int(Score({value: 42}))
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_nested = checker.errors.errors.iter().any(|e| {
            matches!(
                e,
                CompilerError::NestedTypeImplementation { source_type, target_type, .. }
                    if source_type == "Score" && target_type == "Int"
            )
        });
        assert!(
            has_nested,
            "nested `Score -> Int` arrow must emit NestedTypeImplementation: {:?}",
            checker.errors.errors
        );
    }

    #[test]
    fn test_top_level_arrow_is_allowed() {
        // The mirror of the above: top-level arrows must continue to work
        // without complaint. Sanity-check that we did not over-reject.
        use crate::lang::errors::CompilerError;
        use crate::lang::parser::parse_hot;

        let source = r#"
        ::test::top-arrow ns
        Date type { year: Int, month: Int, day: Int }
        Date -> Str fn (d: Date): Str {
          `${d.year}-${d.month}-${d.day}`
        }
        "#;
        let program = parse_hot(source).expect("parse");
        let mut checker = TypeChecker::new();
        let _ = checker.check_program(&program);
        let has_nested = checker
            .errors
            .errors
            .iter()
            .any(|e| matches!(e, CompilerError::NestedTypeImplementation { .. }));
        assert!(
            !has_nested,
            "top-level arrows must not be flagged as nested: {:?}",
            checker.errors.errors
        );
    }
}
