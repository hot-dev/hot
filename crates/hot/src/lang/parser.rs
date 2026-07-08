// Parser for Hot source code.

use crate::lang::ast::{
    AstNode, BuiltinType, DeepPath, DeepPathPart, Flow, FlowType, FnArg, FnArgs, FnCall, FnCallArg,
    FnDef, Lambda, LiteralType, MatchArm, MatchExpr, Meta, Namespace, NamespaceAliases, NsPath,
    NsPathPart, NsRef, Program, Ref, ResultModifier, Scope, Source, Sym, TemplateLiteral,
    TemplatePart, TypeDef, TypeExpr, TypeImplementation, Value, Var, VarRef,
};
use crate::lang::lexer::{KeywordKind, Lexer, Token, TokenType};
use crate::val::Val;
use ariadne::{ColorGenerator, Config, Label, Report, ReportKind};
use indexmap::IndexMap;
use std::iter::Iterator;
use std::path::PathBuf;

/// Location information for parse errors.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorLocation {
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub length: usize,
    pub file: Option<PathBuf>,
}
/// Parse errors with rich source context.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub location: ErrorLocation,
    pub source_context: Option<String>, // For ariadne pretty printing
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file_prefix = if let Some(ref file) = self.location.file {
            format!("{}:", file.display())
        } else {
            String::new()
        };

        write!(
            f,
            "{}Parse error at line {}, column {}: {}",
            file_prefix, self.location.line, self.location.column, self.message
        )
    }
}

impl ParseError {
    /// Format this error using ariadne source context.
    /// When `color` is true, output includes ANSI color codes for terminal display.
    pub fn format_error(&self, source: &str, color: bool) -> Option<String> {
        // Validate position is within source bounds
        let source_len = source.len();
        if self.location.position > source_len {
            // Position is beyond end of file, create a simple error message
            return Some(format!(
                "Parse error at end of file (line {}, column {}): {}",
                self.location.line, self.location.column, self.message
            ));
        }

        let mut colors = ColorGenerator::new();
        let label_color = colors.next();

        let span_start = self.location.position;
        let span_end = (self.location.position + self.location.length.max(1)).min(source_len);

        let source_name = if let Some(ref file) = self.location.file {
            file.display().to_string()
        } else {
            "<source>".to_string()
        };

        let report = Report::build(
            ReportKind::Error,
            (source_name.as_str(), span_start..span_end),
        )
        .with_config(Config::default().with_color(color))
        .with_code("parse-error")
        .with_message("Parse Error")
        .with_label(
            Label::new((source_name.as_str(), span_start..span_end))
                .with_message(&self.message)
                .with_color(label_color),
        );

        let mut output = Vec::new();
        match report.finish().write(
            (source_name.as_str(), ariadne::Source::from(source)),
            &mut output,
        ) {
            Ok(_) => String::from_utf8(output).ok(),
            Err(_) => {
                // Ariadne failed, create a simple error message
                Some(format!(
                    "Parse error at line {}, column {}: {}",
                    self.location.line, self.location.column, self.message
                ))
            }
        }
    }
}

impl std::error::Error for ParseError {}

pub type ParseResult<T> = Result<T, ParseError>;

/// Parser for the Hot grammar.
pub struct Parser {
    tokens: Vec<Token>,
    token_index: usize,
    current_namespace: NsPath,
    file_path: Option<PathBuf>,
    source_content: Option<String>, // Store source for error reporting
    var_counter: usize,             // Counter for auto-generated variable names
}

const NAMESPACE_VAR: &str = "ns";

/// Merge trailing meta (from after a type/enum definition) onto `var.meta`.
/// If `var.meta` already has a map, trailing keys are merged in (trailing wins on conflict).
/// Otherwise the trailing val replaces any existing meta.
fn merge_trailing_meta(var: &mut Var, trailing: Val) {
    match var.meta.take() {
        Some(Meta {
            val: Val::Map(mut existing),
        }) => {
            if let Val::Map(trailing_map) = trailing {
                existing.extend(*trailing_map);
            }
            var.meta = Some(Meta {
                val: Val::Map(existing),
            });
        }
        Some(_) => {
            var.meta = Some(Meta { val: trailing });
        }
        None => {
            var.meta = Some(Meta { val: trailing });
        }
    }
}

const REMOVED_RESULT_MODIFIER_MSG: &str = "The |map, |vec, and |one result modifiers were removed in Hot 2.6.0. Annotate the binding or return type instead: `x: All<Map> cond { ... }`, `: All<Vec>`, or `: One`";

impl Parser {
    pub fn new() -> Self {
        Self {
            tokens: Vec::new(),
            token_index: 0,
            current_namespace: NsPath::new(),
            file_path: None,
            source_content: None,
            var_counter: 0,
        }
    }

    pub fn with_file_context(mut self, file_path: String, source_content: String) -> Self {
        self.file_path = Some(PathBuf::from(file_path));
        self.source_content = Some(source_content);
        self
    }

    /// Parse a single Hot expression (for eval2 command)
    /// This now just uses the same parsing logic as files, but returns the last variable's value
    pub fn parse_expression(&mut self, source: &str) -> ParseResult<Value> {
        // Set up a default namespace for eval
        self.current_namespace = NsPath::from_vec(vec![
            NsPathPart::Sym(Sym::String("hot".to_string())),
            NsPathPart::Sym(Sym::String("run".to_string())),
        ]);

        // Wrap the expression in a variable assignment for proper Hot syntax
        let wrapped_source = format!("result {}", source);

        // Parse as a program (same as file parsing)
        let program = self.parse(&wrapped_source)?;

        // Find the last variable in the current namespace and return its value
        if let Some(namespace) = program.namespaces.get(&self.current_namespace)
            && let Some((_, last_value)) = namespace.scope.vars.iter().last()
        {
            return Ok(last_value.clone());
        }

        // If no variables found, return null
        Ok(Value::Val(Val::Null, None))
    }

    /// Parse a coercion-arrow target type. Accepts:
    /// - simple identifier: `Str`
    /// - dotted enum-variant form: `Shape.Tri`
    /// - namespaced: `::hot::type/Str`
    /// - namespaced + dotted: `::ns/Shape.Tri`
    ///
    /// The dotted suffix is folded into the returned string (e.g.
    /// `"Shape.Tri"`), matching how variant constructors are referenced
    /// elsewhere in the AST.
    fn parse_arrow_target_type(&mut self) -> ParseResult<String> {
        let base = if self.check(&TokenType::DoubleColon) {
            // Parse namespace reference like ::hot::type/Str manually
            let mut type_parts = Vec::new();

            self.next(); // consume "::"

            // Parse the full qualified name (keywords allowed in paths)
            loop {
                let part = self.expect_identifier_or_keyword()?;
                type_parts.push(part);

                if self.check(&TokenType::DoubleColon) {
                    self.next(); // consume "::"
                    type_parts.push("::".to_string());
                } else if self.check(&TokenType::Slash) {
                    self.next(); // consume "/"
                    type_parts.push("/".to_string());
                    let final_part = self.expect_identifier_or_keyword()?;
                    type_parts.push(final_part);
                    break;
                } else {
                    break;
                }
            }

            format!("::{}", type_parts.join(""))
        } else {
            // Simple identifier
            self.expect_identifier()?
        };

        // Optional dotted variant suffix: `Shape.Tri` (single segment only;
        // multi-level `Shape.A.B` is not a valid variant reference).
        if self.check(&TokenType::Dot) {
            // Only consume the dot if the next token is an identifier — the
            // dot might also belong to whatever follows (e.g. trailing meta).
            // We require an immediate identifier here for the variant suffix.
            if let Some(next) = self.peek_n(1)
                && let TokenType::Identifier(_) = &next.token_type
            {
                self.next(); // consume `.`
                let variant = self.expect_identifier()?;
                return Ok(format!("{base}.{variant}"));
            }
        }

        Ok(base)
    }

    /// Synthesize the trivial wrap function for a bodyless coercion arrow
    /// like `Triangle -> Shape.Tri`. Generates the equivalent of:
    ///
    /// ```hot
    /// fn (s: Source): Target { Target(s) }
    /// ```
    ///
    /// where `Target(s)` is a function call whose function value is the
    /// dotted variant constructor (e.g. `Shape.Tri`). For non-dotted
    /// targets (e.g. plain `Str`) the body is `Str(s)` — also a valid
    /// coercion shape, though such bodyless plain-type arrows are unusual.
    fn synthesize_wrap_function(source_type: &str, target_type: &str) -> Value {
        // Parameter name: pick something unlikely to collide with anything
        // a user might write inside the (synthesized) body. The body never
        // references the user's scope, so any name works; we use `$src`.
        let param_name = "$src".to_string();

        // Build the parameter list: a single arg of type `source_type`.
        let args = FnArgs {
            args: vec![FnArg {
                var: Var {
                    sym: Sym::String(param_name.clone()),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                lazy: false,
                type_annotation: Some(source_type.to_string()),
            }],
            variadic: false,
        };

        // Build the body: a call to the target constructor with `$src`.
        // For a dotted target like `Shape.Tri`, we model the function as
        // a Var ref `Shape` with deep_path `["Tri"]`, mirroring how
        // `Shape.Tri(...)` would be parsed in user code. For plain
        // targets, we just reference the type by name.
        let function_value: Value = if let Some((base, variant)) = target_type.split_once('.') {
            Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String(base.to_string()),
                    deep_set: None,
                    deep_path: Some(DeepPath::from_vec(vec![DeepPathPart::Key(
                        variant.to_string(),
                    )])),
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            }))
        } else {
            Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String(target_type.to_string()),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            }))
        };

        let arg_ref = Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String(param_name),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: None,
        }));

        let body = Value::FnCall(FnCall {
            function: Box::new(function_value),
            args: vec![FnCallArg {
                value: arg_ref,
                lazy: false,
                spread: false,
                src: None,
            }],
            result_path: None,
            src: None,
        });

        Value::Fn(vec![FnDef {
            args,
            body,
            return_type: Some(target_type.to_string()),
        }])
    }

    fn apply_type_implementation_inference(
        function: Value,
        source_type: &str,
        target_type: &str,
    ) -> Value {
        if let Value::Fn(fn_defs) = function {
            let updated_defs: Vec<FnDef> = fn_defs
                .into_iter()
                .map(|mut fn_def| {
                    // Infer first argument type from source_type if not explicitly annotated
                    if let Some(first_arg) = fn_def.args.args.first_mut()
                        && first_arg.type_annotation.is_none()
                    {
                        first_arg.type_annotation = Some(source_type.to_string());
                    }

                    // Infer return type from target_type if not explicitly annotated
                    if fn_def.return_type.is_none() {
                        fn_def.return_type = Some(target_type.to_string());
                    }

                    fn_def
                })
                .collect();
            Value::Fn(updated_defs)
        } else {
            function
        }
    }

    /// Parse Hot source code.
    pub fn parse(&mut self, source: &str) -> ParseResult<Program> {
        tracing::trace!("Parsing source: '{}'", source);
        // Tokenize the source before parsing.
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        // Check for lexer errors before proceeding
        if !lexer.errors().is_empty() {
            let first_error = &lexer.errors()[0];
            return Err(ParseError {
                message: format!("Lexer error: {}", first_error.kind),
                location: ErrorLocation {
                    line: first_error.line,
                    column: first_error.column,
                    position: first_error.position,
                    length: 1,
                    file: first_error.file.clone(),
                },
                source_context: Some(source.to_string()),
            });
        }

        self.tokens = tokens;
        self.token_index = 0;

        // Parse into a program with an initial default namespace.
        let mut hot_program = Program {
            namespaces: IndexMap::new(),
            current_namespace: self.current_namespace.clone(),
        };

        // Create default namespace
        hot_program.namespaces.insert(
            self.current_namespace.clone(),
            Namespace {
                path: self.current_namespace.clone(),
                scope: Scope {
                    vars: IndexMap::new(),
                },
                meta: None,
                source_file: self.file_path.clone(),
                aliases: NamespaceAliases::new(),
            },
        );

        // Process tokens until the end of input.
        while self.peek().is_some() {
            // Skip EOF tokens and newlines
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Eof) {
                    // EOF encountered - exit main parsing loop
                    return Ok(hot_program);
                } else if matches!(token.token_type, TokenType::Newline) {
                    self.next(); // consume newline
                } else {
                    break;
                }
            }

            // Check if we've reached the end after skipping tokens (empty file case)
            if self.peek().is_none() {
                break;
            }

            // Check if this looks like a namespace declaration, alias, or expression (starts with ::)
            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::DoubleColon)
            {
                // Save position for potential backtrack
                let saved_index = self.token_index;

                // Try to parse as a namespace declaration first: ::path [#metadata] ns
                if let Some((ns_path, ns_meta)) = self.try_parse_namespace_declaration()? {
                    // Successfully parsed a namespace declaration
                    self.current_namespace = ns_path.clone();

                    // Make sure the target namespace exists and set its metadata
                    if !hot_program.namespaces.contains_key(&self.current_namespace) {
                        hot_program.namespaces.insert(
                            self.current_namespace.clone(),
                            Namespace {
                                path: self.current_namespace.clone(),
                                scope: Scope {
                                    vars: IndexMap::new(),
                                },
                                meta: ns_meta.clone(),
                                source_file: self.file_path.clone(),
                                aliases: NamespaceAliases::new(),
                            },
                        );
                    } else if let Some(meta) = ns_meta.clone()
                        && let Some(namespace) =
                            hot_program.namespaces.get_mut(&self.current_namespace)
                    {
                        namespace.meta = Some(meta);
                    }
                    hot_program.current_namespace = self.current_namespace.clone();

                    // Create the ns variable with the namespace reference as its value
                    let ns_ref = NsRef {
                        ns: ns_path.clone(),
                        src: None,
                        function_name: None,
                    };
                    let var = Var {
                        sym: Sym::String(NAMESPACE_VAR.to_string()),
                        deep_set: None,
                        deep_path: None,
                        meta: ns_meta,
                        type_annotation: None,
                        src: None,
                    };
                    let value = Value::Ref(Ref::Ns(ns_ref));

                    // Store the ns variable in the namespace
                    if let Some(namespace) = hot_program.namespaces.get_mut(&self.current_namespace)
                    {
                        namespace.scope.vars.insert(var, value);
                    }

                    tracing::trace!(
                        "Parser: Namespace declaration switching to '{}'",
                        self.current_namespace
                    );
                    continue;
                }

                // Not a namespace declaration - restore tokens and try alias
                self.token_index = saved_index;

                // Try to parse as an alias: ::alias ::source::path
                // An alias is two namespace paths separated by whitespace
                if let Some((alias_path, source_path)) = self.try_parse_namespace_alias()? {
                    // Successfully parsed an alias
                    if let Some(namespace) = hot_program.namespaces.get_mut(&self.current_namespace)
                    {
                        tracing::trace!(
                            "Parser: Adding namespace alias {} -> {} in namespace {}",
                            alias_path,
                            source_path,
                            self.current_namespace
                        );
                        namespace.aliases.insert(alias_path, source_path);
                    }
                    continue;
                }

                // Not an alias - restore tokens and parse as expression
                self.token_index = saved_index;

                let expr_value = self.parse_value()?;

                // Not an alias - this is a regular expression (like a function call)
                // Create an auto-named variable so multiple expressions don't overwrite each other
                let auto_name = format!("$var_{}", self.var_counter);
                self.var_counter += 1;
                let var = Var {
                    sym: Sym::String(auto_name),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                };

                // Store in current namespace
                if let Some(namespace) = hot_program.namespaces.get_mut(&self.current_namespace) {
                    namespace.scope.vars.insert(var, expr_value);
                }

                // Continue to next iteration
                continue;
            }

            // Check for implementation declarations: TypeName -> TargetType fn ...
            if let Some(current_token) = self.peek().cloned()
                && let TokenType::Identifier(type_name) = &current_token.token_type
                && let Some(next_token) = self.peek_n(1)
                && matches!(next_token.token_type, TokenType::Implements)
            {
                // Capture the source span at the start of the declaration
                // so duplicate-arrow diagnostics can cite both sites.
                let impl_src = Source::new(
                    self.file_path.as_ref().map(|p| p.display().to_string()),
                    current_token.line as usize,
                    current_token.column as usize,
                    current_token.position,
                    type_name.len(),
                );

                // This is an implementation declaration, parse it specially
                self.next(); // consume type name
                self.next(); // consume "->"

                // Parse target type — supports plain (`Str`), dotted variant
                // form (`Shape.Tri`), and namespaced (`::ns/Type[.Variant]`).
                let target_type = self.parse_arrow_target_type()?;

                // Skip newlines before meta or fn
                while let Some(token) = self.peek() {
                    if matches!(token.token_type, TokenType::Newline) {
                        self.next();
                    } else {
                        break;
                    }
                }

                // Parse meta and fn in any order
                let mut impl_meta = None;

                // Check for optional meta before fn
                if let Some(token) = self.peek()
                    && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                {
                    let is_metadata = self.peek_n(1).is_some_and(|t| {
                        matches!(
                            t.token_type,
                            TokenType::LeftBrace
                                | TokenType::LeftBracket
                                | TokenType::String(_)
                                | TokenType::Int(_)
                                | TokenType::Bool(_)
                                | TokenType::Null
                        )
                    });

                    if is_metadata {
                        self.next(); // consume 'meta' token
                        let meta_value = self.parse_primary_value()?;
                        if let Value::Val(val, _) = meta_value {
                            impl_meta = Some(Meta { val });
                        } else {
                            return Err(self.error("Metadata must be a literal value"));
                        }

                        // Skip newlines before fn
                        while let Some(token) = self.peek() {
                            if matches!(token.token_type, TokenType::Newline) {
                                self.next();
                            } else {
                                break;
                            }
                        }
                    }
                }

                // `fn` is now optional. Bodyless form
                // (`Source -> Enum.Variant` with no `fn`) synthesizes a
                // trivial wrap function. With `fn`, the user-supplied body
                // is parsed normally.
                let has_fn = matches!(
                    self.peek().map(|t| &t.token_type),
                    Some(TokenType::Keyword(KeywordKind::Fn))
                );
                let function = if has_fn {
                    self.next(); // consume "fn"
                    self.parse_function()?
                } else {
                    Self::synthesize_wrap_function(type_name, &target_type)
                };

                // Check for trailing meta after fn (if meta wasn't before fn)
                if impl_meta.is_none() {
                    while let Some(token) = self.peek() {
                        if matches!(token.token_type, TokenType::Newline) {
                            self.next();
                        } else {
                            break;
                        }
                    }

                    if let Some(token) = self.peek()
                        && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                    {
                        let is_metadata = self.peek_n(1).is_some_and(|t| {
                            matches!(
                                t.token_type,
                                TokenType::LeftBrace
                                    | TokenType::LeftBracket
                                    | TokenType::String(_)
                                    | TokenType::Int(_)
                                    | TokenType::Bool(_)
                                    | TokenType::Null
                            )
                        });

                        if is_metadata {
                            self.next(); // consume 'meta' token
                            let meta_value = self.parse_primary_value()?;
                            if let Value::Val(val, _) = meta_value {
                                impl_meta = Some(Meta { val });
                            }
                        }
                    }
                }

                // Apply type inference: fill in missing arg/return types from implementation declaration
                let function =
                    Self::apply_type_implementation_inference(function, type_name, &target_type);

                // Create a type implementation
                let implementation = TypeImplementation {
                    source_type: type_name.clone(),
                    target_type,
                    implementation: function,
                    src: Some(impl_src.clone()),
                };

                // Create a dummy variable for the implementation. The
                // namespace's `vars` map is keyed by `Var.sym`, so two
                // arrows like `Triangle -> Shape.Tri` declared twice would
                // otherwise collide and the second silently replace the
                // first - hiding the duplicate from the type checker. We
                // disambiguate by appending the source position of the
                // arrow declaration to the synthetic sym.
                let var = Var {
                    sym: Sym::String(format!(
                        "$impl_{}_{}@{}",
                        type_name, implementation.target_type, impl_src.position
                    )),
                    deep_set: None,
                    deep_path: None,
                    meta: impl_meta,
                    type_annotation: None,
                    src: None,
                };

                // Store in current namespace
                if let Some(namespace) = hot_program.namespaces.get_mut(&self.current_namespace) {
                    namespace
                        .scope
                        .vars
                        .insert(var, Value::TypeImplementation(Box::new(implementation)));
                }

                // Continue to next iteration without storing anything under the type name
                continue;
            }

            // Check for common control flow syntax mistakes from other languages
            // Users may try: `if { ... }` or `if { ... } else { ... }`
            let control_flow_mistake = if let Some(current_token) = self.peek().cloned()
                && let TokenType::Identifier(name) = &current_token.token_type
                && (name == "if" || name == "else")
            {
                // Check if next non-newline token is LeftBrace
                let mut lookahead_idx = 1;
                while let Some(token) = self.peek_n(lookahead_idx) {
                    if matches!(token.token_type, TokenType::Newline) {
                        lookahead_idx += 1;
                    } else {
                        break;
                    }
                }
                if let Some(next_token) = self.peek_n(lookahead_idx)
                    && matches!(next_token.token_type, TokenType::LeftBrace)
                {
                    Some(name.clone())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(name) = control_flow_mistake {
                let suggestion = if name == "if" {
                    "In Hot, `if` is a function: if(pred, then) or if(pred, then, else)\n\
                     For multiple branches, use `cond`: cond { pred1 => result1 pred2 => result2 => default }"
                } else {
                    "`else` is not a keyword in Hot. Use the `if` function: if(pred, then, else)\n\
                     Or use `cond` for multiple branches: cond { pred1 => result1 => default }"
                };
                return Err(self.error(suggestion));
            }

            // Whitespace distinguishes calls/indexing from assignment forms.
            let is_standalone_value = if let Some(current_token) = self.peek().cloned() {
                if let TokenType::Identifier(_) = &current_token.token_type {
                    // Look at the next token to determine if this is standalone
                    if let Some(next_token) = self.peek_n(1) {
                        match &next_token.token_type {
                            TokenType::LeftParen => {
                                // No whitespace before `(` means this is a call.
                                let var_name_len = if let TokenType::Identifier(name) =
                                    &current_token.token_type
                                {
                                    name.len()
                                } else {
                                    0
                                };
                                let has_whitespace =
                                    next_token.column > current_token.column + var_name_len as u32;
                                !has_whitespace // No whitespace = function call (standalone)
                            }
                            TokenType::LeftBracket => {
                                // Check for whitespace: a[0] (index access) vs a [0] (variable assignment)
                                let var_name_len = if let TokenType::Identifier(name) =
                                    &current_token.token_type
                                {
                                    name.len()
                                } else {
                                    0
                                };
                                let has_whitespace =
                                    next_token.column > current_token.column + var_name_len as u32;

                                // Check if this is empty brackets (append syntax: items[] value)
                                // If so, it's a variable assignment, not a standalone expression
                                let is_append = self.peek_n(2).is_some_and(|t| {
                                    matches!(t.token_type, TokenType::RightBracket)
                                });

                                !has_whitespace && !is_append // No whitespace AND not append = standalone
                            }
                            TokenType::RightBrace | TokenType::Comma => true, // End of scope or followed by comma
                            _ => false, // Likely a variable assignment
                        }
                    } else {
                        true // End of input, treat as standalone
                    }
                } else {
                    false
                }
            } else {
                false
            };

            let (var, value) = if is_standalone_value {
                // Parse as a standalone value and create an auto-named variable
                let value = self.parse_value()?;
                let auto_name = format!("$var_{}", self.var_counter);
                self.var_counter += 1;
                let var = Var {
                    sym: Sym::String(auto_name),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                };
                (var, value)
            } else {
                // Try to parse as variable definition, but handle bare expressions too
                match self.try_parse_var_definition() {
                    Ok((var, value)) => {
                        tracing::trace!(
                            "Successfully parsed as variable definition: '{}' = {:?}",
                            var.sym.name(),
                            value
                        );
                        (var, value)
                    }
                    Err(e) => {
                        tracing::trace!(
                            "Failed to parse as variable definition ({}), treating as bare expression",
                            e
                        );
                        // This might be a bare expression, create an auto-named variable
                        let value = self.parse_value()?;
                        let auto_name = format!("$var_{}", self.var_counter);
                        self.var_counter += 1;
                        let var = Var {
                            sym: Sym::String(auto_name.clone()),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        };
                        tracing::trace!(
                            "Created auto-named variable '{}' for bare expression: {:?}",
                            auto_name,
                            value
                        );
                        (var, value)
                    }
                }
            };

            // Handle namespace declaration fallback
            // Note: Primary namespace declarations (::path ns) are handled in the DoubleColon block above.
            // This handles the 'ns' variable that gets created for namespace declarations.
            if var.sym.name() == NAMESPACE_VAR {
                // Update the current namespace based on the ns value
                if let Value::Ref(Ref::Ns(ns_ref)) = &value {
                    self.current_namespace = ns_ref.ns.clone();

                    // Make sure the target namespace exists and set its metadata
                    if !hot_program.namespaces.contains_key(&self.current_namespace) {
                        hot_program.namespaces.insert(
                            self.current_namespace.clone(),
                            Namespace {
                                path: self.current_namespace.clone(),
                                scope: Scope {
                                    vars: IndexMap::new(),
                                },
                                meta: var.meta.clone(),
                                source_file: self.file_path.clone(),
                                aliases: NamespaceAliases::new(),
                            },
                        );
                    } else if let Some(meta) = var.meta.clone()
                        && let Some(namespace) =
                            hot_program.namespaces.get_mut(&self.current_namespace)
                    {
                        namespace.meta = Some(meta);
                    }
                    hot_program.current_namespace = self.current_namespace.clone();
                }
                // Don't continue - let the ns declaration be added as a variable
                // so the compiler can process it and generate SetNamespace instructions
            }

            // Add variable to current namespace
            if let Some(namespace) = hot_program.namespaces.get_mut(&self.current_namespace) {
                tracing::trace!(
                    "Adding variable '{}' to namespace '{}' with value: {:?}",
                    var.sym.name(),
                    self.current_namespace,
                    value
                );
                namespace.scope.vars.insert(var, value);
            } else {
                tracing::error!(
                    "Failed to find namespace '{}' to add variable '{}'",
                    self.current_namespace,
                    var.sym.name()
                );
            }
        }

        // Note: Variable reference resolution is now handled at the engine level
        // after all programs are merged, so that eval code can access imported functions

        Ok(hot_program)
    }

    // Note: Variable reference resolution is now handled at the engine level
    // after all programs are merged, so that eval code can access imported functions

    /// Parse a variable definition.
    /// Try to parse as a variable definition, return error if it's not one
    fn try_parse_var_definition(&mut self) -> ParseResult<(Var, Value)> {
        // Save current position in case we need to backtrack
        let saved_index = self.token_index;

        match self.parse_var_definition() {
            Ok(result) => Ok(result),
            Err(e) => {
                // Restore tokens and return error
                self.token_index = saved_index;
                Err(e)
            }
        }
    }

    fn parse_var_definition(&mut self) -> ParseResult<(Var, Value)> {
        // Skip newlines
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next();
            } else {
                break;
            }
        }

        // Parse variable name first
        let (var_name, var_src, var_name_end_pos) = match self.next() {
            Some(token) => match &token.token_type {
                TokenType::Identifier(name) => {
                    tracing::trace!("Parsing variable definition for '{}'", name);
                    let src = Source::new(
                        self.file_path.as_ref().map(|p| p.display().to_string()),
                        token.line as usize,
                        token.column as usize,
                        token.position,
                        name.len(),
                    );
                    let end_pos = token.position + token.length;
                    (name.clone(), Some(src), end_pos)
                }
                _ => {
                    return Err(
                        self.error(&format!("Expected identifier, got {:?}", token.token_type))
                    );
                }
            },
            None => return Err(self.error("Expected identifier, got EOF")),
        };

        // Check for metadata after variable name (e.g., var_name meta ["test"] or var_name #1)
        let mut var_meta = None;

        // Skip newlines before checking for metadata
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
                continue;
            }
            break;
        }

        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
        {
            // Check if next token after 'meta' is { or [ (metadata) vs ( (function call)
            // meta {...} or meta [...] = metadata declaration
            // meta(...) = function call to meta function
            let is_metadata = self.peek_n(1).is_some_and(|t| {
                matches!(
                    t.token_type,
                    TokenType::LeftBrace
                        | TokenType::LeftBracket
                        | TokenType::String(_)
                        | TokenType::Int(_)
                        | TokenType::Bool(_)
                        | TokenType::Null
                )
            });

            if is_metadata {
                self.next(); // consume 'meta' token

                // Parse the metadata value directly (map, vector, literal, etc.)
                let meta_value = self.parse_primary_value()?;
                if let Value::Val(val, _) = meta_value {
                    var_meta = Some(Meta { val });
                } else {
                    return Err(self.error("Metadata must be a literal value"));
                }
            }
        }

        // After parsing metadata, check for function definition immediately
        // Skip newlines before checking for 'fn'
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
                continue;
            }
            break;
        }

        // Check for function definition: var #[metadata] fn ...
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Fn))
        {
            self.next(); // consume "fn"
            let function = self.parse_function()?;
            let var = Var {
                sym: Sym::String(var_name),
                deep_set: None,
                deep_path: None,
                meta: var_meta,
                type_annotation: None,
                src: var_src,
            };
            return Ok((var, function));
        }

        let mut var = Var {
            sym: Sym::String(var_name.clone()),
            deep_set: None,
            deep_path: None,
            meta: var_meta, // Use variable metadata if found
            type_annotation: None,
            src: var_src,
        };

        // Check for deep path: var.path.to.value or var.path[0] (dots and brackets for variable definitions)
        let mut deep_path_parts = Vec::new();
        // Track the end position of the last token for adjacency check
        let mut last_token_end = var_name_end_pos;
        loop {
            if self.check(&TokenType::Dot) {
                self.next(); // consume dot

                // Expect identifier after dot
                let part_name = match self.next() {
                    Some(token) => {
                        last_token_end = token.position + token.length;
                        if let Some(name) = self.extract_property_name(&token.token_type) {
                            name
                        } else {
                            return Err(self.error(&format!(
                                "Expected identifier after dot, got {:?}",
                                token.token_type
                            )));
                        }
                    }
                    None => return Err(self.error("Expected identifier after dot")),
                };

                deep_path_parts.push(DeepPathPart::Key(part_name));
            } else if self.check(&TokenType::LeftBracket) {
                // Check if the bracket is immediately adjacent (no whitespace)
                // If there's whitespace, it's a value like `items [1, 2, 3]`, not a path like `items[0]`
                let bracket_token_pos = self.peek().map(|t| t.position);
                let bracket_is_adjacent =
                    bracket_token_pos.is_some_and(|pos| pos == last_token_end);

                if !bracket_is_adjacent {
                    break; // This is a value (has whitespace before [), not a path index
                }

                // Peek ahead to see if this is [] (append), [int] (index), or [value...] (array literal)
                let next_token = self.peek_n(1);
                let is_append = next_token
                    .as_ref()
                    .is_some_and(|t| matches!(t.token_type, TokenType::RightBracket));
                let is_index = next_token
                    .as_ref()
                    .is_some_and(|t| matches!(t.token_type, TokenType::Int(_)))
                    && self
                        .peek_n(2)
                        .is_some_and(|t| matches!(t.token_type, TokenType::RightBracket));

                if !is_append && !is_index {
                    break; // This is a value, not a path index or append
                }

                self.next(); // consume [

                if is_append {
                    // Empty brackets: append to vector
                    let bracket_token = self.next().unwrap(); // consume ]
                    last_token_end = bracket_token.position + bracket_token.length;
                    deep_path_parts.push(DeepPathPart::Append);
                } else {
                    // Integer index
                    let index = match self.next() {
                        Some(token) => {
                            if let TokenType::Int(i) = token.token_type {
                                i as usize
                            } else {
                                return Err(self.error(&format!(
                                    "Expected integer index in brackets, got {:?}",
                                    token.token_type
                                )));
                            }
                        }
                        None => return Err(self.error("Expected integer index in brackets")),
                    };

                    // Expect closing bracket
                    if !self.check(&TokenType::RightBracket) {
                        return Err(self.error("Expected ']' after index"));
                    }
                    let bracket_token = self.next().unwrap(); // consume ]
                    last_token_end = bracket_token.position + bracket_token.length;

                    deep_path_parts.push(DeepPathPart::Index(index));
                }
            } else {
                break;
            }
        }

        // If we found deep path parts, set them on the variable
        if !deep_path_parts.is_empty() {
            var.deep_set = Some(DeepPath::from_vec(deep_path_parts));
        }

        // Check for metadata: var #meta_value (can be on same line or next line)
        // Skip newlines to check for metadata on next line
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
                continue;
            }

            if matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta") {
                // Check if next token after 'meta' is { or [ (metadata) vs ( (function call)
                let is_metadata = self.peek_n(1).is_some_and(|t| {
                    matches!(
                        t.token_type,
                        TokenType::LeftBrace
                            | TokenType::LeftBracket
                            | TokenType::String(_)
                            | TokenType::Int(_)
                            | TokenType::Bool(_)
                            | TokenType::Null
                    )
                });

                if is_metadata {
                    self.next(); // consume 'meta' token

                    // Parse the metadata value directly (map, vector, literal, etc.)
                    let meta_value = self.parse_primary_value()?;
                    if let Value::Val(val, _) = meta_value {
                        var.meta = Some(Meta { val });
                    } else {
                        return Err(self.error("Metadata must be a literal value"));
                    }
                }
            }
            break;
        }

        // Skip newlines before checking for type annotation or function definition
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
                continue;
            }
            break;
        }

        // Check for type annotation: var : Type
        let type_annotation = if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Colon)
        {
            self.next(); // consume colon
            Some(self.parse_type_ref()?) // Parse complex type annotations
        } else {
            None
        };
        var.type_annotation = type_annotation.as_ref().map(ToString::to_string);

        // Check for type definition: var type ...
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Type))
        {
            self.next(); // consume "type"
            let type_def = self.parse_type_definition(var.sym.clone())?;

            // Check for trailing meta after type definition (e.g., "Day type meta {...}")
            // Trailing meta is merged onto var.meta so all type meta lives in one place.
            self.skip_newlines();

            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                && self.is_meta_lookahead()
            {
                self.next(); // consume 'meta' token
                let meta_value = self.parse_primary_value()?;
                if let Value::Val(val, _) = meta_value {
                    merge_trailing_meta(&mut var, val);
                }
            }

            return Ok((var, Value::TypeDef(type_def)));
        }

        // Check for enum definition: var enum { ... }
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Enum))
        {
            self.next(); // consume "enum"
            let type_def = self.parse_enum_definition(var.sym.clone())?;

            // Check for trailing meta after enum definition
            self.skip_newlines();

            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                && self.is_meta_lookahead()
            {
                self.next(); // consume 'meta' token
                let meta_value = self.parse_primary_value()?;
                if let Value::Val(val, _) = meta_value {
                    merge_trailing_meta(&mut var, val);
                }
            }

            return Ok((var, Value::TypeDef(type_def)));
        }

        // Check for implementation clause: var -> TargetType fn ...
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Implements)
        {
            self.next(); // consume "->"

            // Parse target type — supports plain (`Str`), dotted variant
            // form (`Shape.Tri`), and namespaced (`::ns/Type[.Variant]`).
            let target_type = self.parse_arrow_target_type()?;

            // Skip newlines before meta or fn
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next();
                } else {
                    break;
                }
            }

            // Check for optional meta before fn
            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
            {
                let is_metadata = self.peek_n(1).is_some_and(|t| {
                    matches!(
                        t.token_type,
                        TokenType::LeftBrace
                            | TokenType::LeftBracket
                            | TokenType::String(_)
                            | TokenType::Int(_)
                            | TokenType::Bool(_)
                            | TokenType::Null
                    )
                });

                if is_metadata {
                    self.next(); // consume 'meta' token
                    let meta_value = self.parse_primary_value()?;
                    if let Value::Val(val, _) = meta_value {
                        var.meta = Some(Meta { val });
                    } else {
                        return Err(self.error("Metadata must be a literal value"));
                    }

                    // Skip newlines before fn
                    while let Some(token) = self.peek() {
                        if matches!(token.token_type, TokenType::Newline) {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
            }

            // `fn` is now optional. Bodyless form
            // (`Source -> Enum.Variant` with no `fn`) synthesizes a
            // trivial wrap function. With `fn`, the user-supplied body
            // is parsed normally.
            let source_type_for_synth = var.sym.name().to_string();
            let has_fn = matches!(
                self.peek().map(|t| &t.token_type),
                Some(TokenType::Keyword(KeywordKind::Fn))
            );
            let function = if has_fn {
                self.next(); // consume "fn"
                self.parse_function()?
            } else {
                Self::synthesize_wrap_function(&source_type_for_synth, &target_type)
            };

            // Check for trailing meta after fn (if meta wasn't before fn)
            if var.meta.is_none() {
                while let Some(token) = self.peek() {
                    if matches!(token.token_type, TokenType::Newline) {
                        self.next();
                    } else {
                        break;
                    }
                }

                if let Some(token) = self.peek()
                    && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                {
                    let is_metadata = self.peek_n(1).is_some_and(|t| {
                        matches!(
                            t.token_type,
                            TokenType::LeftBrace
                                | TokenType::LeftBracket
                                | TokenType::String(_)
                                | TokenType::Int(_)
                                | TokenType::Bool(_)
                                | TokenType::Null
                        )
                    });

                    if is_metadata {
                        self.next(); // consume 'meta' token
                        let meta_value = self.parse_primary_value()?;
                        if let Value::Val(val, _) = meta_value {
                            var.meta = Some(Meta { val });
                        }
                    }
                }
            }

            // Apply type inference: fill in missing arg/return types from implementation declaration
            let source_type = var.sym.name().to_string();
            let function =
                Self::apply_type_implementation_inference(function, &source_type, &target_type);

            // Create a type implementation. Use the var's `src` (which
            // points at the source-type identifier) for duplicate-arrow
            // diagnostics.
            let implementation = TypeImplementation {
                source_type,
                target_type,
                implementation: function,
                src: var.src.clone(),
            };

            return Ok((var, Value::TypeImplementation(Box::new(implementation))));
        }

        // Check for function definition: var fn ... or var >flow fn ...
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Fn))
        {
            self.next(); // consume "fn"
            let function = self.parse_function()?;
            return Ok((var, function));
        }

        // Check for type definition: var type ...
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Type))
        {
            self.next(); // consume "type"
            let type_def = self.parse_type_definition(var.sym.clone())?;

            self.skip_newlines();

            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                && self.is_meta_lookahead()
            {
                self.next(); // consume 'meta' token
                let meta_value = self.parse_primary_value()?;
                if let Value::Val(val, _) = meta_value {
                    merge_trailing_meta(&mut var, val);
                }
            }

            return Ok((var, Value::TypeDef(type_def)));
        }

        // Check for enum definition: var enum { ... }
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Enum))
        {
            self.next(); // consume "enum"
            let type_def = self.parse_enum_definition(var.sym.clone())?;

            self.skip_newlines();

            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                && self.is_meta_lookahead()
            {
                self.next(); // consume 'meta' token
                let meta_value = self.parse_primary_value()?;
                if let Value::Val(val, _) = meta_value {
                    merge_trailing_meta(&mut var, val);
                }
            }

            return Ok((var, Value::TypeDef(type_def)));
        }

        // Skip newlines before parsing the value
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
                continue;
            }
            break;
        }

        // Skip newlines before parsing value (handles multiline assignments)
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
            } else {
                break;
            }
        }

        // Regular variable assignment: var value
        // Check if there's actually a value to parse
        if self.peek().is_some() {
            tracing::trace!("Parsing value for variable '{}'", var.sym.name());
            let value = self.parse_value()?;
            let value = self.apply_all_annotation_to_value(value, type_annotation.as_ref())?;
            tracing::trace!("Parsed value for '{}': {:?}", var.sym.name(), value);

            // Trailing meta: `name value meta {...}`. Mirrors the
            // trailing-meta support already provided for `type`, `enum`,
            // and `->` clauses so users can place `meta {...}` either
            // before or after the value (matching the flexibility they
            // already enjoy on type/impl forms). Trailing entries merge
            // onto any leading meta with trailing keys winning, the
            // same semantics used by `merge_trailing_meta` elsewhere.
            self.skip_newlines();
            if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::Identifier(ref s) if s == "meta")
                && self.is_meta_lookahead()
            {
                self.next(); // consume 'meta'
                let meta_value = self.parse_primary_value()?;
                if let Value::Val(val, _) = meta_value {
                    merge_trailing_meta(&mut var, val);
                } else {
                    return Err(self.error("Metadata must be a literal value"));
                }
            }

            Ok((var, value))
        } else {
            // No value provided, use null as default
            tracing::trace!(
                "No value provided for variable '{}', using null",
                var.sym.name()
            );
            Ok((var, Value::Val(Val::Null, None)))
        }
    }

    /// Parse function definitions.
    fn parse_function(&mut self) -> ParseResult<Value> {
        let mut definitions = Vec::new();

        // Skip newlines after 'fn' keyword
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
            } else {
                break;
            }
        }

        // Check if the first thing after 'fn' is a flow keyword
        let starts_with_flow = if let Some(token) = self.peek() {
            matches!(
                token.token_type,
                TokenType::Keyword(KeywordKind::Serial)
                    | TokenType::Keyword(KeywordKind::Parallel)
                    | TokenType::Keyword(KeywordKind::Cond)
                    | TokenType::Keyword(KeywordKind::CondAll)
            )
        } else {
            false
        };

        if starts_with_flow {
            // Parse multiple flow-based function definitions: >flow (args) { body }, >flow (args) { body }, ...
            loop {
                // Skip newlines before flow keyword
                while let Some(token) = self.peek() {
                    if matches!(token.token_type, TokenType::Newline) {
                        self.next(); // consume newline
                    } else {
                        break;
                    }
                }

                // Check for flow keyword
                if let Some(token) = self.peek() {
                    let flow_type = match &token.token_type {
                        TokenType::Keyword(KeywordKind::Serial) => {
                            self.next(); // consume serial
                            FlowType::Serial
                        }
                        TokenType::Keyword(KeywordKind::Parallel) => {
                            self.next(); // consume parallel
                            FlowType::Parallel
                        }
                        TokenType::Keyword(KeywordKind::Cond) => {
                            self.next(); // consume cond
                            FlowType::Cond
                        }
                        TokenType::Keyword(KeywordKind::CondAll) => {
                            self.next(); // consume cond-all
                            FlowType::CondAll
                        }
                        TokenType::Keyword(KeywordKind::Match) => {
                            self.next(); // consume match
                            FlowType::Match
                        }
                        TokenType::Keyword(KeywordKind::MatchAll) => {
                            self.next(); // consume match-all
                            FlowType::MatchAll
                        }
                        _ => break, // No more flow definitions
                    };

                    definitions.push(self.parse_single_function_definition_with_flow(flow_type)?);

                    // Check for comma (indicating more definitions)
                    if let Some(token) = self.peek() {
                        if matches!(token.token_type, TokenType::Comma) {
                            self.next(); // consume comma
                        } else {
                            break; // No comma, end of definitions
                        }
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        } else {
            // Parse regular function definitions: (args) { body }, (args) { body }, ...
            definitions.push(self.parse_single_function_definition()?);

            // Check for additional definitions (different arities)
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Comma) {
                    self.next(); // consume comma
                    definitions.push(self.parse_single_function_definition()?);
                } else {
                    break;
                }
            }
        }

        Ok(Value::Fn(definitions))
    }

    fn apply_all_annotation_to_value(
        &mut self,
        mut value: Value,
        type_annotation: Option<&TypeExpr>,
    ) -> ParseResult<Value> {
        let Some(type_annotation) = type_annotation else {
            return Ok(value);
        };
        if !Self::is_all_annotation(type_annotation) {
            return Ok(value);
        }

        if !matches!(value, Value::Flow(_) | Value::Match(_)) {
            return Ok(value);
        }

        let modifier = self.all_annotation_modifier(type_annotation, &value)?;
        match &mut value {
            Value::Flow(flow) => {
                Self::set_all_result_modifier(&mut flow.result_modifier, modifier)?;
            }
            Value::Match(match_expr) => {
                Self::set_all_result_modifier(&mut match_expr.result_modifier, modifier)?;
            }
            // Non-flow uses of `All` are ordinary type annotations. For example,
            // `tagged: All All({value: [1]})` constructs the real marker value.
            _ => {}
        }

        Ok(value)
    }

    fn all_annotation_modifier(
        &mut self,
        type_annotation: &TypeExpr,
        value: &Value,
    ) -> ParseResult<ResultModifier> {
        match type_annotation {
            // `One` / `One<T>`: produce the single final value instead of
            // collecting. Valid on every flow shape — on serial, pipe,
            // cond, and match it documents the default; on collect-all
            // flows it opts out of collection. The inner type, when given,
            // describes the value and is not enforced here.
            TypeExpr::Named(name) if Self::is_one_type_name(name) => Ok(ResultModifier::One),
            TypeExpr::Generic(name, params) if Self::is_one_type_name(name) => {
                if params.len() != 1 {
                    return Err(self.error("One flow annotations expect exactly one type argument"));
                }
                Ok(ResultModifier::One)
            }
            TypeExpr::Named(name) if Self::is_all_type_name(name) => {
                self.bare_all_modifier_for_value(value)
            }
            TypeExpr::Generic(name, params) if Self::is_all_type_name(name) => {
                if params.len() != 1 {
                    return Err(self.error("All flow annotations expect exactly one type argument"));
                }
                match &params[0] {
                    TypeExpr::Builtin(BuiltinType::Map) => Ok(ResultModifier::Map),
                    TypeExpr::Builtin(BuiltinType::Vec) => Ok(ResultModifier::Vec),
                    TypeExpr::Named(name) if name == "Map" || name.ends_with("/Map") => {
                        Ok(ResultModifier::Map)
                    }
                    TypeExpr::Named(name) if name == "Vec" || name.ends_with("/Vec") => {
                        Ok(ResultModifier::Vec)
                    }
                    other => Err(self.error(&format!(
                        "All flow annotations support Map or Vec containers, got All<{}>",
                        other
                    ))),
                }
            }
            _ => self.bare_all_modifier_for_value(value),
        }
    }

    fn is_all_annotation(type_annotation: &TypeExpr) -> bool {
        match type_annotation {
            TypeExpr::Named(name) => Self::is_all_type_name(name) || Self::is_one_type_name(name),
            TypeExpr::Generic(name, _) => {
                Self::is_all_type_name(name) || Self::is_one_type_name(name)
            }
            _ => false,
        }
    }

    fn is_all_type_name(name: &str) -> bool {
        name == "All" || name.ends_with("/All")
    }

    fn is_one_type_name(name: &str) -> bool {
        name == "One" || name.ends_with("/One")
    }

    fn bare_all_modifier_for_value(&mut self, value: &Value) -> ParseResult<ResultModifier> {
        match value {
            Value::Flow(flow) => match flow.flow_type {
                FlowType::Parallel
                | FlowType::ParallelShort
                | FlowType::CondAll
                | FlowType::CondAllShort
                | FlowType::MatchAll
                | FlowType::MatchAllShort => Ok(ResultModifier::Map),
                FlowType::Serial
                | FlowType::SerialShort
                | FlowType::Pipe
                | FlowType::Cond
                | FlowType::CondShort
                | FlowType::Match
                | FlowType::MatchShort => Err(self.error(
                    "Bare All annotations are only allowed on collect-all flows (parallel, cond-all, match-all); use All<Vec> or All<Map> here",
                )),
            },
            Value::Match(match_expr) if match_expr.match_all => Ok(ResultModifier::Map),
            Value::Match(_) => Err(self.error(
                "Bare All annotations are only allowed on collect-all flows (parallel, cond-all, match-all); use All<Vec> or All<Map> here",
            )),
            _ => Ok(ResultModifier::Vec),
        }
    }

    fn set_all_result_modifier(
        current: &mut Option<ResultModifier>,
        next: ResultModifier,
    ) -> ParseResult<()> {
        if let Some(existing) = current
            && existing != &next
        {
            return Err(ParseError {
                message: format!(
                    "conflicting flow result annotations: this flow already produces `{}` results; use one annotation",
                    Self::result_modifier_name(existing)
                ),
                location: ErrorLocation {
                    line: 1,
                    column: 1,
                    position: 0,
                    length: 1,
                    file: None,
                },
                source_context: None,
            });
        }
        *current = Some(next);
        Ok(())
    }

    fn result_modifier_name(modifier: &ResultModifier) -> &'static str {
        match modifier {
            ResultModifier::Map => "map",
            ResultModifier::Vec => "vec",
            ResultModifier::One => "one",
        }
    }

    /// Parse single function definition with specific flow type
    fn parse_single_function_definition_with_flow(
        &mut self,
        flow_type: FlowType,
    ) -> ParseResult<FnDef> {
        // Skip newlines before parameters
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
            } else {
                break;
            }
        }

        // Parse parameters: (param1, param2, ...)
        self.expect_token(&TokenType::LeftParen)?;

        let mut args = Vec::new();
        let mut variadic = false;

        while !self.check(&TokenType::RightParen) {
            // Check for lazy keyword first (can precede both regular and variadic params)
            let lazy = if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Keyword(KeywordKind::Lazy)) {
                    self.next(); // consume "lazy"
                    true
                } else if let TokenType::Identifier(name) = &token.token_type {
                    if name == "lazy" {
                        self.next(); // consume "lazy"
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            // Check for variadic: ...
            if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::DotDotDot) {
                    self.next(); // consume "..."

                    // Expect parameter name after ...
                    let param_name = self.expect_param_name()?;

                    // Parse optional type annotation for variadic parameter
                    let type_annotation = if self.check(&TokenType::Colon) {
                        self.next(); // consume colon
                        let type_expr = self.parse_type_ref()?;
                        Some(type_expr.to_string())
                    } else {
                        None
                    };

                    args.push(FnArg {
                        var: Var {
                            sym: Sym::String(param_name),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        },
                        lazy, // Variadic args can now be lazy
                        type_annotation,
                    });

                    variadic = true;
                    break;
                } else if let TokenType::Identifier(name) = &token.token_type
                    && name == "..."
                {
                    self.next(); // consume "..." (fallback for identifier version)

                    // Expect parameter name after ...
                    let param_name = self.expect_param_name()?;

                    // Parse optional type annotation for variadic parameter
                    let type_annotation = if self.check(&TokenType::Colon) {
                        self.next(); // consume colon
                        let type_expr = self.parse_type_ref()?;
                        Some(type_expr.to_string())
                    } else {
                        None
                    };

                    args.push(FnArg {
                        var: Var {
                            sym: Sym::String(param_name),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        },
                        lazy, // Variadic args can now be lazy
                        type_annotation,
                    });

                    variadic = true;
                    break;
                }
            }

            // Parse parameter name (for non-variadic params, lazy was already checked above)
            let param_name = self.expect_param_name()?;

            // Parse optional type annotation
            let type_annotation = if self.check(&TokenType::Colon) {
                self.next(); // consume colon
                let type_expr = self.parse_type_ref()?;
                Some(type_expr.to_string())
            } else {
                None
            };

            args.push(FnArg {
                var: Var {
                    sym: Sym::String(param_name),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                lazy,
                type_annotation,
            });

            if !self.check(&TokenType::Comma) {
                break;
            }
            self.next(); // consume comma
        }

        self.expect_token(&TokenType::RightParen)?;

        // Parse optional return type annotation: ): ReturnType
        let return_type_expr = if self.check(&TokenType::Colon) {
            self.next(); // consume colon
            let type_expr = self.parse_type_ref()?;
            Some(type_expr)
        } else {
            None
        };

        // Parse function body as a flow with the specified type
        let body = if self.check(&TokenType::LeftBrace) {
            // Function body with braces - parse as the specified flow type
            self.parse_flow_body_with_type(flow_type)?
        } else {
            // Simple expression as function body - wrap in flow
            let expr = self.parse_value()?;
            Value::Flow(Flow {
                flow_type,
                expressions: vec![expr],
                result_modifier: None,
                src: None,
                aliases: vec![],
            })
        };
        let body = self.apply_all_annotation_to_value(body, return_type_expr.as_ref())?;
        let return_type = return_type_expr.map(|type_expr| type_expr.to_string());

        Ok(FnDef {
            args: FnArgs { args, variadic },
            body,
            return_type,
        })
    }

    /// Try to parse a namespace declaration
    /// Supports both: `::path ns [meta {...}]` and `::path [meta {...}] ns`
    /// Returns Some((ns_path, metadata)) if successful, None if not a namespace declaration
    fn try_parse_namespace_declaration(&mut self) -> ParseResult<Option<(NsPath, Option<Meta>)>> {
        // Expect :: at the start
        if !self.check(&TokenType::DoubleColon) {
            return Ok(None);
        }

        // Parse the namespace path
        let ns_path = self.parse_namespace_reference()?;

        // Extract the NsPath from the Value::Ref(Ref::Ns(NsRef { ns }))
        let ns_path = match &ns_path {
            Value::Ref(Ref::Ns(ns_ref)) => ns_ref.ns.clone(),
            _ => return Ok(None),
        };

        // Skip newlines
        while let Some(t) = self.peek()
            && matches!(t.token_type, TokenType::Newline)
        {
            self.next();
        }

        // Check what comes next: 'ns' or 'meta'
        // declaration := var [meta] value [meta]
        let mut ns_meta: Option<Meta> = None;

        if let Some(token) = self.peek() {
            match &token.token_type {
                TokenType::Identifier(name) if name == "ns" => {
                    // Format: `::path ns [meta {...}]`
                    self.next(); // consume 'ns'

                    // Skip newlines after ns
                    while let Some(t) = self.peek()
                        && matches!(t.token_type, TokenType::Newline)
                    {
                        self.next();
                    }

                    // Check for optional trailing metadata (meta followed by { or [, not ()
                    if let Some(t) = self.peek()
                        && matches!(&t.token_type, TokenType::Identifier(s) if s == "meta")
                    {
                        let is_metadata = self.peek_n(1).is_some_and(|tok| {
                            matches!(
                                tok.token_type,
                                TokenType::LeftBrace
                                    | TokenType::LeftBracket
                                    | TokenType::String(_)
                                    | TokenType::Int(_)
                                    | TokenType::Bool(_)
                                    | TokenType::Null
                            )
                        });

                        if is_metadata {
                            self.next(); // consume meta
                            let meta_value = self.parse_primary_value()?;
                            if let Value::Val(val, _) = meta_value {
                                ns_meta = Some(Meta { val });
                            } else {
                                return Err(
                                    self.error("Namespace metadata must be a literal value")
                                );
                            }
                        }
                    }

                    return Ok(Some((ns_path, ns_meta)));
                }
                TokenType::Identifier(name) if name == "meta" => {
                    // Check if this is actually metadata (meta followed by { or [) vs something else
                    let is_metadata = self.peek_n(1).is_some_and(|tok| {
                        matches!(
                            tok.token_type,
                            TokenType::LeftBrace
                                | TokenType::LeftBracket
                                | TokenType::String(_)
                                | TokenType::Int(_)
                                | TokenType::Bool(_)
                                | TokenType::Null
                        )
                    });

                    if !is_metadata {
                        return Ok(None);
                    }

                    // Format: `::path meta {...} ns`
                    self.next(); // consume meta

                    let meta_value = self.parse_primary_value()?;
                    if let Value::Val(val, _) = meta_value {
                        ns_meta = Some(Meta { val });
                    } else {
                        return Err(self.error("Namespace metadata must be a literal value"));
                    }

                    // Skip newlines after metadata
                    while let Some(t) = self.peek()
                        && matches!(t.token_type, TokenType::Newline)
                    {
                        self.next();
                    }

                    // Now expect 'ns'
                    if let Some(t) = self.peek()
                        && let TokenType::Identifier(name) = &t.token_type
                        && name == "ns"
                    {
                        self.next(); // consume 'ns'
                        return Ok(Some((ns_path, ns_meta)));
                    }
                    return Ok(None);
                }
                _ => return Ok(None),
            }
        }

        Ok(None)
    }

    /// Try to parse a namespace alias: ::alias ::source::path
    /// Returns Some((alias_path, source_path)) if successful, None if not an alias pattern
    /// Uses token position information to detect whitespace boundaries between namespace paths
    fn try_parse_namespace_alias(&mut self) -> ParseResult<Option<(NsPath, NsPath)>> {
        // Expect :: at the start
        if !self.check(&TokenType::DoubleColon) {
            return Ok(None);
        }

        // Parse the alias namespace path (stops at whitespace boundary)
        let alias_path = match self.parse_single_namespace_path()? {
            Some(path) => path,
            None => return Ok(None),
        };

        // Check if there's a function name (/) - if so, this is not an alias
        // (aliases must be pure namespace paths)
        // Note: parse_single_namespace_path already handles this

        // Skip newlines
        while let Some(t) = self.peek()
            && matches!(t.token_type, TokenType::Newline)
        {
            self.next();
        }

        // Check if next token is :: (start of source namespace)
        if !self.check(&TokenType::DoubleColon) {
            return Ok(None);
        }

        // Parse the source namespace path
        let source_path = match self.parse_single_namespace_path()? {
            Some(path) => path,
            None => return Ok(None),
        };

        Ok(Some((alias_path, source_path)))
    }

    /// Parse a single namespace path, stopping at whitespace boundaries or function references
    /// Returns None if not a valid namespace path (e.g., has a function name after /)
    fn parse_single_namespace_path(&mut self) -> ParseResult<Option<NsPath>> {
        // Expect :: at the start
        let start_token = match self.next() {
            Some(t) if matches!(t.token_type, TokenType::DoubleColon) => t,
            _ => return Ok(None),
        };

        let mut parts = Vec::new();
        let mut last_token_end = start_token.position + start_token.length;

        // Parse namespace parts
        loop {
            // Check if there's whitespace before the next token (would indicate end of this path)
            let next_info = self
                .peek()
                .map(|t| (t.token_type.clone(), t.position, t.length));
            if let Some((_, next_pos, _)) = &next_info
                && *next_pos > last_token_end
            {
                // There's whitespace - this ends the current namespace path
                break;
            }

            // Expect an identifier
            let (ident, ident_end) = match &next_info {
                Some((TokenType::Identifier(name), pos, len)) => {
                    let ident = name.clone();
                    let end = pos + len;
                    self.next(); // consume identifier
                    (ident, end)
                }
                _ => break,
            };

            parts.push(NsPathPart::Sym(Sym::String(ident)));
            last_token_end = ident_end;

            // Check what comes next
            let next_info = self
                .peek()
                .map(|t| (t.token_type.clone(), t.position, t.length));
            if let Some((next_type, next_pos, next_len)) = next_info {
                // Check for whitespace gap
                if next_pos > last_token_end {
                    // There's whitespace - end of this namespace path
                    break;
                }

                match next_type {
                    TokenType::DoubleColon => {
                        // Continue parsing more namespace parts
                        self.next(); // consume ::
                        last_token_end = next_pos + next_len;
                    }
                    TokenType::Slash => {
                        // This is a function reference (::ns/func), not a pure namespace
                        // For aliases, we only want pure namespace paths, so return None
                        return Ok(None);
                    }
                    _ => {
                        // End of namespace path
                        break;
                    }
                }
            } else {
                // End of input
                break;
            }
        }

        if parts.is_empty() {
            Ok(None)
        } else {
            Ok(Some(NsPath::from_vec(parts)))
        }
    }

    /// Parse namespace reference like ::hot::env or ::hot::type
    fn parse_namespace_reference(&mut self) -> ParseResult<Value> {
        // Capture position of the leading :: for source tracking
        let start_token = self.peek().cloned();
        self.expect_token(&TokenType::DoubleColon)?;

        let mut path_parts = Vec::new();
        let mut last_token_end: Option<(usize, usize)> = None;

        // Parse the namespace path (keywords allowed in paths like ::hot::type)
        loop {
            let part_token = self.peek().cloned();
            let part = self.expect_identifier_or_keyword()?;
            if let Some(t) = &part_token {
                last_token_end = Some((t.position, t.length));
            }
            path_parts.push(NsPathPart::Sym(Sym::String(part)));

            if self.check(&TokenType::DoubleColon) {
                self.next(); // consume "::"
            } else {
                break;
            }
        }

        // Create namespace path and reference
        let ns_path = NsPath(path_parts);

        // Build source location spanning from :: through the last part
        let src = start_token.map(|st| {
            let end = last_token_end
                .map(|(pos, len)| pos + len)
                .unwrap_or(st.position + st.length);
            Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                st.line as usize,
                st.column as usize,
                st.position,
                end.saturating_sub(st.position),
            )
        });

        let ns_ref = NsRef {
            ns: ns_path,
            src,
            function_name: None,
        };

        // Return as a namespace reference value
        Ok(Value::Ref(Ref::Ns(ns_ref)))
    }

    /// Parse flow body with specific flow type
    fn parse_flow_body_with_type(&mut self, flow_type: FlowType) -> ParseResult<Value> {
        self.expect_token(&TokenType::LeftBrace)?;

        let mut expressions = Vec::new();
        let mut flow_aliases: Vec<(usize, NsPath, NsPath)> = Vec::new();

        // Parse statements until closing brace
        while !self.check(&TokenType::RightBrace) {
            // Skip newlines
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if self.check(&TokenType::RightBrace) {
                break;
            }

            // For conditional flows, handle special syntax
            if matches!(flow_type, FlowType::Cond | FlowType::CondAll) {
                // Parse conditional branch: condition => result OR => result (default) OR _ => result (default)
                let is_default = self.check(&TokenType::Arrow)
                    || (self.check(&TokenType::Underscore)
                        && matches!(
                            self.peek_n(1).map(|t| &t.token_type),
                            Some(TokenType::Arrow)
                        ));
                let (condition, has_arrow) = if is_default {
                    if self.check(&TokenType::Underscore) {
                        self.next(); // consume "_"
                    }
                    // Default branch: => { result }
                    (Value::Val(Val::Bool(true), None), true)
                } else {
                    // Regular branch: condition => result
                    let cond = self.parse_value()?;
                    let has_arrow = self.check(&TokenType::Arrow);
                    (cond, has_arrow)
                };

                if has_arrow {
                    self.next(); // consume "=>"
                    // Support optional branch label before body
                    let (branch_name, result) = self.parse_conditional_result()?;
                    expressions.push(Value::Cond(
                        branch_name,
                        Box::new(condition),
                        Flow {
                            flow_type: FlowType::Serial,
                            expressions: vec![result],
                            result_modifier: None,
                            src: None,
                            aliases: vec![],
                        },
                    ));
                } else {
                    expressions.push(condition);
                }
            } else if matches!(flow_type, FlowType::Match | FlowType::MatchAll) {
                // For match flows, parse match arms: Type.Variant => result OR => result (default)
                // The value to match is implicitly the first parameter (handled at compile time)

                // Check for default branch: => { result } or _ => { result } (no type/variant, just arrow)
                let is_default = self.check(&TokenType::Arrow)
                    || (self.check(&TokenType::Underscore)
                        && matches!(
                            self.peek_n(1).map(|t| &t.token_type),
                            Some(TokenType::Arrow)
                        ));
                if is_default {
                    if self.check(&TokenType::Underscore) {
                        self.next(); // consume "_"
                    }
                    // Default branch - create arm with no type_name and no variant
                    let start_line = self.peek().map(|t| t.line as usize).unwrap_or(0);
                    let start_column = self.peek().map(|t| t.column as usize).unwrap_or(0);
                    let start_position = self.peek().map(|t| t.position).unwrap_or(0);

                    self.next(); // consume "=>"

                    // Parse body
                    let body = self.parse_value()?;

                    expressions.push(Value::MatchArm(Box::new(MatchArm {
                        type_name: None,
                        variant: None,
                        value_literal: None,
                        binding: None,
                        body,
                        src: Some(Source::new(
                            self.file_path.as_ref().map(|p| p.display().to_string()),
                            start_line,
                            start_column,
                            start_position,
                            0,
                        )),
                    })));
                } else {
                    let arm = self.parse_match_arm()?;
                    expressions.push(Value::MatchArm(Box::new(arm)));
                }
            } else {
                // For other flow types (serial, parallel, pipe), parse variable assignments or bare expressions

                // Check for namespace alias: ::alias ::source::path
                if self.check(&TokenType::DoubleColon) {
                    let saved_index = self.token_index;
                    if let Ok(Some((alias_path, source_path))) = self.try_parse_namespace_alias() {
                        flow_aliases.push((expressions.len(), alias_path, source_path));
                        continue;
                    }
                    self.token_index = saved_index;
                }

                // Try to parse as: var_name value_expr
                // or just: value_expr (with auto-generated variable name)

                // Check if the next token looks like a variable name followed by something
                let maybe_var_assignment = if let Some(token) = self.peek().cloned() {
                    matches!(token.token_type, TokenType::Identifier(_))
                } else {
                    false
                };

                if maybe_var_assignment {
                    // Try parsing as variable assignment: name expression
                    // Save position to backtrack if needed
                    let saved_index = self.token_index;
                    let saved_var_counter = self.var_counter;

                    if let Ok(var_name) = self.expect_identifier() {
                        // Check if the next token is NOT a call/operator (i.e., this is var assignment)
                        // Look for patterns that indicate this is NOT a variable assignment
                        let is_function_call = self.check(&TokenType::LeftParen);
                        let is_method_access = self.check(&TokenType::Dot);
                        let is_pipe = self.check(&TokenType::Pipe);
                        let is_newline_or_end = self.check(&TokenType::Newline)
                            || self.check(&TokenType::RightBrace)
                            || self.check(&TokenType::Comma);

                        if is_function_call || is_method_access || is_pipe || is_newline_or_end {
                            // This is a bare expression (function call, etc.), backtrack
                            self.token_index = saved_index;
                            self.var_counter = saved_var_counter;

                            // Parse as bare expression with auto-generated variable
                            let expr = self.parse_value()?;
                            let auto_name = format!("$var_{}", self.var_counter);
                            self.var_counter += 1;
                            let var = Var {
                                sym: Sym::String(auto_name),
                                deep_set: None,
                                deep_path: None,
                                meta: None,
                                type_annotation: None,
                                src: None,
                            };
                            expressions.push(Value::Ref(Ref::Var(VarRef { var, src: None })));
                            expressions.push(expr);
                        } else {
                            // This looks like a variable assignment: var_name followed by value
                            let var = Var {
                                sym: Sym::String(var_name),
                                deep_set: None,
                                deep_path: None,
                                meta: None,
                                type_annotation: None,
                                src: None,
                            };
                            expressions.push(Value::Ref(Ref::Var(VarRef { var, src: None })));

                            // Parse the value expression
                            let value_expr = self.parse_value()?;
                            expressions.push(value_expr);
                        }
                    } else {
                        // Couldn't parse as identifier, backtrack and parse as bare expression
                        self.token_index = saved_index;
                        self.var_counter = saved_var_counter;

                        let expr = self.parse_value()?;
                        let auto_name = format!("$var_{}", self.var_counter);
                        self.var_counter += 1;
                        let var = Var {
                            sym: Sym::String(auto_name),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        };
                        expressions.push(Value::Ref(Ref::Var(VarRef { var, src: None })));
                        expressions.push(expr);
                    }
                } else {
                    // Not starting with identifier - parse as bare expression with auto-generated variable
                    let expr = self.parse_value()?;
                    let auto_name = format!("$var_{}", self.var_counter);
                    self.var_counter += 1;
                    let var = Var {
                        sym: Sym::String(auto_name),
                        deep_set: None,
                        deep_path: None,
                        meta: None,
                        type_annotation: None,
                        src: None,
                    };
                    expressions.push(Value::Ref(Ref::Var(VarRef { var, src: None })));
                    expressions.push(expr);
                }
            }

            // Skip newlines and check for comma
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if self.check(&TokenType::Comma) {
                self.next(); // consume comma
            }
        }

        self.expect_token(&TokenType::RightBrace)?;

        Ok(Value::Flow(Flow {
            flow_type,
            expressions,
            result_modifier: None,
            src: None,
            aliases: flow_aliases,
        }))
    }

    /// Parse a single function definition.
    fn parse_single_function_definition(&mut self) -> ParseResult<FnDef> {
        // Skip newlines before flow type or parameters
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
            } else {
                break;
            }
        }

        // Check for flow type (cond, serial, parallel, match, etc.)
        let flow_type = match self.peek() {
            Some(token) => {
                match &token.token_type {
                    TokenType::Keyword(KeywordKind::Serial) => {
                        self.next(); // consume serial
                        Some(FlowType::Serial)
                    }
                    TokenType::Keyword(KeywordKind::Parallel) => {
                        self.next(); // consume parallel
                        Some(FlowType::Parallel)
                    }
                    TokenType::Keyword(KeywordKind::Cond) => {
                        self.next(); // consume cond
                        Some(FlowType::Cond)
                    }
                    TokenType::Keyword(KeywordKind::CondAll) => {
                        self.next(); // consume cond-all
                        Some(FlowType::CondAll)
                    }
                    TokenType::Keyword(KeywordKind::Match) => {
                        self.next(); // consume match
                        Some(FlowType::Match)
                    }
                    TokenType::Keyword(KeywordKind::MatchAll) => {
                        self.next(); // consume match-all
                        Some(FlowType::MatchAll)
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        // If we have a flow type, delegate to the flow-specific parser
        if let Some(flow_type) = flow_type {
            return self.parse_single_function_definition_with_flow(flow_type);
        }

        // Skip newlines before parameters
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
            } else {
                break;
            }
        }

        // Parse parameters: (param1, param2, ...)
        self.expect_token(&TokenType::LeftParen)?;

        let mut args = Vec::new();
        let mut variadic = false;

        while !self.check(&TokenType::RightParen) {
            // Check for lazy keyword first (can precede both regular and variadic params)
            let lazy = if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Keyword(KeywordKind::Lazy)) {
                    self.next(); // consume "lazy"
                    true
                } else if let TokenType::Identifier(name) = &token.token_type {
                    if name == "lazy" {
                        self.next(); // consume "lazy"
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            // Check for variadic: ...
            if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::DotDotDot) {
                    self.next(); // consume "..."

                    // Expect parameter name after ...
                    let param_name = self.expect_param_name()?;

                    // Parse optional type annotation for variadic parameter
                    let type_annotation = if self.check(&TokenType::Colon) {
                        self.next(); // consume colon
                        let type_expr = self.parse_type_ref()?;
                        Some(type_expr.to_string())
                    } else {
                        None
                    };

                    args.push(FnArg {
                        var: Var {
                            sym: Sym::String(param_name),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        },
                        lazy, // Variadic args can now be lazy
                        type_annotation,
                    });

                    variadic = true;
                    break;
                } else if let TokenType::Identifier(name) = &token.token_type
                    && name == "..."
                {
                    self.next(); // consume "..." (fallback for identifier version)

                    // Expect parameter name after ...
                    let param_name = self.expect_param_name()?;

                    // Parse optional type annotation for variadic parameter
                    let type_annotation = if self.check(&TokenType::Colon) {
                        self.next(); // consume colon
                        let type_expr = self.parse_type_ref()?;
                        Some(type_expr.to_string())
                    } else {
                        None
                    };

                    args.push(FnArg {
                        var: Var {
                            sym: Sym::String(param_name),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        },
                        lazy, // Variadic args can now be lazy
                        type_annotation,
                    });

                    variadic = true;
                    break;
                }
            }

            // Parse parameter name (for non-variadic params, lazy was already checked above)
            let param_name = self.expect_param_name()?;

            // Parse optional type annotation
            let type_annotation = if self.check(&TokenType::Colon) {
                self.next(); // consume colon
                let type_expr = self.parse_type_ref()?;
                Some(type_expr.to_string())
            } else {
                None
            };

            args.push(FnArg {
                var: Var {
                    sym: Sym::String(param_name),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                lazy,
                type_annotation,
            });

            if !self.check(&TokenType::Comma) {
                break;
            }
            self.next(); // consume comma
        }

        self.expect_token(&TokenType::RightParen)?;

        // Parse optional return type annotation: ): ReturnType
        let return_type_expr = if self.check(&TokenType::Colon) {
            self.next(); // consume colon
            let type_expr = self.parse_type_ref()?;
            Some(type_expr)
        } else {
            None
        };

        // Parse function body
        let body = if self.check(&TokenType::LeftBrace) {
            // Function body with braces - parse as serial flow
            self.parse_function_body_flow()?
        } else {
            // Simple expression as function body
            self.parse_value()?
        };
        let body = self.apply_all_annotation_to_value(body, return_type_expr.as_ref())?;
        let return_type = return_type_expr.map(|type_expr| type_expr.to_string());

        Ok(FnDef {
            args: FnArgs { args, variadic },
            body,
            return_type,
        })
    }

    /// Parse a type definition.
    fn parse_type_definition(&mut self, name: Sym) -> ParseResult<TypeDef> {
        // Optional `open` modifier for literal-union type aliases.
        // `open` is a context-sensitive identifier (not a reserved keyword)
        // because user code may use it as a function name. We only consume
        // it here when the next token is unambiguously the start of a
        // literal union — a leading `|` or a literal value. This keeps
        // closed-shape declarations (`Foo type Int`, `Foo type meta {...}`,
        // `Foo type` alone, …) backwards-compatible.
        //
        // Forms accepted:
        //     Fruit type open "apple" | "banana"     -- initial declaration
        //     Fruit type open | "kiwi"               -- extension (single, leading `|`)
        //     Fruit type open "kiwi" | "mango"       -- extension (multi)
        //     Fruit type open | "a" | "b"            -- initial w/ visual leading `|`
        //
        // The accumulation/closed-mismatch semantics are handled by the
        // type checker; the parser just records `is_open: true` on the AST.
        let is_open_alias = if let Some(token) = self.peek()
            && matches!(&token.token_type, TokenType::Identifier(s) if s == "open")
            && self.peek_n(1).is_some_and(|t| {
                matches!(
                    t.token_type,
                    TokenType::Pipe
                        | TokenType::String(_)
                        | TokenType::Int(_)
                        | TokenType::Dec(_)
                        | TokenType::Bool(_)
                        | TokenType::Null
                )
            }) {
            self.next(); // consume `open`
            true
        } else {
            false
        };

        // For an open declaration, allow an optional leading `|` so the
        // syntax stays visually consistent with extensions (`type open | "x"`).
        if is_open_alias
            && let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::Pipe)
        {
            self.next(); // consume leading `|`
        }

        // FIRST: Check for type alias on the SAME LINE (before skipping newlines)
        // Type aliases like `Fruit type "apple" | "banana"` must be on the same line
        if let Some(token) = self.peek() {
            match &token.token_type {
                // Literals on same line = type alias
                TokenType::String(_)
                | TokenType::Int(_)
                | TokenType::Dec(_)
                | TokenType::Bool(_)
                | TokenType::Null
                | TokenType::DoubleColon => {
                    let type_expr = self.parse_union_type()?;
                    return Ok(TypeDef {
                        name,
                        fields: None,
                        type_alias: Some(type_expr),
                        variants: None,
                        constructor_functions: None,
                        implementations: None,
                        is_open: is_open_alias,
                    });
                }
                // Identifier on same line (but not 'meta') = type alias
                TokenType::Identifier(ident) if ident != "meta" => {
                    let type_expr = self.parse_union_type()?;
                    return Ok(TypeDef {
                        name,
                        fields: None,
                        type_alias: Some(type_expr),
                        variants: None,
                        constructor_functions: None,
                        implementations: None,
                        is_open: is_open_alias,
                    });
                }
                _ => {}
            }
        }

        // If we consumed `open` but the trailing token sequence didn't
        // form a valid literal union, surface a parse error rather than
        // silently producing an empty struct-like declaration.
        if is_open_alias {
            return Err(self
                .error("Expected a literal value or '|' after 'open' in type alias declaration"));
        }

        // Skip newlines after 'type' keyword (for struct definitions, meta, fn, etc.)
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next(); // consume newline
            } else {
                break;
            }
        }

        // Check what comes after the type keyword
        match self.peek() {
            Some(token) => {
                match &token.token_type {
                    TokenType::LeftBrace => {
                        // Struct-like type: type Name { field1: Type1, field2: Type2 }
                        self.next(); // Consume opening brace
                        let mut fields = Vec::new();

                        // Parse fields
                        while let Some(token) = self.peek() {
                            if matches!(token.token_type, TokenType::RightBrace) {
                                break;
                            }

                            // Skip newlines before field name
                            while let Some(token) = self.peek() {
                                if matches!(token.token_type, TokenType::Newline) {
                                    self.next(); // consume newline
                                } else {
                                    break;
                                }
                            }

                            // Check for end brace after skipping newlines
                            if let Some(token) = self.peek()
                                && matches!(token.token_type, TokenType::RightBrace)
                            {
                                break;
                            }

                            // Parse field name (keywords allowed as field names)
                            let field_name = self.expect_identifier_or_keyword()?;

                            // Expect colon
                            if !self.check(&TokenType::Colon) {
                                return Err(
                                    self.error("Expected ':' after field name in type definition")
                                );
                            }
                            self.next(); // consume colon

                            // Parse field type (support complex types like Str?, Map<Str, Int>, etc.)
                            let field_type = self.parse_type_ref()?;

                            fields.push(crate::lang::ast::TypeField {
                                name: crate::lang::ast::Sym::String(field_name),
                                type_annotation: field_type.to_string(),
                                type_expr: field_type,
                            });

                            // Skip newlines after field type
                            while let Some(token) = self.peek() {
                                if matches!(token.token_type, TokenType::Newline) {
                                    self.next(); // consume newline
                                } else {
                                    break;
                                }
                            }

                            // Check for comma or end
                            if let Some(token) = self.peek() {
                                if matches!(token.token_type, TokenType::Comma) {
                                    self.next(); // consume comma
                                    // Skip newlines after comma
                                    while let Some(token) = self.peek() {
                                        if matches!(token.token_type, TokenType::Newline) {
                                            self.next(); // consume newline
                                        } else {
                                            break;
                                        }
                                    }
                                } else if matches!(token.token_type, TokenType::RightBrace) {
                                    // End of fields
                                    break;
                                } else {
                                    return Err(
                                        self.error("Expected ',' or '}' in type definition")
                                    );
                                }
                            }
                        }

                        // Consume closing brace
                        if !self.check(&TokenType::RightBrace) {
                            return Err(self.error("Expected '}' to close type definition"));
                        }
                        self.next(); // consume closing brace

                        // Skip newlines before checking for 'fn' keyword
                        while let Some(token) = self.peek() {
                            if matches!(token.token_type, TokenType::Newline) {
                                self.next(); // consume newline
                            } else {
                                break;
                            }
                        }

                        // Check if there's a 'fn' keyword after the struct definition for constructor functions
                        if let Some(token) = self.peek()
                            && matches!(token.token_type, TokenType::Keyword(KeywordKind::Fn))
                        {
                            self.next(); // consume "fn"
                            let mut constructors = Vec::new();

                            // Parse constructor functions using the same logic as regular function definitions
                            // Skip newlines after 'fn' keyword
                            while let Some(token) = self.peek() {
                                if matches!(token.token_type, TokenType::Newline) {
                                    self.next(); // consume newline
                                } else {
                                    break;
                                }
                            }

                            // Check if the first thing after 'fn' is a flow keyword
                            let starts_with_flow = if let Some(token) = self.peek() {
                                matches!(
                                    token.token_type,
                                    TokenType::Keyword(KeywordKind::Serial)
                                        | TokenType::Keyword(KeywordKind::Parallel)
                                        | TokenType::Keyword(KeywordKind::Cond)
                                        | TokenType::Keyword(KeywordKind::CondAll)
                                )
                            } else {
                                false
                            };

                            if starts_with_flow {
                                // Parse multiple flow-based constructor definitions: >flow (args) { body }, >flow (args) { body }, ...
                                loop {
                                    // Skip newlines before flow keyword
                                    while let Some(token) = self.peek() {
                                        if matches!(token.token_type, TokenType::Newline) {
                                            self.next(); // consume newline
                                        } else {
                                            break;
                                        }
                                    }

                                    // Check for flow keyword
                                    if let Some(token) = self.peek() {
                                        let flow_type = match &token.token_type {
                                            TokenType::Keyword(KeywordKind::Serial) => {
                                                self.next(); // consume serial
                                                FlowType::Serial
                                            }
                                            TokenType::Keyword(KeywordKind::Parallel) => {
                                                self.next(); // consume parallel
                                                FlowType::Parallel
                                            }
                                            TokenType::Keyword(KeywordKind::Cond) => {
                                                self.next(); // consume cond
                                                FlowType::Cond
                                            }
                                            TokenType::Keyword(KeywordKind::CondAll) => {
                                                self.next(); // consume cond-all
                                                FlowType::CondAll
                                            }
                                            TokenType::Keyword(KeywordKind::Match) => {
                                                self.next(); // consume match
                                                FlowType::Match
                                            }
                                            TokenType::Keyword(KeywordKind::MatchAll) => {
                                                self.next(); // consume match-all
                                                FlowType::MatchAll
                                            }
                                            _ => break, // No more flow definitions
                                        };

                                        constructors.push(
                                            self.parse_single_function_definition_with_flow(
                                                flow_type,
                                            )?,
                                        );

                                        // Check for comma (indicating more definitions)
                                        if let Some(token) = self.peek() {
                                            if matches!(token.token_type, TokenType::Comma) {
                                                self.next(); // consume comma
                                            } else {
                                                break; // No comma, end of definitions
                                            }
                                        } else {
                                            break;
                                        }
                                    } else {
                                        break;
                                    }
                                }
                            } else {
                                // Parse regular constructor definitions: (args) { body }, (args) { body }, ...
                                constructors.push(self.parse_single_function_definition()?);

                                // Check for additional definitions (different arities)
                                while let Some(token) = self.peek() {
                                    if matches!(token.token_type, TokenType::Comma) {
                                        self.next(); // consume comma
                                        constructors.push(self.parse_single_function_definition()?);
                                    } else {
                                        break;
                                    }
                                }
                            }

                            return Ok(TypeDef {
                                name,
                                fields: if fields.is_empty() {
                                    None
                                } else {
                                    Some(fields)
                                },
                                type_alias: None,
                                variants: None,
                                constructor_functions: Some(constructors),
                                implementations: None,
                                is_open: false,
                            });
                        }

                        // No constructor functions, just a struct type
                        Ok(TypeDef {
                            name,
                            fields: if fields.is_empty() {
                                None
                            } else {
                                Some(fields)
                            },
                            type_alias: None,
                            variants: None,
                            constructor_functions: None,
                            implementations: None,
                            is_open: false,
                        })
                    }
                    TokenType::Pipe => {
                        // Variant union type: type Name | Variant1 | Variant2(Type) | ...
                        let variants = self.parse_variant_definitions()?;
                        Ok(TypeDef {
                            name,
                            fields: None,
                            type_alias: None,
                            variants: Some(variants),
                            constructor_functions: None,
                            implementations: None,
                            is_open: false,
                        })
                    }
                    TokenType::Keyword(KeywordKind::Fn) => {
                        // Type with constructor functions: type Name fn ...
                        self.next(); // consume "fn"
                        let mut constructors = Vec::new();

                        // Parse constructor functions using the same logic as regular function definitions
                        // Skip newlines after 'fn' keyword
                        while let Some(token) = self.peek() {
                            if matches!(token.token_type, TokenType::Newline) {
                                self.next(); // consume newline
                            } else {
                                break;
                            }
                        }

                        // Check if the first thing after 'fn' is a flow keyword
                        let starts_with_flow = if let Some(token) = self.peek() {
                            matches!(
                                token.token_type,
                                TokenType::Keyword(KeywordKind::Serial)
                                    | TokenType::Keyword(KeywordKind::Parallel)
                                    | TokenType::Keyword(KeywordKind::Cond)
                                    | TokenType::Keyword(KeywordKind::CondAll)
                            )
                        } else {
                            false
                        };

                        if starts_with_flow {
                            // Parse multiple flow-based constructor definitions: >flow (args) { body }, >flow (args) { body }, ...
                            loop {
                                // Skip newlines before flow keyword
                                while let Some(token) = self.peek() {
                                    if matches!(token.token_type, TokenType::Newline) {
                                        self.next(); // consume newline
                                    } else {
                                        break;
                                    }
                                }

                                // Check for flow keyword
                                if let Some(token) = self.peek() {
                                    let flow_type = match &token.token_type {
                                        TokenType::Keyword(KeywordKind::Serial) => {
                                            self.next(); // consume serial
                                            FlowType::Serial
                                        }
                                        TokenType::Keyword(KeywordKind::Parallel) => {
                                            self.next(); // consume parallel
                                            FlowType::Parallel
                                        }
                                        TokenType::Keyword(KeywordKind::Cond) => {
                                            self.next(); // consume cond
                                            FlowType::Cond
                                        }
                                        TokenType::Keyword(KeywordKind::CondAll) => {
                                            self.next(); // consume cond-all
                                            FlowType::CondAll
                                        }
                                        TokenType::Keyword(KeywordKind::Match) => {
                                            self.next(); // consume match
                                            FlowType::Match
                                        }
                                        TokenType::Keyword(KeywordKind::MatchAll) => {
                                            self.next(); // consume match-all
                                            FlowType::MatchAll
                                        }
                                        _ => break, // No more flow definitions
                                    };

                                    constructors.push(
                                        self.parse_single_function_definition_with_flow(flow_type)?,
                                    );

                                    // Check for comma (indicating more definitions)
                                    if let Some(token) = self.peek() {
                                        if matches!(token.token_type, TokenType::Comma) {
                                            self.next(); // consume comma
                                        } else {
                                            break; // No comma, end of definitions
                                        }
                                    } else {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                        } else {
                            // Parse regular constructor definitions: (args) { body }, (args) { body }, ...
                            constructors.push(self.parse_single_function_definition()?);

                            // Check for additional definitions (different arities) - NO additional 'fn' keyword needed
                            while let Some(token) = self.peek() {
                                if matches!(token.token_type, TokenType::Comma) {
                                    self.next(); // consume comma
                                    constructors.push(self.parse_single_function_definition()?);
                                } else {
                                    break;
                                }
                            }
                        }

                        Ok(TypeDef {
                            name,
                            fields: None,
                            type_alias: None,
                            variants: None,
                            constructor_functions: Some(constructors),
                            implementations: None,
                            is_open: false,
                        })
                    }
                    _ => {
                        // Standalone type declaration (no definition)
                        Ok(TypeDef {
                            name,
                            fields: None,
                            type_alias: None,
                            variants: None,
                            constructor_functions: None,
                            implementations: None,
                            is_open: false,
                        })
                    }
                }
            }
            None => {
                // Standalone type declaration at end of file
                Ok(TypeDef {
                    name,
                    fields: None,
                    type_alias: None,
                    variants: None,
                    constructor_functions: None,
                    implementations: None,
                    is_open: false,
                })
            }
        }
    }

    /// Parse enum definition: `Name enum { A, B, C(Type) }` or
    /// `Name enum open { A, B, C(Type) }` (open variant union, extensible
    /// via `Source -> Name.Variant` arrows from any package).
    fn parse_enum_definition(&mut self, name: Sym) -> ParseResult<TypeDef> {
        // Skip newlines before optional `open` modifier or opening brace
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next();
            } else {
                break;
            }
        }

        // Optional `open` modifier (context-sensitive identifier; `open` is not
        // a reserved keyword because user code uses it as a function name).
        let is_open = if let Some(token) = self.peek()
            && matches!(&token.token_type, TokenType::Identifier(s) if s == "open")
        {
            self.next(); // consume `open`
            // Skip newlines between `open` and `{`
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next();
                } else {
                    break;
                }
            }
            true
        } else {
            false
        };

        // Expect opening brace
        if !self.check(&TokenType::LeftBrace) {
            return Err(self.error("Expected '{' after 'enum'"));
        }
        self.next(); // consume {

        let mut variants = Vec::new();

        // Parse variants: A, B, C(Type)
        loop {
            // Skip newlines
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next();
                } else {
                    break;
                }
            }

            // Check for closing brace
            if self.check(&TokenType::RightBrace) {
                self.next(); // consume }
                break;
            }

            // Expect variant name
            let variant_name = self.expect_identifier()?;

            // Check for type reference: VariantName(TypeRef)
            let type_ref = if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::LeftParen)
            {
                self.next(); // consume (

                // Parse type reference
                let type_name = self.expect_identifier()?;

                // Consume closing paren
                if !self.check(&TokenType::RightParen) {
                    return Err(self.error("Expected ')' after variant type reference"));
                }
                self.next(); // consume )

                Some(type_name)
            } else {
                None // Unit variant
            };

            variants.push(crate::lang::ast::VariantDef {
                name: crate::lang::ast::Sym::String(variant_name),
                type_ref,
            });

            // Skip newlines after variant
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next();
                } else {
                    break;
                }
            }

            // Check for comma (optional before closing brace)
            if self.check(&TokenType::Comma) {
                self.next(); // consume ,
            } else if !self.check(&TokenType::RightBrace) {
                return Err(self.error("Expected ',' or '}' after variant"));
            }
        }

        if variants.is_empty() && !is_open {
            return Err(self.error("Expected at least one variant in enum"));
        }

        Ok(TypeDef {
            name,
            fields: None,
            type_alias: None,
            variants: Some(variants),
            constructor_functions: None,
            implementations: None,
            is_open,
        })
    }

    /// Parse variant definitions for pipe-prefixed variant union syntax.
    /// Syntax: | Variant1 | Variant2(Type) | Variant3
    fn parse_variant_definitions(&mut self) -> ParseResult<Vec<crate::lang::ast::VariantDef>> {
        let mut variants = Vec::new();

        // Parse variant definitions starting with |
        while let Some(token) = self.peek() {
            if !matches!(token.token_type, TokenType::Pipe) {
                break;
            }
            self.next(); // consume |

            // Skip newlines after |
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next();
                } else {
                    break;
                }
            }

            // Expect variant name (identifier starting with uppercase by convention)
            let variant_name = self.expect_identifier()?;

            // Check for type reference: VariantName(TypeRef)
            let type_ref = if let Some(token) = self.peek()
                && matches!(token.token_type, TokenType::LeftParen)
            {
                self.next(); // consume (

                // Parse type reference
                let type_name = self.expect_identifier()?;

                // Consume closing paren
                if !self.check(&TokenType::RightParen) {
                    return Err(self.error("Expected ')' after variant type reference"));
                }
                self.next(); // consume )

                Some(type_name)
            } else {
                None // Unit variant
            };

            variants.push(crate::lang::ast::VariantDef {
                name: crate::lang::ast::Sym::String(variant_name),
                type_ref,
            });

            // Skip newlines before next variant
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next();
                } else {
                    break;
                }
            }
        }

        if variants.is_empty() {
            return Err(self.error("Expected at least one variant in variant union type"));
        }

        Ok(variants)
    }

    /// Parse a value.
    fn parse_value(&mut self) -> ParseResult<Value> {
        self.parse_pipe_chain()
    }

    /// Parse a pipe chain.
    fn parse_pipe_chain(&mut self) -> ParseResult<Value> {
        let mut left = self.parse_primary_value()?;

        // Handle pipe operations: value |> function (with multiline support)
        loop {
            // Skip newlines before checking for pipe operator
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next(); // consume newline
                } else {
                    break;
                }
            }

            // Check for pipe operator
            if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::PipeOp) {
                    self.next(); // consume "|>"

                    // Skip newlines after pipe operator
                    while let Some(token) = self.peek() {
                        if matches!(token.token_type, TokenType::Newline) {
                            self.next(); // consume newline
                        } else {
                            break;
                        }
                    }

                    let right = self.parse_primary_value()?;

                    // Create or extend pipe flow
                    left = match left {
                        Value::Flow(mut existing_flow)
                            if existing_flow.flow_type == FlowType::Pipe =>
                        {
                            // Extend existing pipe flow
                            existing_flow.expressions.push(right);
                            Value::Flow(existing_flow)
                        }
                        _ => {
                            // Create new pipe flow
                            Value::Flow(Flow {
                                flow_type: FlowType::Pipe,
                                expressions: vec![left, right],
                                result_modifier: None,
                                src: None,
                                aliases: vec![],
                            })
                        }
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        // The pipe-tail |map/|vec/|one result-modifier syntax was removed
        // in Hot 2.6.0 in favor of All<...>/One annotations; keep a
        // targeted error so old code gets a migration hint.
        if let Value::Flow(ref flow) = left
            && flow.flow_type == FlowType::Pipe
            && self.check(&TokenType::Pipe)
        {
            return Err(self.error(REMOVED_RESULT_MODIFIER_MSG));
        }

        Ok(left)
    }

    /// Parse pipe continuation starting from a given initial value
    fn parse_pipe_continuation(&mut self, initial_value: Value) -> ParseResult<Value> {
        let mut left = initial_value;

        // Handle pipe operations: value |> function (with multiline support)
        loop {
            // Skip newlines before checking for pipe operator
            while let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::Newline) {
                    self.next(); // consume newline
                } else {
                    break;
                }
            }

            // Check for pipe operator
            if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::PipeOp) {
                    self.next(); // consume "|>"

                    // Skip newlines after pipe operator
                    while let Some(token) = self.peek() {
                        if matches!(token.token_type, TokenType::Newline) {
                            self.next(); // consume newline
                        } else {
                            break;
                        }
                    }

                    let right = self.parse_primary_value()?;

                    // Create or extend pipe flow
                    left = match left {
                        Value::Flow(mut existing_flow)
                            if existing_flow.flow_type == FlowType::Pipe =>
                        {
                            // Extend existing pipe flow
                            existing_flow.expressions.push(right);
                            Value::Flow(existing_flow)
                        }
                        _ => {
                            // Create new pipe flow
                            Value::Flow(Flow {
                                flow_type: FlowType::Pipe,
                                expressions: vec![left, right],
                                result_modifier: None,
                                src: None,
                                aliases: vec![],
                            })
                        }
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(left)
    }

    /// Parse a primary value.
    fn parse_primary_value(&mut self) -> ParseResult<Value> {
        match self.peek() {
            Some(token) => match &token.token_type {
                // Raw (no-unwrap) prefix: !value
                TokenType::Not => {
                    self.next(); // consume "!"
                    let inner = self.parse_primary_value()?;
                    Ok(Value::Raw(Box::new(inner)))
                }

                // Literals: 42, "hello", true, null
                // Type info is always populated for literals to enable compile-time type resolution
                TokenType::Int(i) => {
                    let value = *i;
                    self.next();
                    Ok(Value::Val(Val::Int(value), Some("Int".to_string())))
                }
                TokenType::Dec(d) => {
                    let value = *d;
                    self.next();
                    Ok(Value::Val(Val::Dec(value), Some("Dec".to_string())))
                }
                TokenType::String(s) => {
                    let value = s.clone();
                    self.next();
                    Ok(Value::Val(Val::from(value), Some("Str".to_string())))
                }
                TokenType::Bool(b) => {
                    let value = *b;
                    self.next();
                    Ok(Value::Val(Val::Bool(value), Some("Bool".to_string())))
                }
                TokenType::Null => {
                    self.next();
                    Ok(Value::Val(Val::Null, Some("Null".to_string())))
                }

                // Template literals: `hello ${name}`
                TokenType::TemplateString(content) => {
                    let content = content.clone();
                    self.next();
                    // Parse the template literal content to extract text and expression parts
                    let parts = self.parse_template_literal_parts(&content)?;
                    Ok(Value::TemplateLiteral(TemplateLiteral { parts }))
                }

                // Maps: { key: value }
                TokenType::LeftBrace => self.parse_map(),

                // Vectors: [1, 2, 3]
                TokenType::LeftBracket => self.parse_vector(),

                // Qualified references: ::namespace/var
                TokenType::DoubleColon => self.parse_qualified_reference(),

                // Do keyword for lazy evaluation
                TokenType::Keyword(KeywordKind::Do) => {
                    self.next(); // consume "do"
                    let inner = self.parse_primary_value()?;
                    Ok(Value::Do(Box::new(inner)))
                }

                // Match expression for variant unions
                TokenType::Keyword(KeywordKind::Match) => self.parse_match_expression(false),
                // Match-all expression (execute all matching arms)
                TokenType::Keyword(KeywordKind::MatchAll) => self.parse_match_expression(true),

                // % placeholder lambda: %, %1, %2, %.field, %1.field, %(expr), etc.
                TokenType::Percent(n) => {
                    let n = *n;
                    let percent_token = token.clone();
                    self.next(); // consume "%" or "%N"

                    // %(expr) — explicit lambda boundary (only bare %, adjacent paren)
                    if n == 0
                        && self.check(&TokenType::LeftParen)
                        && self.peek().is_some_and(|t| {
                            percent_token.position + percent_token.length == t.position
                        })
                    {
                        self.next(); // consume (
                        let inner = self.parse_value()?;
                        self.expect_token(&TokenType::RightParen)?;
                        Ok(force_wrap_placeholder(inner))
                    } else if self.check(&TokenType::Dot) || self.check(&TokenType::LeftBracket) {
                        let arg_index = if n == 0 { 1 } else { n };
                        self.parse_placeholder_access(arg_index)
                    } else {
                        let arg_index = if n == 0 { 1 } else { n };
                        Ok(Value::Placeholder(arg_index))
                    }
                }

                // Variable references and function calls
                TokenType::Identifier(name) => {
                    let name = name.clone();
                    self.parse_ref_or_call(name)
                }

                // Parenthesized expressions and flows: (expr) or serial(...)
                TokenType::LeftParen => self.parse_flow_or_parenthesized(),

                // Flow keywords: serial, parallel, cond, etc.
                TokenType::Keyword(KeywordKind::Serial)
                | TokenType::Keyword(KeywordKind::Parallel)
                | TokenType::Keyword(KeywordKind::Cond)
                | TokenType::Keyword(KeywordKind::CondAll) => self.parse_flow(),

                // Function definitions: fn (args) { body }
                TokenType::Keyword(KeywordKind::Fn) => {
                    self.next(); // consume "fn"
                    self.parse_function()
                }

                // Type definitions: type { ... }
                TokenType::Keyword(KeywordKind::Type) => {
                    self.next(); // consume "type"
                    let type_def =
                        self.parse_type_definition(Sym::String("$__AUTO__".to_string()))?;
                    Ok(Value::TypeDef(type_def))
                }

                // Enum definitions: enum { A, B, C }
                TokenType::Keyword(KeywordKind::Enum) => {
                    self.next(); // consume "enum"
                    let type_def =
                        self.parse_enum_definition(Sym::String("$__AUTO__".to_string()))?;
                    Ok(Value::TypeDef(type_def))
                }

                _ => Err(self.error("Expected value")),
            },
            None => Err(self.error("Unexpected end of input")),
        }
    }

    /// Parse a map.
    fn parse_map(&mut self) -> ParseResult<Value> {
        self.expect_token(&TokenType::LeftBrace)?;

        // Use lookahead to distinguish between map literal and block expression
        // Map literal: { "key": value } or { key: value } or {} (empty map)
        // Block expression: { value } or { statement1 statement2 }

        let mut lookahead_index = 0;
        let mut is_map_literal = false;

        // Skip newlines in lookahead
        while let Some(token) = self.peek_n(lookahead_index) {
            if matches!(token.token_type, TokenType::Newline) {
                lookahead_index += 1;
            } else {
                break;
            }
        }

        // Check if this is an empty map: {} or { } (with only whitespace/newlines)
        // An empty {} should be an empty map, not an empty block (which has no semantic meaning)
        if let Some(first_token) = self.peek_n(lookahead_index)
            && matches!(first_token.token_type, TokenType::RightBrace)
        {
            is_map_literal = true;
        }
        // Spread syntax: {...expr} is always a map literal
        else if let Some(first_token) = self.peek_n(lookahead_index)
            && matches!(first_token.token_type, TokenType::DotDotDot)
        {
            is_map_literal = true;
        }
        // Check if first non-newline token after { is followed by a colon
        // Allow keywords as map keys (fn, lazy, do, etc.) in addition to strings and identifiers
        // Note: type, ns, meta are lexed as Identifier tokens, so they're already covered
        else if let Some(first_token) = self.peek_n(lookahead_index)
            && matches!(
                first_token.token_type,
                TokenType::String(_)
                    | TokenType::Identifier(_)
                    | TokenType::Keyword(_) // All keywords are allowed as map keys
                    | TokenType::Bool(_)
                    | TokenType::Null
            )
        {
            // Look for colon or comma after the potential key, skipping newlines
            // Colon → normal map entry; Comma → punned entry (field punning)
            let mut colon_lookahead = lookahead_index + 1;
            while let Some(token) = self.peek_n(colon_lookahead) {
                if matches!(token.token_type, TokenType::Newline) {
                    colon_lookahead += 1;
                } else if matches!(token.token_type, TokenType::Colon) {
                    is_map_literal = true;
                    break;
                } else if matches!(token.token_type, TokenType::Comma) {
                    // Identifier followed by comma → punned map entry
                    // Only for Identifier tokens (not strings, bools, etc.)
                    if matches!(first_token.token_type, TokenType::Identifier(_)) {
                        is_map_literal = true;
                    }
                    break;
                } else {
                    break;
                }
            }
        }

        // If this is not a map literal, parse as a block expression
        if !is_map_literal {
            return self.parse_block_expression_after_brace();
        }

        let mut map = IndexMap::new();

        // Skip newlines after opening brace
        while self.check(&TokenType::Newline) {
            self.next();
        }

        let mut spread_entries: Vec<(usize, Value)> = Vec::new();
        let mut entry_index = 0usize;

        while !self.check(&TokenType::RightBrace) {
            // Skip newlines before key
            while self.check(&TokenType::Newline) {
                self.next();
            }

            // Check for closing brace after skipping newlines
            if self.check(&TokenType::RightBrace) {
                break;
            }

            // Check for spread entry: ...expr
            if self.check(&TokenType::DotDotDot) {
                self.next(); // consume ...
                let spread_value = self.parse_value()?;
                spread_entries.push((entry_index, spread_value));
                entry_index += 1;

                // Skip newlines after spread value
                while self.check(&TokenType::Newline) {
                    self.next();
                }
                if !self.check(&TokenType::Comma) {
                    break;
                }
                self.next(); // consume comma
                while self.check(&TokenType::Newline) {
                    self.next();
                }
                continue;
            }

            // Parse key - allow keywords as unquoted map keys
            let key = self.parse_map_key()?;

            // Check for punning: identifier NOT followed by colon
            if !self.check(&TokenType::Colon) {
                // Field punning: {name} desugars to {name: name}
                let var_value = Value::Ref(Ref::Var(VarRef {
                    var: Var {
                        sym: Sym::String(key.clone()),
                        deep_set: None,
                        deep_path: None,
                        meta: None,
                        type_annotation: None,
                        src: None,
                    },
                    src: None,
                }));
                let stored = Val::Box(Box::new(crate::lang::ast::AstNode(var_value)));
                map.insert(Val::from(key), stored);
                entry_index += 1;
            } else {
                self.expect_token(&TokenType::Colon)?;
                let value = self.parse_value()?;
                // Box AST nodes so they can be executed later; literals remain as raw Vals
                let stored = match &value {
                    Value::Val(v, _) => v.clone(),
                    _ => Val::Box(Box::new(crate::lang::ast::AstNode(value.clone()))),
                };
                map.insert(Val::from(key), stored);
                entry_index += 1;
            }

            // Skip newlines after value
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if !self.check(&TokenType::Comma) {
                break;
            }
            self.next(); // consume comma

            // Skip newlines after comma
            while self.check(&TokenType::Newline) {
                self.next();
            }
        }

        self.expect_token(&TokenType::RightBrace)?;

        if spread_entries.is_empty() {
            Ok(Value::Val(Val::Map(Box::new(map)), Some("Map".to_string())))
        } else {
            // Map with spread entries needs special AST representation
            Ok(Value::MapWithSpread {
                base_entries: map,
                spread_entries,
            })
        }
    }

    /// Parse a map key - allows keywords as unquoted keys in map context
    /// This is a context-sensitive parsing rule: within a map, keywords like
    /// `type`, `fn`, `ns` are treated as string keys rather than language keywords.
    /// Note: type, ns, meta are lexed as Identifier tokens (not keyword tokens)
    fn parse_map_key(&mut self) -> ParseResult<String> {
        match self.peek().map(|t| t.token_type.clone()) {
            // String literals are always valid keys
            Some(TokenType::String(s)) => {
                self.next();
                Ok(s)
            }
            // Regular identifiers (includes type, ns, meta which are lexed as identifiers)
            Some(TokenType::Identifier(name)) => {
                self.next();
                Ok(name)
            }
            // All keywords are allowed as unquoted map keys
            Some(TokenType::Keyword(kw)) => {
                self.next();
                Ok(kw.as_str().to_string())
            }
            // Literal keywords as keys (though unusual)
            Some(TokenType::Bool(b)) => {
                self.next();
                Ok(if b {
                    "true".to_string()
                } else {
                    "false".to_string()
                })
            }
            Some(TokenType::Null) => {
                self.next();
                Ok("null".to_string())
            }
            _ => Err(self.error("Map key must be a string or identifier")),
        }
    }

    /// Parse a vector.
    fn parse_vector(&mut self) -> ParseResult<Value> {
        let _start_token = self.peek().cloned();
        self.expect_token(&TokenType::LeftBracket)?;

        // Vec literals are syntactic data, not function calls. We always emit a
        // `Val::Vec(elements)` where each element is either a raw Val (for
        // literals) or `Val::Box(AstNode(...))` (for refs, fn calls, flows, and
        // `...identifier` spreads). The compiler's `compile_literal_value`
        // handles this representation directly, emitting `MakeVec` /
        // `MakeVecWithSpread` bytecode without going through the function-call
        // machinery — so user code can shadow the `Vec` symbol freely without
        // breaking literal construction.
        let mut elements = Vec::new();

        // Skip newlines after opening bracket
        while self.check(&TokenType::Newline) {
            self.next();
        }

        while !self.check(&TokenType::RightBracket) {
            // Skip newlines before element
            while self.check(&TokenType::Newline) {
                self.next();
            }

            // Check for closing bracket after skipping newlines
            if self.check(&TokenType::RightBracket) {
                break;
            }

            // Check for variadic expansion: ...identifier
            if let Some(token) = self.peek() {
                if matches!(token.token_type, TokenType::DotDotDot) {
                    self.next(); // consume "..."

                    let identifier = self.expect_identifier()?;
                    let expansion_value = Value::VariadicExpansion(identifier);
                    // Box the VariadicExpansion as an AstNode so the compiler's
                    // existing detection (`matches!(&ast_node.0,
                    // Value::VariadicExpansion(_))` in compile_literal_value)
                    // fires and emits MakeVecWithSpread with the right mask.
                    elements.push(Val::Box(Box::new(crate::lang::ast::AstNode(
                        expansion_value,
                    ))));
                } else {
                    let element = self.parse_value()?;
                    let stored = match &element {
                        Value::Val(v, _) => v.clone(),
                        _ => Val::Box(Box::new(crate::lang::ast::AstNode(element.clone()))),
                    };
                    elements.push(stored);
                }
            } else {
                return Err(self.error("Expected element in vector"));
            }

            // Skip newlines after element
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if !self.check(&TokenType::Comma) {
                break;
            }
            self.next(); // consume comma

            // Skip newlines after comma
            while self.check(&TokenType::Newline) {
                self.next();
            }
        }

        self.expect_token(&TokenType::RightBracket)?;

        Ok(Value::Val(Val::Vec(elements), Some("Vec".to_string())))
    }

    /// Parse block expression - handles { value } or { statement1 statement2 }
    /// Parse conditional result with optional branch name: branchname { body } OR branchname >flow { body } OR { body } OR value
    fn parse_conditional_result(&mut self) -> ParseResult<(Option<String>, Value)> {
        // Check for branch name pattern: identifier followed by brace or flow
        if let Some(token) = self.peek().cloned()
            && let TokenType::Identifier(name) = &token.token_type
        {
            let name = name.clone();
            // Look ahead to see if identifier is followed by brace or flow expression
            let mut lookahead_index = 1;
            let mut found_branch_body = false;

            // Skip newlines
            while let Some(ahead_token) = self.peek_n(lookahead_index) {
                if matches!(ahead_token.token_type, TokenType::Newline) {
                    lookahead_index += 1;
                } else if matches!(
                    ahead_token.token_type,
                    TokenType::LeftBrace
                        | TokenType::Keyword(KeywordKind::Serial)
                        | TokenType::Keyword(KeywordKind::Parallel)
                        | TokenType::Keyword(KeywordKind::Cond)
                        | TokenType::Keyword(KeywordKind::CondAll)
                ) {
                    // Pattern: branchname { body } OR branchname >flow { body }
                    found_branch_body = true;
                    break;
                } else {
                    break;
                }
            }

            if found_branch_body {
                // Parse as: branchname { body } OR branchname >flow { body }
                let branch_name = name.clone();
                self.next(); // consume branch name

                // Skip newlines before body
                while self.check(&TokenType::Newline) {
                    self.next();
                }

                // Parse the branch body (could be block expression or flow expression)
                let body = self.parse_value()?;
                return Ok((Some(branch_name), body));
            }
        }

        // Regular result value
        let result = self.parse_value()?;
        Ok((None, result))
    }

    /// Parse match expression: match value { Variant => body, Variant(binding) => body }
    /// If match_all is true, this is a match-all expression that executes all matching arms
    fn parse_match_expression(&mut self, match_all: bool) -> ParseResult<Value> {
        let start_line = self.peek().map(|t| t.line as usize).unwrap_or(0);
        let start_column = self.peek().map(|t| t.column as usize).unwrap_or(0);
        let start_position = self.peek().map(|t| t.position).unwrap_or(0);

        self.next(); // consume "match" or "match-all"

        // |map/|vec/|one result modifiers were removed in Hot 2.6.0;
        // point old code at the All<...>/One annotations.
        if self.check(&TokenType::Pipe) {
            return Err(self.error(REMOVED_RESULT_MODIFIER_MSG));
        }
        let result_modifier = None;

        // Parse the value to match
        let value = self.parse_value()?;

        // Expect opening brace
        self.expect_token(&TokenType::LeftBrace)?;

        let mut arms = Vec::new();

        // Skip newlines after opening brace
        while self.check(&TokenType::Newline) {
            self.next();
        }

        // Parse match arms
        while !self.check(&TokenType::RightBrace) {
            // Skip newlines before arm
            while self.check(&TokenType::Newline) {
                self.next();
            }

            // Check for closing brace after skipping newlines
            if self.check(&TokenType::RightBrace) {
                break;
            }

            // Check for default branch: => { result } or _ => { result } (no type/variant, just arrow)
            let mut value_literal_for_arm: Option<Box<Value>> = None;
            let is_default_arm = self.check(&TokenType::Arrow)
                || (self.check(&TokenType::Underscore)
                    && matches!(
                        self.peek_n(1).map(|t| &t.token_type),
                        Some(TokenType::Arrow)
                    ));
            let (type_name, variant): (Option<String>, Option<String>) = if is_default_arm {
                if self.check(&TokenType::Underscore) {
                    self.next(); // consume "_"
                }
                // Default branch - no type or variant
                (None, None)
            } else {
                // Check if this arm starts with a value literal (Int, Dec, Str, Bool, Null, Vec, Map)
                let is_value_literal = matches!(
                    self.peek().map(|t| &t.token_type),
                    Some(TokenType::Int(_))
                        | Some(TokenType::Dec(_))
                        | Some(TokenType::String(_))
                        | Some(TokenType::Bool(_))
                        | Some(TokenType::Null)
                        | Some(TokenType::LeftBracket)
                        | Some(TokenType::LeftBrace)
                );

                if is_value_literal {
                    // Value equality arm: parse the literal value
                    let literal = self.parse_value()?;
                    value_literal_for_arm = Some(Box::new(literal));
                    (None, None)
                } else {
                    // Parse variant pattern: "Type" (type-level) or "Type.Variant" (variant-level)
                    match self.peek() {
                        Some(token) => {
                            match &token.token_type {
                                TokenType::Identifier(name) => {
                                    let first_name = name.clone();
                                    self.next();

                                    // Check for "." indicating Type.Variant syntax
                                    if self.check(&TokenType::Dot) {
                                        self.next(); // consume "."

                                        // Parse the variant name
                                        let variant_name = match self.peek() {
                                            Some(token) => match &token.token_type {
                                                TokenType::Identifier(name) => {
                                                    let name = name.clone();
                                                    self.next();
                                                    name
                                                }
                                                _ => {
                                                    return Err(ParseError {
                                                message:
                                                    "Expected variant name after '.' in match arm"
                                                        .to_string(),
                                                location: ErrorLocation {
                                                    line: token.line as usize,
                                                    column: token.column as usize,
                                                    position: token.position,
                                                    length: 1,
                                                    file: self.file_path.clone(),
                                                },
                                                source_context: None,
                                            });
                                                }
                                            },
                                            None => {
                                                return Err(ParseError {
                                            message:
                                                "Unexpected end of file after '.' in match arm"
                                                    .to_string(),
                                            location: ErrorLocation {
                                                line: start_line,
                                                column: start_column,
                                                position: start_position,
                                                length: 1,
                                                file: self.file_path.clone(),
                                            },
                                            source_context: None,
                                        });
                                            }
                                        };
                                        // Type.Variant syntax: type_name is Some, variant is Some
                                        (Some(first_name), Some(variant_name))
                                    } else {
                                        // Just a type name - type-level matching (any variant)
                                        // type_name is Some, variant is None
                                        (Some(first_name), None)
                                    }
                                }
                                TokenType::DoubleColon => {
                                    // Parse fully qualified type like ::postmark::api/HttpError
                                    let qualified_name = self.parse_namespaced_type_name()?;

                                    // Check for "." indicating Type.Variant syntax
                                    if self.check(&TokenType::Dot) {
                                        self.next(); // consume "."

                                        // Parse the variant name
                                        let variant_name = match self.peek() {
                                            Some(token) => match &token.token_type {
                                                TokenType::Identifier(name) => {
                                                    let name = name.clone();
                                                    self.next();
                                                    name
                                                }
                                                _ => {
                                                    return Err(ParseError {
                                                message:
                                                    "Expected variant name after '.' in match arm"
                                                        .to_string(),
                                                location: ErrorLocation {
                                                    line: token.line as usize,
                                                    column: token.column as usize,
                                                    position: token.position,
                                                    length: 1,
                                                    file: self.file_path.clone(),
                                                },
                                                source_context: None,
                                            });
                                                }
                                            },
                                            None => {
                                                return Err(ParseError {
                                            message:
                                                "Unexpected end of file after '.' in match arm"
                                                    .to_string(),
                                            location: ErrorLocation {
                                                line: start_line,
                                                column: start_column,
                                                position: start_position,
                                                length: 1,
                                                file: self.file_path.clone(),
                                            },
                                            source_context: None,
                                        });
                                            }
                                        };
                                        (Some(qualified_name), Some(variant_name))
                                    } else {
                                        // Just a qualified type name - type-level matching
                                        (Some(qualified_name), None)
                                    }
                                }
                                _ => {
                                    return Err(ParseError {
                                message: "Expected type, variant name, or value literal in match arm".to_string(),
                                location: ErrorLocation {
                                    line: token.line as usize,
                                    column: token.column as usize,
                                    position: token.position,
                                    length: 1,
                                    file: self.file_path.clone(),
                                },
                                source_context: None,
                            });
                                }
                            }
                        }
                        None => {
                            return Err(ParseError {
                                message: "Unexpected end of file in match expression".to_string(),
                                location: ErrorLocation {
                                    line: start_line,
                                    column: start_column,
                                    position: start_position,
                                    length: 1,
                                    file: self.file_path.clone(),
                                },
                                source_context: None,
                            });
                        }
                    }
                }
            };

            // Check for optional binding: Variant(binding)
            let binding = if self.check(&TokenType::LeftParen) {
                self.next(); // consume "("
                let binding_name = match self.peek() {
                    Some(token) => match &token.token_type {
                        TokenType::Identifier(name) => {
                            let name = name.clone();
                            self.next();
                            name
                        }
                        _ => {
                            return Err(ParseError {
                                message: "Expected binding name in match arm".to_string(),
                                location: ErrorLocation {
                                    line: token.line as usize,
                                    column: token.column as usize,
                                    position: token.position,
                                    length: 1,
                                    file: self.file_path.clone(),
                                },
                                source_context: None,
                            });
                        }
                    },
                    None => {
                        return Err(ParseError {
                            message: "Unexpected end of file in match arm binding".to_string(),
                            location: ErrorLocation {
                                line: start_line,
                                column: start_column,
                                position: start_position,
                                length: 1,
                                file: self.file_path.clone(),
                            },
                            source_context: None,
                        });
                    }
                };
                self.expect_token(&TokenType::RightParen)?;
                Some(binding_name)
            } else {
                None
            };

            // Expect arrow =>
            self.expect_token(&TokenType::Arrow)?;

            // Parse body
            let body = self.parse_value()?;

            arms.push(MatchArm {
                type_name,
                variant,
                value_literal: value_literal_for_arm,
                binding,
                body,
                src: None,
            });

            // Skip newlines after arm
            while self.check(&TokenType::Newline) {
                self.next();
            }

            // Optional comma between arms
            if self.check(&TokenType::Comma) {
                self.next();
                // Skip newlines after comma
                while self.check(&TokenType::Newline) {
                    self.next();
                }
            }
        }

        // Consume closing brace
        self.expect_token(&TokenType::RightBrace)?;

        Ok(Value::Match(MatchExpr {
            value: Box::new(value),
            arms,
            match_all,
            result_modifier,
            src: Some(Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                start_line,
                start_column,
                start_position,
                0, // length will be calculated later
            )),
        }))
    }

    /// Parse a single match arm: Type.Variant => body, value_literal => body, or Type => body
    /// Used for match flow function bodies
    fn parse_match_arm(&mut self) -> ParseResult<MatchArm> {
        let start_line = self.peek().map(|t| t.line as usize).unwrap_or(0);
        let start_column = self.peek().map(|t| t.column as usize).unwrap_or(0);
        let start_position = self.peek().map(|t| t.position).unwrap_or(0);

        // Check if this arm starts with a value literal
        let is_value_literal = matches!(
            self.peek().map(|t| &t.token_type),
            Some(TokenType::Int(_))
                | Some(TokenType::Dec(_))
                | Some(TokenType::String(_))
                | Some(TokenType::Bool(_))
                | Some(TokenType::Null)
                | Some(TokenType::LeftBracket)
                | Some(TokenType::LeftBrace)
        );

        let mut value_literal_for_arm: Option<Box<Value>> = None;
        let (type_name, variant): (Option<String>, Option<String>) = if is_value_literal {
            let literal = self.parse_value()?;
            value_literal_for_arm = Some(Box::new(literal));
            (None, None)
        } else {
            // Parse variant pattern: "Type" (type-level) or "Type.Variant" (variant-level)
            match self.peek() {
                Some(token) => match &token.token_type {
                    TokenType::Identifier(name) => {
                        let first_name = name.clone();
                        self.next();

                        // Check for "." indicating Type.Variant syntax
                        if self.check(&TokenType::Dot) {
                            self.next(); // consume "."

                            // Parse the variant name
                            let variant_name = match self.peek() {
                                Some(token) => match &token.token_type {
                                    TokenType::Identifier(name) => {
                                        let name = name.clone();
                                        self.next();
                                        name
                                    }
                                    _ => {
                                        return Err(ParseError {
                                            message: "Expected variant name after '.' in match arm"
                                                .to_string(),
                                            location: ErrorLocation {
                                                line: token.line as usize,
                                                column: token.column as usize,
                                                position: token.position,
                                                length: 1,
                                                file: self.file_path.clone(),
                                            },
                                            source_context: None,
                                        });
                                    }
                                },
                                None => {
                                    return Err(ParseError {
                                        message: "Unexpected end of file after '.' in match arm"
                                            .to_string(),
                                        location: ErrorLocation {
                                            line: start_line,
                                            column: start_column,
                                            position: start_position,
                                            length: 1,
                                            file: self.file_path.clone(),
                                        },
                                        source_context: None,
                                    });
                                }
                            };
                            (Some(first_name), Some(variant_name))
                        } else {
                            // Just a type name - type-level matching
                            (Some(first_name), None)
                        }
                    }
                    TokenType::DoubleColon => {
                        // Parse fully qualified type like ::postmark::api/HttpError
                        let qualified_name = self.parse_namespaced_type_name()?;

                        // Check for "." indicating Type.Variant syntax
                        if self.check(&TokenType::Dot) {
                            self.next(); // consume "."

                            // Parse the variant name
                            let variant_name = match self.peek() {
                                Some(token) => match &token.token_type {
                                    TokenType::Identifier(name) => {
                                        let name = name.clone();
                                        self.next();
                                        name
                                    }
                                    _ => {
                                        return Err(ParseError {
                                            message: "Expected variant name after '.' in match arm"
                                                .to_string(),
                                            location: ErrorLocation {
                                                line: token.line as usize,
                                                column: token.column as usize,
                                                position: token.position,
                                                length: 1,
                                                file: self.file_path.clone(),
                                            },
                                            source_context: None,
                                        });
                                    }
                                },
                                None => {
                                    return Err(ParseError {
                                        message: "Unexpected end of file after '.' in match arm"
                                            .to_string(),
                                        location: ErrorLocation {
                                            line: start_line,
                                            column: start_column,
                                            position: start_position,
                                            length: 1,
                                            file: self.file_path.clone(),
                                        },
                                        source_context: None,
                                    });
                                }
                            };
                            (Some(qualified_name), Some(variant_name))
                        } else {
                            // Just a qualified type name - type-level matching
                            (Some(qualified_name), None)
                        }
                    }
                    _ => {
                        return Err(ParseError {
                            message: "Expected type, variant name, or value literal in match arm"
                                .to_string(),
                            location: ErrorLocation {
                                line: token.line as usize,
                                column: token.column as usize,
                                position: token.position,
                                length: 1,
                                file: self.file_path.clone(),
                            },
                            source_context: None,
                        });
                    }
                },
                None => {
                    return Err(ParseError {
                        message: "Unexpected end of file in match arm".to_string(),
                        location: ErrorLocation {
                            line: start_line,
                            column: start_column,
                            position: start_position,
                            length: 1,
                            file: self.file_path.clone(),
                        },
                        source_context: None,
                    });
                }
            }
        };

        // Check for optional binding: Variant(binding)
        let binding = if self.check(&TokenType::LeftParen) {
            self.next(); // consume "("
            let binding_name = match self.peek() {
                Some(token) => match &token.token_type {
                    TokenType::Identifier(name) => {
                        let name = name.clone();
                        self.next();
                        name
                    }
                    _ => {
                        return Err(ParseError {
                            message: "Expected binding name in match arm".to_string(),
                            location: ErrorLocation {
                                line: token.line as usize,
                                column: token.column as usize,
                                position: token.position,
                                length: 1,
                                file: self.file_path.clone(),
                            },
                            source_context: None,
                        });
                    }
                },
                None => {
                    return Err(ParseError {
                        message: "Unexpected end of file in match arm binding".to_string(),
                        location: ErrorLocation {
                            line: start_line,
                            column: start_column,
                            position: start_position,
                            length: 1,
                            file: self.file_path.clone(),
                        },
                        source_context: None,
                    });
                }
            };
            self.expect_token(&TokenType::RightParen)?;
            Some(binding_name)
        } else {
            None
        };

        // Expect arrow =>
        self.expect_token(&TokenType::Arrow)?;

        // Parse body
        let body = self.parse_value()?;

        Ok(MatchArm {
            type_name,
            variant,
            value_literal: value_literal_for_arm,
            binding,
            body,
            src: Some(Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                start_line,
                start_column,
                start_position,
                0,
            )),
        })
    }

    /// Parse block expression after opening brace is already consumed
    fn parse_block_expression_after_brace(&mut self) -> ParseResult<Value> {
        let mut expressions = Vec::new();

        // Skip newlines after opening brace
        while self.check(&TokenType::Newline) {
            self.next();
        }

        while !self.check(&TokenType::RightBrace) {
            // Skip newlines before expression
            while self.check(&TokenType::Newline) {
                self.next();
            }

            // Check for closing brace after skipping newlines
            if self.check(&TokenType::RightBrace) {
                break;
            }

            let expr = self.parse_value()?;
            expressions.push(expr);

            // Skip newlines after expression
            while self.check(&TokenType::Newline) {
                self.next();
            }

            // If there's no comma or closing brace, continue parsing expressions
            if self.check(&TokenType::Comma) {
                self.next(); // consume comma
                // Skip newlines after comma
                while self.check(&TokenType::Newline) {
                    self.next();
                }
            }
        }

        self.expect_token(&TokenType::RightBrace)?;

        // If there's only one expression, return it directly
        // If there are multiple expressions, wrap them in a flow
        match expressions.len() {
            0 => Ok(Value::Val(Val::Null, None)),             // Empty block
            1 => Ok(expressions.into_iter().next().unwrap()), // Single expression
            _ => {
                // Multiple expressions - create a serial flow
                Ok(Value::Flow(Flow {
                    flow_type: FlowType::Serial,
                    expressions,
                    result_modifier: None,
                    src: None,
                    aliases: vec![],
                }))
            }
        }
    }

    /// Parse a qualified reference.
    /// Supports:
    /// - `::namespace::path` - bare namespace reference
    /// - `::namespace::path/function` - function reference
    ///
    /// Note: Enum variant references like `::namespace/Type.Variant` are NOT supported
    /// directly. Use type aliasing instead: `MyResult ::hot::type/Result` then `MyResult.Ok(...)`
    fn parse_qualified_reference(&mut self) -> ParseResult<Value> {
        // Capture position of the leading :: for source tracking
        let start_token = self.peek().cloned();
        self.expect_token(&TokenType::DoubleColon)?;

        let mut parts = Vec::new();
        let mut _var_name = None;
        let mut last_token_end: Option<(usize, usize)> = None; // (position, length)

        // Parse namespace parts and optional function/variable name
        // Only thing after / is a function/variable name
        // Everything else is a namespace part
        // Keywords are allowed in namespace paths (e.g., ::hot::type/ok)
        loop {
            // Capture the token position before consuming
            let part_token = self.peek().cloned();
            let part = self.expect_identifier_or_keyword()?;

            if self.check(&TokenType::Slash) {
                // This is a namespace part, and we have a function/variable name after the slash
                self.next(); // consume "/"
                parts.push(part);

                // Parse the function/variable name after the slash
                let func_token = self.peek().cloned();
                let func_name = self.expect_identifier_or_keyword()?;
                if let Some(t) = &func_token {
                    last_token_end = Some((t.position, t.length));
                }
                _var_name = Some(func_name);
                break;
            } else if self.check(&TokenType::DoubleColon) {
                // This is a namespace part, continue parsing
                self.next(); // consume "::"
                parts.push(part);
            } else {
                // No more :: or /, so this is the last namespace part
                // (not a function name unless preceded by /)
                if let Some(t) = &part_token {
                    last_token_end = Some((t.position, t.length));
                }
                parts.push(part);
                break;
            }
        }

        let ns_path = NsPath::from_vec(
            parts
                .into_iter()
                .map(|part| NsPathPart::Sym(Sym::String(part)))
                .collect(),
        );

        // Build source location spanning from :: through the function name
        let src = start_token.map(|st| {
            let end = last_token_end
                .map(|(pos, len)| pos + len)
                .unwrap_or(st.position + st.length);
            Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                st.line as usize,
                st.column as usize,
                st.position,
                end.saturating_sub(st.position),
            )
        });

        let ns_ref = Value::Ref(Ref::Ns(NsRef {
            ns: ns_path,
            src,
            function_name: _var_name, // Use the parsed function name (may include .Variant)
        }));

        // If we have a variable name, we need to create a different structure
        // For now, we'll handle this as a simple namespace reference

        // Check if this is followed by a function call
        if self.check(&TokenType::LeftParen) {
            // This is a function call: ::ns::path/func(args)
            self.parse_function_call_with_function(ns_ref)
        } else {
            // This is just a reference: ::ns::path/func
            Ok(ns_ref)
        }
    }

    /// Parse template literal content into text and expression parts
    fn parse_template_literal_parts(&mut self, content: &str) -> ParseResult<Vec<TemplatePart>> {
        let mut parts = Vec::new();
        let mut chars = content.chars().peekable();
        let mut current_text = String::new();

        while let Some(ch) = chars.next() {
            if ch == '$' && chars.peek() == Some(&'{') {
                // Found start of expression ${
                chars.next(); // consume '{'

                // Add any accumulated text as a text part
                if !current_text.is_empty() {
                    parts.push(TemplatePart::Text(current_text.clone()));
                    current_text.clear();
                }

                // Parse the expression inside ${}
                let mut expr_content = String::new();
                let mut brace_depth = 1;

                for expr_ch in chars.by_ref() {
                    if expr_ch == '{' {
                        brace_depth += 1;
                    } else if expr_ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            break;
                        }
                    }
                    expr_content.push(expr_ch);
                }

                if brace_depth > 0 {
                    return Err(ParseError {
                        message: format!("Unterminated template expression: ${{{}", expr_content),
                        location: ErrorLocation {
                            line: 0,
                            column: 0,
                            position: 0,
                            length: expr_content.len(),
                            file: None,
                        },
                        source_context: None,
                    });
                }

                // Parse the expression content as Hot code
                if !expr_content.is_empty() {
                    let expr_value = self.parse_expression_from_string(&expr_content)?;
                    parts.push(TemplatePart::Expression(Box::new(expr_value)));
                }
            } else {
                // Regular character, add to current text
                current_text.push(ch);
            }
        }

        // Add any remaining text
        if !current_text.is_empty() {
            parts.push(TemplatePart::Text(current_text));
        }

        // If no parts were found, add an empty text part
        if parts.is_empty() {
            parts.push(TemplatePart::Text(String::new()));
        }

        Ok(parts)
    }

    /// Parse a Hot expression from a string
    fn parse_expression_from_string(&mut self, expr_str: &str) -> ParseResult<Value> {
        // Create a new lexer for the expression string
        let mut expr_lexer = crate::lang::lexer::Lexer::new(expr_str);
        let expr_tokens = expr_lexer.tokenize();

        // Check for lexer errors
        if !expr_lexer.errors().is_empty() {
            let first_error = &expr_lexer.errors()[0];
            return Err(ParseError {
                message: format!("Lexer error in template expression: {}", first_error.kind),
                location: ErrorLocation {
                    line: first_error.line,
                    column: first_error.column,
                    position: first_error.position,
                    length: 1,
                    file: None,
                },
                source_context: None,
            });
        }

        // Create a new parser for the expression tokens
        let mut expr_parser = Parser::new();
        expr_parser.tokens = expr_tokens;
        expr_parser.token_index = 0;
        expr_parser.parse_value()
    }

    /// Parse a reference or call.
    fn parse_ref_or_call(&mut self, name: String) -> ParseResult<Value> {
        // Save identifier token info before consuming (for adjacency check)
        let ident_token = self.peek().cloned();
        self.next(); // consume identifier

        // Check for common control flow syntax mistakes from other languages
        // Users may try: `if { ... }` or `if { ... } else { ... }`
        if (name == "if" || name == "else") && self.check(&TokenType::LeftBrace) {
            let suggestion = if name == "if" {
                "In Hot, `if` is a function: if(pred, then) or if(pred, then, else)\n\
                 For multiple branches, use `cond`: cond { pred1 => result1 pred2 => result2 => default }"
            } else {
                "`else` is not a keyword in Hot. Use the `if` function: if(pred, then, else)\n\
                 Or use `cond` for multiple branches: cond { pred1 => result1 => default }"
            };
            return Err(self.error(suggestion));
        }

        // Helper to check if the next token is immediately adjacent (no whitespace)
        // This is used to distinguish a[0] (index access) from a [0] (variable assignment)
        let is_adjacent = |ident: &Option<Token>, next: Option<&Token>| -> bool {
            match (ident, next) {
                (Some(id_tok), Some(next_tok)) => {
                    id_tok.position + id_tok.length == next_tok.position
                }
                _ => false,
            }
        };

        // Check for lambda function pattern: name (params) { body }
        if self.check(&TokenType::LeftParen) {
            // Look ahead to see if this is a lambda function or regular function call
            // Lambda: name (params) { body }
            // Function call: name(args)

            // Use lookahead to find the closing paren and check what follows
            let mut paren_depth = 0;
            let mut lookahead_index = 0;
            let mut found_closing_paren = false;

            while let Some(token) = self.peek_n(lookahead_index) {
                match token.token_type {
                    TokenType::LeftParen => paren_depth += 1,
                    TokenType::RightParen => {
                        paren_depth -= 1;
                        if paren_depth == 0 {
                            found_closing_paren = true;
                            lookahead_index += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                lookahead_index += 1;
            }

            // Check what comes after the closing paren (skipping newlines)
            let mut is_lambda = false;
            if found_closing_paren {
                while let Some(token) = self.peek_n(lookahead_index) {
                    if matches!(token.token_type, TokenType::Newline) {
                        lookahead_index += 1;
                    } else if matches!(token.token_type, TokenType::LeftBrace) {
                        is_lambda = true;
                        break;
                    } else {
                        break;
                    }
                }
            }

            if is_lambda {
                // Parse as lambda function: name (params) { body }
                // For now, treat as function call - lambda parsing can be added later
                self.parse_function_call(name)
            } else {
                // Parse as function call: name(args)
                self.parse_function_call(name)
            }
        } else if self.check(&TokenType::Dot)
            || (self.check(&TokenType::LeftBracket) && is_adjacent(&ident_token, self.peek()))
        {
            // Property access: obj.prop or obj[0] or obj.prop[0].field
            // Note: For brackets, only treat as index access if immediately adjacent (no whitespace)
            // e.g., data[0] is index access, but data [0] is a separate vector literal
            self.parse_property_access(name)
        } else {
            // Simple variable reference
            let span_len = name.len();
            let cur = self.peek();
            let (line, col, pos) = if let Some(tok) = cur {
                (tok.line as usize, tok.column as usize, tok.position)
            } else {
                (1, 1, 0)
            };
            Ok(Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String(name.clone()),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: Some(Source::new(
                    self.file_path.as_ref().map(|p| p.display().to_string()),
                    line,
                    col,
                    pos,
                    span_len,
                )),
            })))
        }
    }

    /// Parse property access like obj.$prop or obj.field or obj[0] or obj.field[0].prop
    fn parse_property_access(&mut self, base_name: String) -> ParseResult<Value> {
        let mut deep_path_parts = Vec::new();
        // Capture source at the start of the base identifier for accurate span
        let start_line = self.peek().map(|t| t.line as usize).unwrap_or(1);
        let start_column = self.peek().map(|t| t.column as usize).unwrap_or(1);
        let start_position = self.peek().map(|t| t.position).unwrap_or(0);

        // Parse dot-separated property access chain and bracket indexing
        loop {
            if self.check(&TokenType::Dot) {
                self.next(); // consume dot

                // Expect identifier or keyword after dot
                let property_name = match self.next() {
                    Some(token) => {
                        if let Some(name) = self.extract_property_name(&token.token_type) {
                            name
                        } else {
                            return Err(self.error(&format!(
                                "Expected identifier after dot, got {:?}",
                                token.token_type
                            )));
                        }
                    }
                    None => return Err(self.error("Expected identifier after dot")),
                };

                deep_path_parts.push(DeepPathPart::Key(property_name));
            } else if self.check(&TokenType::LeftBracket) {
                self.next(); // consume [

                if self.check(&TokenType::RightBracket) {
                    self.next(); // consume ]
                    deep_path_parts.push(DeepPathPart::Append);
                } else {
                    match self.next() {
                        Some(token) => match &token.token_type {
                            TokenType::Int(i) => {
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::Index(*i as usize));
                            }
                            TokenType::Identifier(name) => {
                                let var_name = name.clone();
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::DynamicIndex(var_name));
                            }
                            TokenType::String(s) => {
                                let key = s.clone();
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::Key(key));
                            }
                            TokenType::Percent(n) => {
                                let idx = if *n == 0 { 1 } else { *n };
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::DynamicIndex(format!(
                                    "__placeholder_{}__",
                                    idx
                                )));
                            }
                            _ => {
                                return Err(self.error(&format!(
                                    "Expected integer, identifier, or string in brackets, got {:?}",
                                    token.token_type
                                )));
                            }
                        },
                        None => return Err(self.error("Expected index or key in brackets")),
                    };
                }
            } else {
                break;
            }
        }

        // Create a variable reference with deep path
        let deep_path = if deep_path_parts.is_empty() {
            None
        } else {
            Some(DeepPath::from_vec(deep_path_parts))
        };

        let var_ref = Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String(base_name.clone()),
                deep_set: None,
                deep_path,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: Some(Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                start_line,
                start_column,
                start_position,
                base_name.len(),
            )),
        }));

        // Check if this is followed by a function call: obj.method(args)
        if self.check(&TokenType::LeftParen) {
            self.parse_function_call_with_function(var_ref)
        } else {
            Ok(var_ref)
        }
    }

    /// Parse access after `%` placeholder: %.field, %1.field, %[0] chains
    fn parse_placeholder_access(&mut self, arg_index: usize) -> ParseResult<Value> {
        let mut deep_path_parts = Vec::new();

        loop {
            if self.check(&TokenType::Dot) {
                self.next(); // consume dot
                let property_name = match self.next() {
                    Some(token) => {
                        if let Some(name) = self.extract_property_name(&token.token_type) {
                            name
                        } else {
                            return Err(self.error(&format!(
                                "Expected identifier after dot in % access, got {:?}",
                                token.token_type
                            )));
                        }
                    }
                    None => return Err(self.error("Expected identifier after dot in % access")),
                };
                deep_path_parts.push(DeepPathPart::Key(property_name));
            } else if self.check(&TokenType::LeftBracket) {
                self.next(); // consume [
                if self.check(&TokenType::RightBracket) {
                    self.next(); // consume ]
                    deep_path_parts.push(DeepPathPart::Append);
                } else {
                    match self.next() {
                        Some(token) => match &token.token_type {
                            TokenType::Int(i) => {
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::Index(*i as usize));
                            }
                            TokenType::Identifier(name) => {
                                let var_name = name.clone();
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::DynamicIndex(var_name));
                            }
                            TokenType::String(s) => {
                                let key = s.clone();
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::Key(key));
                            }
                            TokenType::Percent(n) => {
                                let idx = if *n == 0 { 1 } else { *n };
                                self.expect_token(&TokenType::RightBracket)?;
                                deep_path_parts.push(DeepPathPart::DynamicIndex(format!(
                                    "__placeholder_{}__",
                                    idx
                                )));
                            }
                            _ => {
                                return Err(self.error(&format!(
                                    "Expected integer, identifier, or string in %[] access, got {:?}",
                                    token.token_type
                                )));
                            }
                        },
                        None => return Err(self.error("Expected index or key in %[] access")),
                    }
                }
            } else {
                break;
            }
        }

        let deep_path = if deep_path_parts.is_empty() {
            None
        } else {
            Some(DeepPath::from_vec(deep_path_parts))
        };

        // Sentinel name encodes the 1-based arg index for multi-param support
        let sentinel = format!("__placeholder_{}__", arg_index);
        Ok(Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String(sentinel),
                deep_set: None,
                deep_path,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: None,
        })))
    }

    /// Parse an optional deep path (for chained access like `[0]` or `.prop` after expressions)
    fn parse_optional_deep_path(&mut self) -> ParseResult<Option<DeepPath>> {
        let mut deep_path_parts = Vec::new();

        loop {
            if self.check(&TokenType::Dot) {
                self.next(); // consume dot

                // Expect identifier after dot
                let property_name = match self.next() {
                    Some(token) => {
                        if let Some(name) = self.extract_property_name(&token.token_type) {
                            name
                        } else {
                            return Err(self.error(&format!(
                                "Expected identifier after dot, got {:?}",
                                token.token_type
                            )));
                        }
                    }
                    None => return Err(self.error("Expected identifier after dot")),
                };

                deep_path_parts.push(DeepPathPart::Key(property_name));
            } else if self.check(&TokenType::LeftBracket) {
                self.next(); // consume [

                // Check for empty brackets (append syntax)
                if self.check(&TokenType::RightBracket) {
                    self.next(); // consume ]
                    deep_path_parts.push(DeepPathPart::Append);
                } else {
                    // Accept integer literal, identifier, or string for dynamic index access
                    match self.next() {
                        Some(token) => {
                            match &token.token_type {
                                TokenType::Int(i) => {
                                    // Static integer index
                                    self.expect_token(&TokenType::RightBracket)?;
                                    deep_path_parts.push(DeepPathPart::Index(*i as usize));
                                }
                                TokenType::Identifier(name) => {
                                    // Dynamic index via variable reference
                                    let var_name = name.clone();
                                    self.expect_token(&TokenType::RightBracket)?;
                                    deep_path_parts.push(DeepPathPart::DynamicIndex(var_name));
                                }
                                TokenType::String(s) => {
                                    // String literal key (for map access like m["key"])
                                    let key = s.clone();
                                    self.expect_token(&TokenType::RightBracket)?;
                                    deep_path_parts.push(DeepPathPart::Key(key));
                                }
                                _ => {
                                    return Err(self.error(&format!(
                                        "Expected integer, identifier, or string in brackets, got {:?}",
                                        token.token_type
                                    )));
                                }
                            }
                        }
                        None => return Err(self.error("Expected index or key in brackets")),
                    };
                }
            } else {
                break;
            }
        }

        if deep_path_parts.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DeepPath::from_vec(deep_path_parts)))
        }
    }

    /// Parse a function call.
    ///
    /// Note: `%` placeholder arguments are emitted as bare `Value::Placeholder(n)`
    /// nodes and are NOT wrapped into lambdas here. Wrapping is performed by the
    /// `placeholder_lowering` compiler pass which has full project-wide signature
    /// information and can wrap at the nearest enclosing `Fn`-typed parameter slot
    /// (including for user-defined HOFs). The explicit `%(expr)` boundary is still
    /// honored at parse time via `force_wrap_placeholder`.
    fn parse_function_call_with_function(&mut self, function_value: Value) -> ParseResult<Value> {
        let start_token = self.peek().cloned();
        self.expect_token(&TokenType::LeftParen)?;
        let mut args = Vec::new();

        while self.check(&TokenType::Newline) {
            self.next();
        } // Skip newlines after opening paren

        while !self.check(&TokenType::RightParen) {
            while self.check(&TokenType::Newline) {
                self.next();
            } // Skip newlines before each argument
            if self.check(&TokenType::RightParen) {
                break;
            }

            // Check for spread operator (...)
            let spread = if self.check(&TokenType::DotDotDot) {
                self.next(); // consume "..."
                true
            } else {
                false
            };

            let arg_start_token = self.peek().cloned();
            let arg_value = self.parse_value()?;

            let arg_src = match &arg_value {
                Value::Ref(Ref::Var(var_ref)) => var_ref.src.clone(),
                Value::Ref(Ref::Ns(ns_ref)) => ns_ref.src.clone(),
                Value::FnCall(fn_call) => fn_call.src.clone(),
                _ => arg_start_token.map(|t| {
                    Source::new(
                        self.file_path.as_ref().map(|p| p.display().to_string()),
                        t.line as usize,
                        t.column as usize,
                        t.position,
                        t.length,
                    )
                }),
            };

            args.push(FnCallArg {
                lazy: false,
                spread,
                value: arg_value,
                src: arg_src,
            });

            while self.check(&TokenType::Newline) {
                self.next();
            }

            if !self.check(&TokenType::Comma) {
                break;
            }
            self.next(); // consume comma
            while self.check(&TokenType::Newline) {
                self.next();
            } // Skip newlines after comma
        }

        self.expect_token(&TokenType::RightParen)?;

        let src = start_token.map(|t| {
            Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                t.line as usize,
                t.column as usize,
                t.position,
                1, // Length of "(" - we're at the function call position
            )
        });

        // Check for result path access: fn()[0] or fn().prop
        let result_path = self.parse_optional_deep_path()?;

        Ok(Value::FnCall(FnCall {
            function: Box::new(function_value),
            args,
            result_path,
            src,
        }))
    }

    fn parse_function_call(&mut self, function_name: String) -> ParseResult<Value> {
        let start_token = self.peek().cloned();
        self.expect_token(&TokenType::LeftParen)?;

        let mut args = Vec::new();

        // Skip newlines after opening paren
        while self.check(&TokenType::Newline) {
            self.next();
        }

        while !self.check(&TokenType::RightParen) {
            // Skip newlines before each argument
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if self.check(&TokenType::RightParen) {
                break;
            }

            // Check for lazy argument
            let lazy = if let Some(token) = self.peek() {
                if let TokenType::Identifier(name) = &token.token_type {
                    if name == "lazy" {
                        self.next(); // consume "lazy"
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            // Check for spread operator (...)
            let spread = if self.check(&TokenType::DotDotDot) {
                self.next(); // consume "..."
                true
            } else {
                false
            };

            let arg_start_token = self.peek().cloned();
            let arg_value = self.parse_value()?;

            let arg_src = match &arg_value {
                Value::Ref(Ref::Var(var_ref)) => var_ref.src.clone(),
                Value::Ref(Ref::Ns(ns_ref)) => ns_ref.src.clone(),
                Value::FnCall(fn_call) => fn_call.src.clone(),
                _ => arg_start_token.map(|t| {
                    Source::new(
                        self.file_path.as_ref().map(|p| p.display().to_string()),
                        t.line as usize,
                        t.column as usize,
                        t.position,
                        t.length,
                    )
                }),
            };

            args.push(FnCallArg {
                value: arg_value,
                lazy,
                spread,
                src: arg_src,
            });

            // Skip newlines after argument
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if !self.check(&TokenType::Comma) {
                break;
            }
            self.next(); // consume comma

            // Skip newlines after comma
            while self.check(&TokenType::Newline) {
                self.next();
            }
        }

        self.expect_token(&TokenType::RightParen)?;

        let function = Box::new(Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String(function_name.clone()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: None,
        })));

        let src = start_token.map(|t| {
            Source::new(
                self.file_path.as_ref().map(|p| p.display().to_string()),
                t.line as usize,
                t.column as usize,
                t.position,
                function_name.len(), // Length of the function name
            )
        });

        // Check for result path access: fn()[0] or fn().prop
        let result_path = self.parse_optional_deep_path()?;

        Ok(Value::FnCall(FnCall {
            function,
            args,
            result_path,
            src,
        }))
    }

    /// Parse a flow or parenthesized expression.
    fn parse_flow_or_parenthesized(&mut self) -> ParseResult<Value> {
        self.expect_token(&TokenType::LeftParen)?;

        // Check if this looks like a lambda function: (param) { body } or (param1, param2) { body }
        if self.is_lambda_function() {
            return self.parse_lambda_function();
        }

        // Check if this is a flow or just parenthesized expression
        let expr = self.parse_value()?;

        if self.check(&TokenType::Comma) {
            // This is a flow with multiple expressions
            let mut expressions = vec![expr];

            while self.check(&TokenType::Comma) {
                self.next(); // consume comma
                // Trailing comma: if next token is ), stop
                if self.check(&TokenType::RightParen) {
                    break;
                }
                expressions.push(self.parse_value()?);
            }

            self.expect_token(&TokenType::RightParen)?;

            // Default to serial flow for parenthesized expressions
            Ok(Value::Flow(Flow {
                flow_type: FlowType::Serial,
                expressions,
                result_modifier: None,
                src: None,
                aliases: vec![],
            }))
        } else {
            // Just a parenthesized expression
            self.expect_token(&TokenType::RightParen)?;
            Ok(expr)
        }
    }

    /// Check if this looks like a lambda function
    fn is_lambda_function(&mut self) -> bool {
        // Better heuristic: look for pattern (param1, param2, ...) { body }
        // We need to look ahead to see if there's a { after the closing )
        // NOTE: The opening ( has already been consumed by parse_flow_or_parenthesized
        let mut lookahead_index = 0;
        let mut paren_depth = 0; // Start at 0 since opening ( was already consumed
        let mut found_closing_paren = false;

        // Skip through the parameter list to find the closing )
        while let Some(token) = self.peek_n(lookahead_index) {
            match token.token_type {
                TokenType::LeftParen => paren_depth += 1,
                TokenType::RightParen => {
                    if paren_depth == 0 {
                        // This is the closing ) for the lambda parameters
                        found_closing_paren = true;
                        lookahead_index += 1;
                        break;
                    } else {
                        paren_depth -= 1;
                    }
                }
                _ => {}
            }
            lookahead_index += 1;
        }

        // If we found the closing paren, check if it's followed by { or : ReturnType { (skipping newlines)
        if found_closing_paren {
            while let Some(token) = self.peek_n(lookahead_index) {
                match &token.token_type {
                    TokenType::Newline => {
                        lookahead_index += 1;
                    }
                    TokenType::LeftBrace => {
                        return true; // Found pattern (...) { so it's a lambda
                    }
                    TokenType::Colon => {
                        // Could be return type annotation: (...): ReturnType {
                        // Skip past the colon and type to look for {
                        lookahead_index += 1;
                        // Skip over the type expression (simple heuristic: skip until { or newline)
                        while let Some(type_token) = self.peek_n(lookahead_index) {
                            match &type_token.token_type {
                                TokenType::LeftBrace => return true,
                                TokenType::Newline => {
                                    lookahead_index += 1;
                                }
                                _ => {
                                    lookahead_index += 1;
                                }
                            }
                        }
                        return false;
                    }
                    _ => {
                        return false; // Found something else, not a lambda
                    }
                }
            }
        }

        false // Default to not a lambda
    }

    /// Parse lambda function like (x) { body } or (x: Int): Str { body }
    fn parse_lambda_function(&mut self) -> ParseResult<Value> {
        // Parse parameter list
        let mut params = Vec::new();

        // Parse first parameter (if not immediately closing paren)
        if !self.check(&TokenType::RightParen) {
            if let Some(token) = self.next() {
                if let TokenType::Identifier(name) = &token.token_type {
                    // Check for type annotation
                    let type_annotation = if self.check(&TokenType::Colon) {
                        self.next(); // consume colon
                        let type_expr = self.parse_type_ref()?;
                        Some(type_expr.to_string())
                    } else {
                        None
                    };

                    params.push(FnArg {
                        var: Var {
                            sym: Sym::String(name.clone()),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        },
                        lazy: false,
                        type_annotation,
                    });
                } else {
                    return Err(self.error("Expected parameter name in lambda"));
                }
            }

            // Parse additional parameters
            while self.check(&TokenType::Comma) {
                self.next(); // consume comma
                // Trailing comma: if next token is ), stop
                if self.check(&TokenType::RightParen) {
                    break;
                }
                if let Some(token) = self.next() {
                    if let TokenType::Identifier(name) = &token.token_type {
                        // Check for type annotation
                        let type_annotation = if self.check(&TokenType::Colon) {
                            self.next(); // consume colon
                            let type_expr = self.parse_type_ref()?;
                            Some(type_expr.to_string())
                        } else {
                            None
                        };

                        params.push(FnArg {
                            var: Var {
                                sym: Sym::String(name.clone()),
                                deep_set: None,
                                deep_path: None,
                                meta: None,
                                type_annotation: None,
                                src: None,
                            },
                            lazy: false,
                            type_annotation,
                        });
                    } else {
                        return Err(self.error("Expected parameter name in lambda"));
                    }
                }
            }
        }

        self.expect_token(&TokenType::RightParen)?;

        // Parse optional return type annotation: ): ReturnType {
        let _return_type = if self.check(&TokenType::Colon) {
            self.next(); // consume colon
            Some(self.parse_type_ref()?)
        } else {
            None
        };

        // Parse lambda body - expect { block } for lambda functions
        let body = if self.check(&TokenType::LeftBrace) {
            self.parse_function_body_flow()?
        } else {
            // Single expression lambda body (though this is uncommon for lambdas)
            self.parse_value()?
        };

        // Create lambda
        // Note: Lambda struct doesn't currently store return type, but we parse it for syntax support
        let lambda = Lambda {
            args: FnArgs {
                args: params,
                variadic: false,
            },
            body: Box::new(body),
            flow_type: None, // No flow type for regular lambdas: (x) { ... }
        };

        // Return as lambda value
        Ok(Value::Lambda(lambda))
    }

    /// Check if the current position is a flow-based lambda: serial(args) { body }
    /// This looks ahead from the current position (which should be at '(') to see
    /// if after the closing ')' there's a '{' (allowing newlines in between)
    fn is_flow_based_lambda(&self) -> bool {
        // We need to find the closing paren and check if { follows
        let mut paren_depth = 0;
        let mut lookahead_index = 0;
        let mut found_closing_paren = false;

        while let Some(token) = self.peek_n(lookahead_index) {
            match token.token_type {
                TokenType::LeftParen => paren_depth += 1,
                TokenType::RightParen => {
                    paren_depth -= 1;
                    if paren_depth == 0 {
                        found_closing_paren = true;
                        lookahead_index += 1;
                        break;
                    }
                }
                _ => {}
            }
            lookahead_index += 1;
        }

        if !found_closing_paren {
            return false;
        }

        // Check what comes after the closing paren (skipping newlines)
        while let Some(token) = self.peek_n(lookahead_index) {
            if matches!(token.token_type, TokenType::Newline) {
                lookahead_index += 1;
            } else {
                return matches!(token.token_type, TokenType::LeftBrace);
            }
        }

        false
    }

    /// Parse flow-based lambda: serial(args) { body } or parallel(args) { body } etc.
    fn parse_flow_based_lambda(&mut self, flow_type: FlowType) -> ParseResult<Value> {
        self.expect_token(&TokenType::LeftParen)?;

        // Parse parameter list
        let mut params = Vec::new();

        // Parse first parameter (if not immediately closing paren)
        if !self.check(&TokenType::RightParen) {
            if let Some(token) = self.next() {
                if let TokenType::Identifier(name) = &token.token_type {
                    // Check for type annotation
                    let type_annotation = if self.check(&TokenType::Colon) {
                        self.next(); // consume colon
                        let type_expr = self.parse_type_ref()?;
                        Some(type_expr.to_string())
                    } else {
                        None
                    };

                    params.push(FnArg {
                        var: Var {
                            sym: Sym::String(name.clone()),
                            deep_set: None,
                            deep_path: None,
                            meta: None,
                            type_annotation: None,
                            src: None,
                        },
                        lazy: false,
                        type_annotation,
                    });
                } else {
                    return Err(self.error("Expected parameter name in flow-based lambda"));
                }
            }

            // Parse additional parameters
            while self.check(&TokenType::Comma) {
                self.next(); // consume comma
                // Trailing comma: if next token is ), stop
                if self.check(&TokenType::RightParen) {
                    break;
                }
                if let Some(token) = self.next() {
                    if let TokenType::Identifier(name) = &token.token_type {
                        // Check for type annotation
                        let type_annotation = if self.check(&TokenType::Colon) {
                            self.next(); // consume colon
                            let type_expr = self.parse_type_ref()?;
                            Some(type_expr.to_string())
                        } else {
                            None
                        };

                        params.push(FnArg {
                            var: Var {
                                sym: Sym::String(name.clone()),
                                deep_set: None,
                                deep_path: None,
                                meta: None,
                                type_annotation: None,
                                src: None,
                            },
                            lazy: false,
                            type_annotation,
                        });
                    } else {
                        return Err(self.error("Expected parameter name in flow-based lambda"));
                    }
                }
            }
        }

        self.expect_token(&TokenType::RightParen)?;

        // Parse optional return type annotation: ): ReturnType {
        let _return_type = if self.check(&TokenType::Colon) {
            self.next(); // consume colon
            Some(self.parse_type_ref()?)
        } else {
            None
        };

        // Parse lambda body with the specified flow type
        let body = self.parse_flow_body_with_type(flow_type.clone())?;

        // Create flow-based lambda
        let lambda = Lambda {
            args: FnArgs {
                args: params,
                variadic: false,
            },
            body: Box::new(body),
            flow_type: Some(flow_type),
        };

        Ok(Value::Lambda(lambda))
    }

    /// Parse a function body as a serial flow.
    fn parse_function_body_flow(&mut self) -> ParseResult<Value> {
        self.expect_token(&TokenType::LeftBrace)?;

        let mut expressions = Vec::new();
        let mut flow_aliases: Vec<(usize, NsPath, NsPath)> = Vec::new();

        // Parse statements until closing brace
        while !self.check(&TokenType::RightBrace) {
            // Skip newlines
            while self.check(&TokenType::Newline) {
                self.next();
            }

            if self.check(&TokenType::RightBrace) {
                break;
            }

            // Check for namespace alias: ::alias ::source::path
            if self.check(&TokenType::DoubleColon) {
                let saved_index = self.token_index;
                if let Ok(Some((alias_path, source_path))) = self.try_parse_namespace_alias() {
                    flow_aliases.push((expressions.len(), alias_path, source_path));
                    continue;
                }
                self.token_index = saved_index;
            }

            // Check if this line starts with a pipe operator, which means it's a continuation
            // of the previous expression (multiline pipe chain)
            if self.check(&TokenType::PipeOp) {
                tracing::trace!("Parser: Found pipe continuation in function body");
                // This is a continuation of the previous expression
                // We need to modify the last expression to include this pipe operation
                if let Some(last_expr) = expressions.last_mut() {
                    // Parse the continuation as a pipe chain starting with the last expression
                    let continued_expr = match last_expr {
                        Value::MultipleValues(values) => {
                            // For variable assignments, the value is the second element
                            if values.len() >= 2 {
                                let initial_value = values[1].clone();
                                let continued = self.parse_pipe_continuation(initial_value)?;
                                values[1] = continued;
                                continue;
                            } else {
                                // Fallback: treat the whole thing as the initial value
                                let initial_value = last_expr.clone();
                                self.parse_pipe_continuation(initial_value)?
                            }
                        }
                        _ => {
                            // Regular expression continuation
                            let initial_value = last_expr.clone();
                            self.parse_pipe_continuation(initial_value)?
                        }
                    };
                    *last_expr = continued_expr;
                    continue;
                } else {
                    return Err(
                        self.error("Pipe operator at start of block with no previous expression")
                    );
                }
            }

            // Check if this looks like a variable assignment (identifier followed by a value)
            // Be more conservative - only detect clear assignment patterns
            let is_var_assignment = if let Some(token) = self.peek().cloned() {
                if let TokenType::Identifier(var_name) = &token.token_type {
                    let var_name_len = var_name.len();
                    let var_column = token.column;

                    // Look ahead to see what follows the identifier, skipping newlines
                    let mut lookahead_index = 1;
                    let mut next_non_newline_token = None;

                    // Skip newlines to find the first non-newline token
                    while let Some(lookahead_token) = self.peek_n(lookahead_index) {
                        if matches!(lookahead_token.token_type, TokenType::Newline) {
                            lookahead_index += 1;
                        } else {
                            next_non_newline_token = Some(lookahead_token);
                            break;
                        }
                    }

                    if let Some(next_token) = next_non_newline_token {
                        match &next_token.token_type {
                            // Clear assignment patterns
                            TokenType::LeftBrace => true, // identifier { ... } (map literal)
                            TokenType::LeftBracket => {
                                // Check whitespace: a[0] (index access) vs a [0] (vector assignment)
                                // If there's whitespace, it's a vector assignment: a [0]
                                // If no whitespace (adjacent), it might be an index access OR append syntax
                                let has_whitespace =
                                    next_token.column > var_column + var_name_len as u32;

                                // Also check for append syntax: items[] value (empty brackets)
                                let is_append = self.peek_n(lookahead_index + 1).is_some_and(|t| {
                                    matches!(t.token_type, TokenType::RightBracket)
                                });

                                has_whitespace || is_append // Treat as assignment if there's whitespace OR it's append syntax
                            }
                            TokenType::String(_) => true, // identifier "string"
                            TokenType::Int(_) => true,    // identifier 42
                            TokenType::Bool(_) => true,   // identifier true
                            TokenType::Null => true,      // identifier null
                            TokenType::DoubleColon => true, // identifier ::ns::ref
                            TokenType::Colon => true, // identifier: Type value (type annotation)
                            TokenType::Dot => {
                                // identifier.path... could be a deep path assignment or a method call
                                // Need to look ahead to determine which
                                // For now, trace forward through the path to see if it ends with
                                // an assignment pattern (value, append, etc.) or a function call
                                let mut path_lookahead = lookahead_index + 1;
                                let mut found_assignment_pattern = false;

                                // Skip through property accesses and index accesses
                                while let Some(prop_token) = self.peek_n(path_lookahead) {
                                    // Skip identifier (property name)
                                    if !matches!(prop_token.token_type, TokenType::Identifier(_)) {
                                        break;
                                    }
                                    path_lookahead += 1;

                                    // Check what comes after the property
                                    if let Some(after_prop) = self.peek_n(path_lookahead) {
                                        match &after_prop.token_type {
                                            TokenType::Dot => {
                                                // Continue following the path
                                                path_lookahead += 1;
                                            }
                                            TokenType::LeftBracket => {
                                                // Check if it's [] (append) or [n] (index)
                                                path_lookahead += 1;
                                                if let Some(bracket_content) =
                                                    self.peek_n(path_lookahead)
                                                {
                                                    if matches!(
                                                        bracket_content.token_type,
                                                        TokenType::RightBracket
                                                    ) {
                                                        // Empty brackets - this is append syntax
                                                        found_assignment_pattern = true;
                                                    } else if matches!(
                                                        bracket_content.token_type,
                                                        TokenType::Int(_)
                                                    ) {
                                                        // Index - continue checking
                                                        path_lookahead += 1; // skip the int
                                                        if let Some(close_bracket) =
                                                            self.peek_n(path_lookahead)
                                                            && matches!(
                                                                close_bracket.token_type,
                                                                TokenType::RightBracket
                                                            )
                                                        {
                                                            path_lookahead += 1;
                                                            // Check what's after the index
                                                            if let Some(after_index) =
                                                                self.peek_n(path_lookahead)
                                                            {
                                                                match &after_index.token_type {
                                                                    TokenType::Dot => {
                                                                        path_lookahead += 1;
                                                                        continue;
                                                                    }
                                                                    TokenType::LeftParen => {
                                                                        // Method call
                                                                        break;
                                                                    }
                                                                    _ => {
                                                                        // Value assignment
                                                                        found_assignment_pattern =
                                                                            true;
                                                                    }
                                                                }
                                                            } else {
                                                                found_assignment_pattern = true;
                                                            }
                                                        }
                                                    }
                                                }
                                                break;
                                            }
                                            TokenType::LeftParen => {
                                                // This is a method call, not an assignment
                                                break;
                                            }
                                            TokenType::Newline | TokenType::RightBrace => {
                                                // End of expression - this is a read, not an assignment
                                                break;
                                            }
                                            _ => {
                                                // Something else follows - likely an assignment value
                                                found_assignment_pattern = true;
                                                break;
                                            }
                                        }
                                    } else {
                                        break;
                                    }
                                }

                                found_assignment_pattern
                            }
                            TokenType::Identifier(s) if s == "meta" => true, // identifier meta {...} value
                            TokenType::Implements => true, // identifier -> TargetType fn ...
                            TokenType::Keyword(KeywordKind::Serial) => true, // identifier serial { ... }
                            TokenType::Keyword(KeywordKind::Parallel) => true, // identifier parallel { ... }
                            TokenType::Keyword(KeywordKind::Cond) => true, // identifier cond { ... }
                            TokenType::Keyword(KeywordKind::CondAll) => true, // identifier cond-all { ... }
                            TokenType::Keyword(KeywordKind::Match) => true, // identifier match fn { ... }
                            TokenType::Keyword(KeywordKind::MatchAll) => true, // identifier match-all fn { ... }
                            TokenType::Keyword(KeywordKind::Type) => true, // identifier type { ... } / identifier type "a" | "b"
                            TokenType::Keyword(KeywordKind::Enum) => true, // identifier enum { Variant, ... }
                            // Be more conservative with these patterns
                            TokenType::Identifier(_) => {
                                // This is a pattern like: var_name function_name(args)
                                // This should be treated as a variable assignment where the value is a function call
                                // Check if there's a LeftParen after the second identifier
                                let mut next_lookahead_index = lookahead_index + 1;
                                let mut found_third_token = None;

                                while let Some(third_token) = self.peek_n(next_lookahead_index) {
                                    if matches!(third_token.token_type, TokenType::Newline) {
                                        next_lookahead_index += 1;
                                    } else {
                                        found_third_token = Some(third_token);
                                        break;
                                    }
                                }

                                if let Some(third_token) = found_third_token {
                                    // If pattern is: var_name function_name(, treat as assignment
                                    // If pattern is: var_name identifier.something, treat as assignment (deep path access)
                                    // If pattern is: var_name type {, treat as assignment (type definition)
                                    // If pattern is: var_name identifier[, treat as assignment if no whitespace (index access)
                                    //   e.g., b0 data[0] -> b0 = data[0] (assignment where value is index access)
                                    //   e.g., b0 data [0] -> two expressions: b0 (reference), data (reference), [0] (vector)
                                    if matches!(third_token.token_type, TokenType::LeftBracket) {
                                        // Check whitespace between second identifier and bracket
                                        let has_whitespace = third_token.column
                                            > next_token.column + next_token.length as u32;
                                        // If NO whitespace: data[0] is index access, so this IS an assignment
                                        // If whitespace: data [0] are separate, so NOT an assignment
                                        !has_whitespace // Assignment if no whitespace (index access pattern)
                                    } else {
                                        matches!(
                                            third_token.token_type,
                                            TokenType::LeftParen
                                                | TokenType::Dot
                                                | TokenType::LeftBrace
                                        )
                                    }
                                } else {
                                    // If no third token, treat as assignment: var_name function_name
                                    true
                                }
                            }
                            // Direct function calls like function() should NOT be treated as assignments
                            // Only patterns like: var_name function_name() should be assignments
                            TokenType::LeftParen => false,
                            _ => false,
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if is_var_assignment {
                // Parse as variable assignment
                let (var, value) = self.parse_var_definition()?;
                tracing::trace!(
                    "Parser: Variable assignment - name: '{}', value: {:?}",
                    var.sym.name(),
                    value
                );
                // In function bodies, variable assignments are treated as expressions
                expressions.push(Value::MultipleValues(vec![
                    Value::Ref(Ref::Var(VarRef { var, src: None })),
                    value,
                ]));
            } else {
                // Parse as standalone expression (function calls, etc.)
                let expr = self.parse_value()?;
                expressions.push(expr);
            }
        }

        self.expect_token(&TokenType::RightBrace)?;

        // Return as serial flow
        Ok(Value::Flow(Flow {
            flow_type: FlowType::Serial,
            expressions,
            result_modifier: None,
            src: None,
            aliases: flow_aliases,
        }))
    }

    /// Parse a flow.
    fn parse_flow(&mut self) -> ParseResult<Value> {
        let flow_type = match self.peek() {
            Some(token) => match &token.token_type {
                TokenType::Keyword(KeywordKind::Serial) => {
                    self.next();
                    FlowType::Serial
                }
                TokenType::Keyword(KeywordKind::Parallel) => {
                    self.next();
                    FlowType::Parallel
                }
                TokenType::Keyword(KeywordKind::Cond) => {
                    self.next();
                    FlowType::Cond
                }
                TokenType::Keyword(KeywordKind::CondAll) => {
                    self.next();
                    FlowType::CondAll
                }
                _ => return Err(self.error("Expected flow keyword")),
            },
            None => return Err(self.error("Expected flow keyword")),
        };

        // Check if this is a flow-based lambda: serial(args) { body }
        // Look ahead to see if after the closing paren there's a { (allowing newlines)
        if self.check(&TokenType::LeftParen) && self.is_flow_based_lambda() {
            return self.parse_flow_based_lambda(flow_type);
        }

        // |map/|vec/|one result modifiers were removed in Hot 2.6.0;
        // point old code at the All<...>/One annotations.
        if self.check(&TokenType::Pipe) {
            return Err(self.error(REMOVED_RESULT_MODIFIER_MSG));
        }
        let result_modifier = None;

        let mut expressions = Vec::new();

        // Parse flow body
        if self.check(&TokenType::LeftParen) {
            // Parenthesized flow: serial(expr1, expr2)
            self.next(); // consume "("
            while !self.check(&TokenType::RightParen) {
                expressions.push(self.parse_value()?);
                if !self.check(&TokenType::Comma) {
                    break;
                }
                self.next(); // consume comma
            }
            self.expect_token(&TokenType::RightParen)?;
        } else if self.check(&TokenType::LeftBrace) {
            // Braced flow for conditionals: cond { condition => result }
            self.next(); // consume "{"

            // Skip newlines after opening brace
            while self.check(&TokenType::Newline) {
                self.next();
            }

            while !self.check(&TokenType::RightBrace) {
                // Skip newlines before condition
                while self.check(&TokenType::Newline) {
                    self.next();
                }

                // Check for closing brace after skipping newlines
                if self.check(&TokenType::RightBrace) {
                    break;
                }

                // Parse conditional branch: condition => result OR => result (default) OR _ => result (default)
                let is_default = self.check(&TokenType::Arrow)
                    || (self.check(&TokenType::Underscore)
                        && matches!(
                            self.peek_n(1).map(|t| &t.token_type),
                            Some(TokenType::Arrow)
                        ));
                let (condition, has_arrow) = if is_default {
                    if self.check(&TokenType::Underscore) {
                        self.next(); // consume "_"
                    }
                    // Default branch: => { result }
                    (Value::Val(Val::Bool(true), None), true)
                } else {
                    // Regular branch: condition => result
                    let cond = self.parse_value()?;
                    let has_arrow = self.check(&TokenType::Arrow);
                    (cond, has_arrow)
                };

                if has_arrow {
                    self.next(); // consume "=>"

                    // Parse result with optional branch name
                    let (branch_name, result) = self.parse_conditional_result()?;

                    // Create conditional value
                    expressions.push(Value::Cond(
                        branch_name,
                        Box::new(condition),
                        Flow {
                            flow_type: FlowType::Serial,
                            expressions: vec![result],
                            result_modifier: None,
                            src: None,
                            aliases: vec![],
                        },
                    ));
                } else {
                    expressions.push(condition);
                }

                // Skip newlines after result/condition
                while self.check(&TokenType::Newline) {
                    self.next();
                }

                // Check if we're at the end of the flow
                if self.check(&TokenType::RightBrace) {
                    break;
                }

                // Handle optional comma between branches
                if self.check(&TokenType::Comma) {
                    self.next(); // consume comma

                    // Skip newlines after comma
                    while self.check(&TokenType::Newline) {
                        self.next();
                    }
                }

                // Continue to next branch (no break unless we see closing brace)
            }
            self.expect_token(&TokenType::RightBrace)?;
        } else {
            // Single expression flow
            expressions.push(self.parse_value()?);
        }

        Ok(Value::Flow(Flow {
            flow_type,
            expressions,
            result_modifier,
            src: None,
            aliases: vec![],
        }))
    }

    // Helper methods

    // Token manipulation helpers

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.token_index)
    }

    fn peek_n(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.token_index + n)
    }

    fn next(&mut self) -> Option<Token> {
        if self.token_index < self.tokens.len() {
            let token = self.tokens[self.token_index].clone();
            self.token_index += 1;
            Some(token)
        } else {
            None
        }
    }

    fn check(&self, token_type: &TokenType) -> bool {
        if let Some(current_token) = self.peek() {
            std::mem::discriminant(&current_token.token_type) == std::mem::discriminant(token_type)
        } else {
            false
        }
    }

    fn skip_newlines(&mut self) {
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Newline) {
                self.next();
            } else {
                break;
            }
        }
    }

    fn is_meta_lookahead(&self) -> bool {
        self.peek_n(1).is_some_and(|t| {
            matches!(
                t.token_type,
                TokenType::LeftBrace
                    | TokenType::LeftBracket
                    | TokenType::String(_)
                    | TokenType::Int(_)
                    | TokenType::Bool(_)
                    | TokenType::Null
            )
        })
    }

    fn expect_token(&mut self, expected: &TokenType) -> ParseResult<()> {
        if self.check(expected) {
            self.next();
            Ok(())
        } else {
            Err(self.error(&format!("Expected {:?}", expected)))
        }
    }

    fn expect_identifier(&mut self) -> ParseResult<String> {
        if let Some(token) = self.peek() {
            match &token.token_type {
                TokenType::Identifier(name) => {
                    let name = name.clone();
                    self.next();
                    Ok(name)
                }
                other => {
                    let msg = format!("Expected identifier, got: {:?}", other);
                    Err(self.error(&msg))
                }
            }
        } else {
            Err(self.error("Expected identifier, got EOF"))
        }
    }

    /// Expect an identifier that can be used as a parameter name.
    /// Rejects reserved keywords that cannot be used as parameter names.
    fn expect_param_name(&mut self) -> ParseResult<String> {
        if let Some(token) = self.peek() {
            match token.token_type.clone() {
                TokenType::Identifier(name) => {
                    self.next();
                    Ok(name)
                }
                TokenType::Keyword(kw) => {
                    // Keywords are not allowed as parameter names
                    let msg = format!(
                        "'{}' is a reserved keyword and cannot be used as a parameter name",
                        kw.as_str()
                    );
                    Err(self.error(&msg))
                }
                other => {
                    let msg = format!("Expected parameter name, got: {:?}", other);
                    Err(self.error(&msg))
                }
            }
        } else {
            Err(self.error("Expected parameter name, got EOF"))
        }
    }

    /// Expect an identifier or keyword - used for namespace path parts where keywords
    /// like `type` are valid (e.g., ::hot::type/ok).
    fn expect_identifier_or_keyword(&mut self) -> ParseResult<String> {
        if let Some(token) = self.peek() {
            match &token.token_type {
                TokenType::Identifier(name) => {
                    let name = name.clone();
                    self.next();
                    Ok(name)
                }
                TokenType::Keyword(kw) => {
                    let name = kw.as_str().to_string();
                    self.next();
                    Ok(name)
                }
                other => {
                    let msg = format!("Expected identifier, got: {:?}", other);
                    Err(self.error(&msg))
                }
            }
        } else {
            Err(self.error("Expected identifier, got EOF"))
        }
    }

    fn error(&mut self, message: &str) -> ParseError {
        // Get current token position if available
        if let Some(current_token) = self.peek() {
            ParseError {
                message: message.to_string(),
                location: ErrorLocation {
                    line: current_token.line as usize,
                    column: current_token.column as usize,
                    position: current_token.position,
                    length: 1, // Default length, could be improved
                    file: self.file_path.clone(),
                },
                source_context: self.source_content.clone(),
            }
        } else {
            // If no current token, we're at EOF - use the end of source position
            let source_len = self.source_content.as_ref().map(|s| s.len()).unwrap_or(0);
            let (line, column) = if let Some(source) = &self.source_content {
                // Calculate line and column at EOF
                let mut line = 1;
                let mut column = 1;
                for ch in source.chars() {
                    if ch == '\n' {
                        line += 1;
                        column = 1;
                    } else {
                        column += 1;
                    }
                }
                (line, column)
            } else {
                (1, 1)
            };

            ParseError {
                message: format!("Unexpected end of file: {}", message),
                location: ErrorLocation {
                    line,
                    column,
                    position: source_len,
                    length: 0, // EOF has no length
                    file: self.file_path.clone(),
                },
                source_context: self.source_content.clone(),
            }
        }
    }

    // Type parsing methods.

    /// Parse type reference (union types, optional types, generic types)
    fn parse_type_ref(&mut self) -> ParseResult<TypeExpr> {
        self.parse_union_type()
    }

    /// Parse union types (Int | Str | Null)
    fn parse_union_type(&mut self) -> ParseResult<TypeExpr> {
        let mut types = vec![self.parse_optional_type()?];

        // Parse additional union types separated by |
        while let Some(token) = self.peek() {
            if matches!(token.token_type, TokenType::Pipe) {
                self.next(); // Consume |
                types.push(self.parse_optional_type()?);
            } else {
                break;
            }
        }

        if types.len() == 1 {
            Ok(types.into_iter().next().unwrap())
        } else {
            Ok(TypeExpr::Union(types))
        }
    }

    /// Parse optional types (Int?)
    fn parse_optional_type(&mut self) -> ParseResult<TypeExpr> {
        let base_type = self.parse_generic_type()?;

        // Check for optional modifier ?
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::QuestionMark)
        {
            self.next(); // Consume ?
            return Ok(TypeExpr::Optional(Box::new(base_type)));
        }

        Ok(base_type)
    }

    /// Parse generic types (Vec<Int>, Map<Str, Int>)
    fn parse_generic_type(&mut self) -> ParseResult<TypeExpr> {
        let base_type = self.parse_simple_type()?;

        // Check for generic parameters <...>
        if let Some(token) = self.peek()
            && matches!(token.token_type, TokenType::AngleBracketOpen)
        {
            self.next(); // Consume <

            let mut type_params = vec![];

            // Parse type parameters
            loop {
                match self.peek() {
                    Some(token) if matches!(token.token_type, TokenType::AngleBracketClose) => {
                        self.next(); // Consume >
                        break;
                    }
                    Some(_) => {
                        type_params.push(self.parse_type_ref()?);

                        // Check for comma or closing bracket
                        if let Some(token) = self.peek() {
                            match &token.token_type {
                                TokenType::Comma => {
                                    self.next(); // Consume comma
                                }
                                TokenType::AngleBracketClose => {
                                    // Will be consumed in next iteration
                                }
                                _ => {
                                    return Err(ParseError {
                                        message: "Expected ',' or '>' in generic type".to_string(),
                                        location: ErrorLocation {
                                            line: token.line as usize,
                                            column: token.column as usize,
                                            position: token.position,
                                            length: 1,
                                            file: self.file_path.clone(),
                                        },
                                        source_context: self.source_content.clone(),
                                    });
                                }
                            }
                        } else {
                            return Err(ParseError {
                                message: "Unexpected end of input in generic type".to_string(),
                                location: ErrorLocation {
                                    line: 1,
                                    column: 1,
                                    position: 0,
                                    length: 1,
                                    file: self.file_path.clone(),
                                },
                                source_context: self.source_content.clone(),
                            });
                        }
                    }
                    None => {
                        return Err(ParseError {
                            message: "Unexpected end of input in generic type".to_string(),
                            location: ErrorLocation {
                                line: 1,
                                column: 1,
                                position: 0,
                                length: 1,
                                file: self.file_path.clone(),
                            },
                            source_context: self.source_content.clone(),
                        });
                    }
                }
            }

            match base_type {
                TypeExpr::Named(base_name) => {
                    return Ok(TypeExpr::Generic(base_name, type_params));
                }
                TypeExpr::Builtin(builtin_type) => {
                    // Convert builtin type to named type for generics
                    let type_name = match builtin_type {
                        BuiltinType::Vec => "Vec".to_string(),
                        BuiltinType::Map => "Map".to_string(),
                        _ => {
                            return Err(self.error(&format!(
                                "Builtin type {:?} cannot be generic",
                                builtin_type
                            )));
                        }
                    };
                    return Ok(TypeExpr::Generic(type_name, type_params));
                }
                _ => {
                    return Err(self.error(
                        "Only named types and certain builtin types (Vec, Map) can be generic",
                    ));
                }
            }
        }

        Ok(base_type)
    }

    /// Parse simple types (Int, Str, Bool, custom types, Any, literals for enum unions)
    fn parse_simple_type(&mut self) -> ParseResult<TypeExpr> {
        let file_path = self.file_path.clone();
        let source_content = self.source_content.clone();
        let token = self.peek().ok_or_else(|| ParseError {
            message: "Expected type name".to_string(),
            location: ErrorLocation {
                line: 1,
                column: 1,
                position: 0,
                length: 1,
                file: file_path,
            },
            source_context: source_content,
        })?;

        match &token.token_type {
            TokenType::Identifier(name) => {
                let name = name.clone();
                self.next(); // Consume identifier

                // Map to built-in types or custom types
                match name.as_str() {
                    "Int" => Ok(TypeExpr::Builtin(BuiltinType::Int)),
                    "Dec" => Ok(TypeExpr::Builtin(BuiltinType::Dec)),
                    "Str" => Ok(TypeExpr::Builtin(BuiltinType::Str)),
                    "Bool" => Ok(TypeExpr::Builtin(BuiltinType::Bool)),
                    "Vec" => Ok(TypeExpr::Builtin(BuiltinType::Vec)),
                    "Map" => Ok(TypeExpr::Builtin(BuiltinType::Map)),
                    "Null" => Ok(TypeExpr::Builtin(BuiltinType::Null)),
                    "Any" => Ok(TypeExpr::Any),
                    // Handle true/false as literal types when used as type names
                    "true" => Ok(TypeExpr::Literal(LiteralType::Bool(true))),
                    "false" => Ok(TypeExpr::Literal(LiteralType::Bool(false))),
                    "null" => Ok(TypeExpr::Literal(LiteralType::Null)),
                    _ => Ok(TypeExpr::Named(name)),
                }
            }
            TokenType::DoubleColon => {
                // Parse namespaced type reference like ::hot::time/Instant
                let qualified_name = self.parse_namespaced_type_name()?;
                Ok(TypeExpr::Named(qualified_name))
            }
            // Literal types for enum-style unions (e.g., "apple" | "banana" | "orange")
            TokenType::String(s) => {
                let s = s.clone();
                self.next(); // Consume string literal
                Ok(TypeExpr::Literal(LiteralType::Str(s)))
            }
            TokenType::Int(n) => {
                let n = *n;
                self.next(); // Consume int literal
                Ok(TypeExpr::Literal(LiteralType::Int(n)))
            }
            TokenType::Dec(d) => {
                let d = d.to_string();
                self.next(); // Consume dec literal
                Ok(TypeExpr::Literal(LiteralType::Dec(d)))
            }
            TokenType::Bool(b) => {
                let b = *b;
                self.next(); // Consume bool literal
                Ok(TypeExpr::Literal(LiteralType::Bool(b)))
            }
            TokenType::Null => {
                self.next(); // Consume null literal
                Ok(TypeExpr::Literal(LiteralType::Null))
            }
            _ => Err(ParseError {
                message: format!("Expected type name or literal, got {:?}", token.token_type),
                location: ErrorLocation {
                    line: token.line as usize,
                    column: token.column as usize,
                    position: token.position,
                    length: 1,
                    file: self.file_path.clone(),
                },
                source_context: self.source_content.clone(),
            }),
        }
    }

    /// Parse a namespaced type name like ::hot::time/Instant
    /// Returns the fully qualified name as a string
    fn parse_namespaced_type_name(&mut self) -> ParseResult<String> {
        let mut parts = Vec::new();

        // Consume the initial ::
        if !self.check(&TokenType::DoubleColon) {
            return Err(self.error("Expected '::' at start of namespaced type"));
        }
        self.next(); // consume "::"
        parts.push("::".to_string());

        // Parse the full qualified name: hot::time/Instant (keywords allowed in paths)
        loop {
            let part = self.expect_identifier_or_keyword()?;
            parts.push(part);

            if self.check(&TokenType::DoubleColon) {
                self.next(); // consume "::"
                parts.push("::".to_string());
            } else if self.check(&TokenType::Slash) {
                self.next(); // consume "/"
                parts.push("/".to_string());
            } else {
                // No more parts
                break;
            }
        }

        Ok(parts.join(""))
    }

    /// Extract property name from token (allows keywords as property names)
    fn extract_property_name(&self, token_type: &TokenType) -> Option<String> {
        match token_type {
            TokenType::Identifier(name) => Some(name.clone()),
            TokenType::Keyword(kw) => Some(kw.as_str().to_string()),
            _ => None,
        }
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function to parse Hot source
pub fn parse_hot(source: &str) -> ParseResult<Program> {
    let mut parser = Parser::new();
    parser.parse(source)
}

/// Parse Hot source code from a file with proper error reporting
pub fn parse_hot_file(source: &str, file_path: &str) -> ParseResult<Program> {
    let mut parser = Parser::new().with_file_context(file_path.to_string(), source.to_string());
    parser.parse(source)
}

pub fn parse_hot_expression(source: &str) -> ParseResult<Value> {
    let mut parser = Parser::new();
    parser.parse_expression(source)
}

#[cfg(test)]
mod tests;

// Placeholder lambda helpers for `%` desugaring

/// Check if a Value AST tree contains any placeholder nodes (% or %N)
fn deep_path_has_placeholder_node(dp: &DeepPath) -> bool {
    match dp {
        DeepPath::DynamicIndex(s) => s.starts_with("__placeholder_"),
        DeepPath::Chain(a, b) => {
            deep_path_has_placeholder_node(a) || deep_path_has_placeholder_node(b)
        }
        _ => false,
    }
}

fn deep_path_has_placeholder(var: &Var) -> bool {
    var.deep_path
        .as_ref()
        .is_some_and(deep_path_has_placeholder_node)
}

fn val_contains_placeholder(v: &Val) -> bool {
    match v {
        Val::Box(b) => b
            .as_any()
            .downcast_ref::<AstNode>()
            .is_some_and(|ast| contains_placeholder(&ast.0)),
        Val::Map(m) => m.values().any(val_contains_placeholder),
        Val::Vec(v) => v.iter().any(val_contains_placeholder),
        _ => false,
    }
}

pub(crate) fn contains_placeholder(value: &Value) -> bool {
    match value {
        Value::Placeholder(_) => true,
        Value::Val(v, _) => val_contains_placeholder(v),
        Value::Ref(Ref::Var(vr)) => {
            vr.var.sym.name().starts_with("__placeholder_") || deep_path_has_placeholder(&vr.var)
        }
        Value::FnCall(call) => {
            contains_placeholder(&call.function)
                || call.args.iter().any(|arg| contains_placeholder(&arg.value))
        }
        Value::Flow(flow) => flow.expressions.iter().any(contains_placeholder),
        Value::Cond(_, cond, flow) => {
            contains_placeholder(cond) || flow.expressions.iter().any(contains_placeholder)
        }
        Value::CondDefault(flow) => flow.expressions.iter().any(contains_placeholder),
        Value::Raw(inner) | Value::Do(inner) => contains_placeholder(inner),
        Value::TemplateLiteral(tl) => tl.parts.iter().any(|p| match p {
            TemplatePart::Expression(e) => contains_placeholder(e),
            _ => false,
        }),
        Value::MultipleValues(vals) => vals.iter().any(contains_placeholder),
        Value::Match(m) => {
            contains_placeholder(&m.value)
                || m.arms.iter().any(|arm| contains_placeholder(&arm.body))
        }
        Value::MatchArm(arm) => contains_placeholder(&arm.body),
        Value::MapWithSpread { spread_entries, .. } => {
            spread_entries.iter().any(|(_, v)| contains_placeholder(v))
        }
        _ => false,
    }
}

fn deep_path_max_placeholder_node(dp: &DeepPath) -> usize {
    match dp {
        DeepPath::DynamicIndex(s) => s
            .strip_prefix("__placeholder_")
            .and_then(|r| r.strip_suffix("__"))
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0),
        DeepPath::Chain(a, b) => {
            deep_path_max_placeholder_node(a).max(deep_path_max_placeholder_node(b))
        }
        _ => 0,
    }
}

fn deep_path_max_placeholder(var: &Var) -> usize {
    var.deep_path
        .as_ref()
        .map_or(0, deep_path_max_placeholder_node)
}

fn val_max_placeholder_index(v: &Val) -> usize {
    match v {
        Val::Box(b) => b
            .as_any()
            .downcast_ref::<AstNode>()
            .map_or(0, |ast| max_placeholder_index(&ast.0)),
        Val::Map(m) => m.values().map(val_max_placeholder_index).max().unwrap_or(0),
        Val::Vec(v) => v.iter().map(val_max_placeholder_index).max().unwrap_or(0),
        _ => 0,
    }
}

/// Find the highest 1-based placeholder index in a Value AST tree
pub(crate) fn max_placeholder_index(value: &Value) -> usize {
    match value {
        Value::Placeholder(n) => *n,
        Value::Val(v, _) => val_max_placeholder_index(v),
        Value::Ref(Ref::Var(vr)) => {
            let name_max = {
                let name = vr.var.sym.name();
                if let Some(rest) = name.strip_prefix("__placeholder_") {
                    rest.strip_suffix("__")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0)
                } else {
                    0
                }
            };
            name_max.max(deep_path_max_placeholder(&vr.var))
        }
        Value::FnCall(call) => {
            let fn_max = max_placeholder_index(&call.function);
            let args_max = call
                .args
                .iter()
                .map(|arg| max_placeholder_index(&arg.value))
                .max()
                .unwrap_or(0);
            fn_max.max(args_max)
        }
        Value::Flow(flow) => flow
            .expressions
            .iter()
            .map(max_placeholder_index)
            .max()
            .unwrap_or(0),
        Value::Cond(_, cond, flow) => {
            let cond_max = max_placeholder_index(cond);
            let flow_max = flow
                .expressions
                .iter()
                .map(max_placeholder_index)
                .max()
                .unwrap_or(0);
            cond_max.max(flow_max)
        }
        Value::CondDefault(flow) => flow
            .expressions
            .iter()
            .map(max_placeholder_index)
            .max()
            .unwrap_or(0),
        Value::Raw(inner) | Value::Do(inner) => max_placeholder_index(inner),
        Value::TemplateLiteral(tl) => tl
            .parts
            .iter()
            .map(|p| match p {
                TemplatePart::Expression(e) => max_placeholder_index(e),
                _ => 0,
            })
            .max()
            .unwrap_or(0),
        Value::MultipleValues(vals) => vals.iter().map(max_placeholder_index).max().unwrap_or(0),
        Value::Match(m) => {
            let val_max = max_placeholder_index(&m.value);
            let arms_max = m
                .arms
                .iter()
                .map(|arm| max_placeholder_index(&arm.body))
                .max()
                .unwrap_or(0);
            val_max.max(arms_max)
        }
        Value::MatchArm(arm) => max_placeholder_index(&arm.body),
        Value::MapWithSpread { spread_entries, .. } => spread_entries
            .iter()
            .map(|(_, v)| max_placeholder_index(v))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

fn replace_deep_path_node(dp: DeepPath) -> DeepPath {
    match dp {
        DeepPath::DynamicIndex(ref s) if s.starts_with("__placeholder_") => {
            let n = s
                .strip_prefix("__placeholder_")
                .and_then(|r| r.strip_suffix("__"))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            DeepPath::DynamicIndex(format!("__{}", n - 1))
        }
        DeepPath::Chain(a, b) => DeepPath::Chain(
            Box::new(replace_deep_path_node(*a)),
            Box::new(replace_deep_path_node(*b)),
        ),
        other => other,
    }
}

fn replace_deep_path_placeholders(deep_path: Option<DeepPath>) -> Option<DeepPath> {
    deep_path.map(replace_deep_path_node)
}

fn val_replace_placeholders(v: Val) -> Val {
    match v {
        Val::Box(b) => {
            if b.as_any().downcast_ref::<AstNode>().is_some() {
                let ast = b.into_any().downcast::<AstNode>().unwrap();
                Val::Box(Box::new(AstNode(replace_placeholders(ast.0))))
            } else {
                Val::Box(b)
            }
        }
        Val::Map(m) => {
            let new_map = m
                .into_iter()
                .map(|(k, v)| (k, val_replace_placeholders(v)))
                .collect();
            Val::Map(Box::new(new_map))
        }
        Val::Vec(v) => Val::Vec(v.into_iter().map(val_replace_placeholders).collect()),
        other => other,
    }
}

/// Replace all placeholder nodes with VarRefs to generated param names.
/// Placeholder(n) -> __N-1__ (1-based to 0-based: %1 -> __0, %2 -> __1)
pub(crate) fn replace_placeholders(value: Value) -> Value {
    match value {
        Value::Val(v, ty) => Value::Val(val_replace_placeholders(v), ty),
        Value::Placeholder(n) => {
            let var_name = format!("__{}", n - 1);
            Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String(var_name),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            }))
        }
        Value::Ref(Ref::Var(vr))
            if vr.var.sym.name().starts_with("__placeholder_")
                || deep_path_has_placeholder(&vr.var) =>
        {
            let (new_sym, deep_set) = if vr.var.sym.name().starts_with("__placeholder_") {
                let name = vr.var.sym.name().to_string();
                let n = name
                    .strip_prefix("__placeholder_")
                    .and_then(|s| s.strip_suffix("__"))
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1);
                (Sym::String(format!("__{}", n - 1)), vr.var.deep_set)
            } else {
                (vr.var.sym, vr.var.deep_set)
            };
            let new_deep_path = replace_deep_path_placeholders(vr.var.deep_path);
            Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: new_sym,
                    deep_set,
                    deep_path: new_deep_path,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            }))
        }
        Value::FnCall(mut call) => {
            call.function = Box::new(replace_placeholders(*call.function));
            call.args = call
                .args
                .into_iter()
                .map(|mut arg| {
                    arg.value = replace_placeholders(arg.value);
                    arg
                })
                .collect();
            Value::FnCall(call)
        }
        Value::Flow(mut flow) => {
            flow.expressions = flow
                .expressions
                .into_iter()
                .map(replace_placeholders)
                .collect();
            Value::Flow(flow)
        }
        Value::Cond(label, cond, mut flow) => {
            let cond = replace_placeholders(*cond);
            flow.expressions = flow
                .expressions
                .into_iter()
                .map(replace_placeholders)
                .collect();
            Value::Cond(label, Box::new(cond), flow)
        }
        Value::CondDefault(mut flow) => {
            flow.expressions = flow
                .expressions
                .into_iter()
                .map(replace_placeholders)
                .collect();
            Value::CondDefault(flow)
        }
        Value::Raw(inner) => Value::Raw(Box::new(replace_placeholders(*inner))),
        Value::Do(inner) => Value::Do(Box::new(replace_placeholders(*inner))),
        Value::TemplateLiteral(mut tl) => {
            tl.parts = tl
                .parts
                .into_iter()
                .map(|p| match p {
                    TemplatePart::Expression(e) => {
                        TemplatePart::Expression(Box::new(replace_placeholders(*e)))
                    }
                    other => other,
                })
                .collect();
            Value::TemplateLiteral(tl)
        }
        Value::MultipleValues(vals) => {
            Value::MultipleValues(vals.into_iter().map(replace_placeholders).collect())
        }
        Value::Match(mut m) => {
            m.value = Box::new(replace_placeholders(*m.value));
            m.arms = m
                .arms
                .into_iter()
                .map(|mut arm| {
                    arm.body = replace_placeholders(arm.body);
                    arm
                })
                .collect();
            Value::Match(m)
        }
        Value::MatchArm(mut arm) => {
            arm.body = replace_placeholders(arm.body);
            Value::MatchArm(arm)
        }
        Value::MapWithSpread {
            base_entries,
            mut spread_entries,
        } => {
            spread_entries = spread_entries
                .into_iter()
                .map(|(idx, v)| (idx, replace_placeholders(v)))
                .collect();
            Value::MapWithSpread {
                base_entries,
                spread_entries,
            }
        }
        other => other,
    }
}

/// Create a lambda from an expression containing placeholders.
/// Used by `%(expr)` explicit lambda boundary syntax.
/// Unlike `wrap_placeholder_arg`, this wraps unconditionally (even bare `%`).
fn force_wrap_placeholder(value: Value) -> Value {
    if contains_placeholder(&value) {
        let max_idx = max_placeholder_index(&value);
        let arity = max_idx.max(1);
        let replaced = replace_placeholders(value);
        let args: Vec<FnArg> = (0..arity)
            .map(|i| FnArg {
                var: Var {
                    sym: Sym::String(format!("__{}", i)),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                lazy: false,
                type_annotation: None,
            })
            .collect();
        Value::Lambda(Lambda {
            args: FnArgs {
                args,
                variadic: false,
            },
            body: Box::new(replaced),
            flow_type: None,
        })
    } else {
        value
    }
}
/// If a function call argument contains placeholder(s), wrap it in a lambda.
/// Bare placeholders (just `%` or `%N`) are left unwrapped so they propagate
/// to be wrapped at the enclosing call boundary.
pub(crate) fn wrap_placeholder_arg(value: Value) -> Value {
    if matches!(&value, Value::Placeholder(_)) {
        return value;
    }
    if contains_placeholder(&value) {
        let max_idx = max_placeholder_index(&value);
        let arity = max_idx.max(1); // at least 1 param
        let replaced = replace_placeholders(value);
        let args: Vec<FnArg> = (0..arity)
            .map(|i| FnArg {
                var: Var {
                    sym: Sym::String(format!("__{}", i)),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                lazy: false,
                type_annotation: None,
            })
            .collect();
        Value::Lambda(Lambda {
            args: FnArgs {
                args,
                variadic: false,
            },
            body: Box::new(replaced),
            flow_type: None,
        })
    } else {
        value
    }
}
