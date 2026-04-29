//! AST Cache Serialization
//!
//! This module provides lossless serialization/deserialization for AST types,
//! specifically designed for disk-based caching. Unlike the default serde
//! implementations (which are optimized for user-facing JSON), this module
//! preserves exact type information for all values.
//!
//! Key differences from default serialization:
//! - Val::Map preserves non-string key types (serialized as array of [key, value] pairs)
//! - All Val variants use explicit type tags for unambiguous deserialization
//! - Val::Dec uses string representation to preserve arbitrary precision
//! - All nested Value types are recursively transformed

use crate::val::Val;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

// =============================================================================
// Tagged Val - Lossless Val serialization
// =============================================================================

/// Tagged representation of Val for lossless serialization
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "$v")]
pub enum TaggedVal {
    Int {
        v: i64,
    },
    Dec {
        v: String,
    }, // String to preserve arbitrary precision
    Str {
        v: String,
    },
    Bool {
        v: bool,
    },
    Byte {
        v: u8,
    },
    Bytes {
        v: String,
    }, // Base64 encoded
    Vec {
        v: Vec<TaggedVal>,
    },
    Map {
        entries: Vec<(TaggedVal, TaggedVal)>,
    }, // Array of [key, value] pairs
    Null,
    Box {
        content: String,
    }, // JSON string of boxed content (for FunctionRef, etc.)
    /// Special case: Val::Box containing AstNode wrapping a Value
    /// We serialize the inner Value using CacheableValue
    AstNode {
        value: Box<CacheableValue>,
    },
}

impl From<&Val> for TaggedVal {
    fn from(val: &Val) -> Self {
        match val {
            Val::Int(i) => TaggedVal::Int { v: *i },
            Val::Dec(d) => TaggedVal::Dec { v: d.to_string() },
            Val::Str(s) => TaggedVal::Str {
                v: (*s).to_string(),
            },
            Val::Bool(b) => TaggedVal::Bool { v: *b },
            Val::Byte(b) => TaggedVal::Byte { v: *b },
            Val::Bytes(bytes) => TaggedVal::Bytes {
                v: crate::lang::hot::base64::encode_bytes_to_base64(bytes),
            },
            Val::Vec(vec) => TaggedVal::Vec {
                v: vec.iter().map(TaggedVal::from).collect(),
            },
            Val::Map(map) => TaggedVal::Map {
                entries: map
                    .iter()
                    .map(|(k, v)| (TaggedVal::from(k), TaggedVal::from(v)))
                    .collect(),
            },
            Val::Box(b) => {
                // Check if this is an AstNode containing a Value
                if let Some(ast_node) = b.as_any().downcast_ref::<crate::lang::ast::AstNode>() {
                    TaggedVal::AstNode {
                        value: Box::new(CacheableValue::from(&ast_node.0)),
                    }
                } else {
                    // For other boxed types (FunctionRef, etc.), use JSON serialization
                    TaggedVal::Box {
                        content: b
                            .serialize_json()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|_| "null".to_string()),
                    }
                }
            }
            Val::Null => TaggedVal::Null,
        }
    }
}

impl TryFrom<TaggedVal> for Val {
    type Error = String;

    fn try_from(tagged: TaggedVal) -> Result<Self, Self::Error> {
        match tagged {
            TaggedVal::Int { v } => Ok(Val::Int(v)),
            TaggedVal::Dec { v } => {
                let decimal =
                    fastnum::decimal::D256::from_str(&v, fastnum::decimal::Context::default())
                        .map_err(|e| format!("Failed to parse decimal '{}': {:?}", v, e))?;
                Ok(Val::Dec(decimal))
            }
            TaggedVal::Str { v } => Ok(Val::from(v)),
            TaggedVal::Bool { v } => Ok(Val::Bool(v)),
            TaggedVal::Byte { v } => Ok(Val::Byte(v)),
            TaggedVal::Bytes { v } => {
                let bytes = crate::lang::hot::base64::decode_base64_to_bytes(&v)
                    .map_err(|e| format!("Failed to decode base64: {}", e))?;
                Ok(Val::Bytes(bytes))
            }
            TaggedVal::Vec { v } => {
                let vec: Result<Vec<Val>, String> = v.into_iter().map(Val::try_from).collect();
                Ok(Val::Vec(vec?))
            }
            TaggedVal::Map { entries } => {
                let map: Result<IndexMap<Val, Val>, String> = entries
                    .into_iter()
                    .map(|(k, v)| Ok((Val::try_from(k)?, Val::try_from(v)?)))
                    .collect();
                Ok(Val::Map(Box::new(map?)))
            }
            TaggedVal::Box { content } => {
                // Try to reconstruct FunctionRef first
                if let Ok(func_ref) = serde_json::from_str::<
                    crate::lang::runtime::function_ref::FunctionRef,
                >(&content)
                {
                    return Ok(Val::Box(Box::new(func_ref)));
                }
                // Try to reconstruct TypeRef (for variant union types)
                if let Ok(type_ref) = serde_json::from_str::<crate::lang::refs::TypeRef>(&content) {
                    return Ok(Val::Box(Box::new(type_ref)));
                }
                // Fallback to null for other boxed types (they're typically not cacheable)
                Ok(Val::Null)
            }
            TaggedVal::AstNode { value } => {
                // Reconstruct the Value from CacheableValue, then wrap in AstNode
                let ast_value = crate::lang::ast::Value::try_from(*value)?;
                Ok(Val::Box(Box::new(crate::lang::ast::AstNode(ast_value))))
            }
            TaggedVal::Null => Ok(Val::Null),
        }
    }
}

// =============================================================================
// Cacheable AST Types - Recursive Value handling
// =============================================================================

/// Cacheable Ref (VarRef or NsRef)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "$ref_type")]
pub enum CacheableRef {
    Var { var_ref: CacheableVarRef },
    Ns { ns_ref: CacheableNsRef },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableVarRef {
    pub var: CacheableVar,
    pub data: Option<CacheableVarData>,
    pub src: Option<CacheableSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableVarData {
    pub type_annotation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableNsRef {
    pub ns: String, // NsPath as string
    pub src: Option<CacheableSource>,
    pub function_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableSource {
    pub file: Option<String>,
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableVar {
    pub sym: String,
    pub deep_set: Option<CacheableDeepPath>,
    pub deep_path: Option<CacheableDeepPath>,
    pub meta: Option<CacheableMeta>,
    pub src: Option<CacheableSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "$dp_type")]
pub enum CacheableDeepPath {
    Index {
        i: usize,
    },
    Key {
        k: String,
    },
    DynamicIndex {
        var: String,
    },
    Chain {
        left: Box<CacheableDeepPath>,
        right: Box<CacheableDeepPath>,
    },
    Append,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableMeta {
    pub val: TaggedVal,
}

/// Cacheable FnCall with recursive Value handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableFnCall {
    pub function: Box<CacheableValue>,
    pub args: Vec<CacheableFnCallArg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<CacheableDeepPath>,
    pub src: Option<CacheableSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableFnCallArg {
    pub value: CacheableValue,
    pub lazy: bool,
    pub spread: bool,
    pub src: Option<CacheableSource>,
}

/// Cacheable Flow with recursive Value handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableFlow {
    pub flow_type: String, // FlowType as string
    pub expressions: Vec<CacheableValue>,
    pub result_modifier: Option<String>, // ResultModifier as string
    pub src: Option<CacheableSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<(usize, String, String)>,
}

/// Cacheable FnDef with recursive Value handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableFnDef {
    pub args: CacheableFnArgs,
    pub body: CacheableValue,
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableFnArgs {
    pub args: Vec<CacheableFnArg>,
    pub variadic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableFnArg {
    pub var: CacheableVar,
    pub lazy: bool,
    pub type_annotation: Option<String>,
}

/// Cacheable Lambda with recursive Value handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableLambda {
    pub args: CacheableFnArgs,
    pub body: Box<CacheableValue>,
    #[serde(default)]
    pub flow_type: Option<String>, // FlowType as string (e.g., "Serial", "Parallel")
}

/// Cacheable TemplateLiteral with recursive Value handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableTemplateLiteral {
    pub parts: Vec<CacheableTemplatePart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "$tp_type")]
pub enum CacheableTemplatePart {
    Text { text: String },
    Expression { expr: Box<CacheableValue> },
}

/// Cacheable variant definition for tagged unions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableVariantDef {
    pub name: String,
    pub type_ref: Option<String>,
}

/// Cacheable TypeDef
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableTypeDef {
    pub name: String,
    pub fields: Option<Vec<CacheableTypeField>>,
    /// Type alias expression (e.g. for `Fruit type "apple" | "banana"`).
    /// Stored as the actual `TypeExpr` so literal-union aliases survive a
    /// cache round-trip; previously we serialized `to_string()` and dropped
    /// the value on read, which silently broke compile-time checks against
    /// these unions whenever the AST cache was warm.
    pub type_alias: Option<crate::lang::ast::TypeExpr>,
    pub variants: Option<Vec<CacheableVariantDef>>, // For variant unions
    pub constructor_functions: Option<Vec<CacheableFnDef>>,
    pub implementations: Option<Vec<CacheableTypeImplementation>>,
    /// Mirrors `TypeDef::is_open`. `#[serde(default)]` keeps older cache
    /// files (without this field) loadable as `is_open = false`.
    #[serde(default)]
    pub is_open: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableTypeField {
    pub name: String,
    pub type_annotation: String,
}

/// Cacheable TypeImplementation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableTypeImplementation {
    pub source_type: String,
    pub target_type: String,
    pub implementation: Box<CacheableValue>,
    /// Mirrors `TypeImplementation::src`. `#[serde(default)]` keeps older
    /// cache files (without this field) loadable as `src = None`.
    #[serde(default)]
    pub src: Option<crate::lang::ast::Source>,
}

// =============================================================================
// CacheableValue - The main recursive Value wrapper
// =============================================================================

/// Wrapper for AST Value that uses tagged Val serialization
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "$value_type")]
pub enum CacheableValue {
    Val {
        val: TaggedVal,
        type_info: Option<String>,
    },
    Ref {
        ref_data: CacheableRef,
    },
    FnCall {
        fn_call: CacheableFnCall,
    },
    Flow {
        flow: CacheableFlow,
    },
    Fn {
        fn_defs: Vec<CacheableFnDef>,
    },
    TypeDef {
        type_def: CacheableTypeDef,
    },
    TypeImplementation {
        impl_def: CacheableTypeImplementation,
    },
    Unbound {
        name: String,
    },
    Cond {
        label: Option<String>,
        condition: Box<CacheableValue>,
        flow: CacheableFlow,
    },
    CondDefault {
        flow: CacheableFlow,
    },
    TemplateLiteral {
        template: CacheableTemplateLiteral,
    },
    Raw {
        inner: Box<CacheableValue>,
    },
    Do {
        inner: Box<CacheableValue>,
    },
    VariadicExpansion {
        name: String,
    },
    MultipleValues {
        values: Vec<CacheableValue>,
    },
    Lambda {
        lambda: CacheableLambda,
    },
    Match {
        match_expr: CacheableMatchExpr,
    },
    MatchArm {
        arm: Box<CacheableMatchArm>,
    },
    MapWithSpread {
        base_entries: Vec<(TaggedVal, TaggedVal)>,
        spread_entries: Vec<(usize, CacheableValue)>,
    },
    Placeholder {
        index: usize,
    },
}

/// Cacheable version of MatchExpr
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableMatchExpr {
    pub value: Box<CacheableValue>,
    pub arms: Vec<CacheableMatchArm>,
    pub match_all: bool,
    pub result_modifier: Option<CacheableResultModifier>,
    pub src: Option<CacheableSource>,
}

/// Cacheable version of ResultModifier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CacheableResultModifier {
    One,
    Map,
    Vec,
}

/// Cacheable version of MatchArm
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableMatchArm {
    pub type_name: Option<String>,
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_literal: Option<Box<CacheableValue>>,
    pub binding: Option<String>,
    pub body: Box<CacheableValue>,
    pub src: Option<CacheableSource>,
}

// =============================================================================
// Conversion: AST -> Cacheable
// =============================================================================

impl From<&crate::lang::ast::Source> for CacheableSource {
    fn from(src: &crate::lang::ast::Source) -> Self {
        CacheableSource {
            file: src.file.clone(),
            line: src.line,
            column: src.column,
            position: src.position,
            length: src.length,
        }
    }
}

impl From<&crate::lang::ast::DeepPath> for CacheableDeepPath {
    fn from(dp: &crate::lang::ast::DeepPath) -> Self {
        use crate::lang::ast::DeepPath;
        match dp {
            DeepPath::Index(i) => CacheableDeepPath::Index { i: *i },
            DeepPath::Key(k) => CacheableDeepPath::Key { k: k.clone() },
            DeepPath::DynamicIndex(var) => CacheableDeepPath::DynamicIndex { var: var.clone() },
            DeepPath::Chain(l, r) => CacheableDeepPath::Chain {
                left: Box::new(CacheableDeepPath::from(l.as_ref())),
                right: Box::new(CacheableDeepPath::from(r.as_ref())),
            },
            DeepPath::Append => CacheableDeepPath::Append,
        }
    }
}

impl From<&crate::lang::ast::Meta> for CacheableMeta {
    fn from(meta: &crate::lang::ast::Meta) -> Self {
        CacheableMeta {
            val: TaggedVal::from(&meta.val),
        }
    }
}

impl From<&crate::lang::ast::Var> for CacheableVar {
    fn from(var: &crate::lang::ast::Var) -> Self {
        CacheableVar {
            sym: var.sym.name().to_string(),
            deep_set: var.deep_set.as_ref().map(CacheableDeepPath::from),
            deep_path: var.deep_path.as_ref().map(CacheableDeepPath::from),
            meta: var.meta.as_ref().map(CacheableMeta::from),
            src: var.src.as_ref().map(CacheableSource::from),
        }
    }
}

impl From<&crate::lang::ast::VarData> for CacheableVarData {
    fn from(vd: &crate::lang::ast::VarData) -> Self {
        CacheableVarData {
            type_annotation: vd.type_annotation.clone(),
        }
    }
}

impl From<&crate::lang::ast::VarRef> for CacheableVarRef {
    fn from(vr: &crate::lang::ast::VarRef) -> Self {
        CacheableVarRef {
            var: CacheableVar::from(&vr.var),
            data: vr.data.as_ref().map(CacheableVarData::from),
            src: vr.src.as_ref().map(CacheableSource::from),
        }
    }
}

impl From<&crate::lang::ast::NsRef> for CacheableNsRef {
    fn from(nr: &crate::lang::ast::NsRef) -> Self {
        CacheableNsRef {
            ns: nr.ns.to_string(),
            src: nr.src.as_ref().map(CacheableSource::from),
            function_name: nr.function_name.clone(),
        }
    }
}

impl From<&crate::lang::ast::Ref> for CacheableRef {
    fn from(r: &crate::lang::ast::Ref) -> Self {
        use crate::lang::ast::Ref;
        match r {
            Ref::Var(vr) => CacheableRef::Var {
                var_ref: CacheableVarRef::from(vr),
            },
            Ref::Ns(nr) => CacheableRef::Ns {
                ns_ref: CacheableNsRef::from(nr),
            },
        }
    }
}

impl From<&crate::lang::ast::FnCallArg> for CacheableFnCallArg {
    fn from(arg: &crate::lang::ast::FnCallArg) -> Self {
        CacheableFnCallArg {
            value: CacheableValue::from(&arg.value),
            lazy: arg.lazy,
            spread: arg.spread,
            src: arg.src.as_ref().map(CacheableSource::from),
        }
    }
}

impl From<&crate::lang::ast::FnCall> for CacheableFnCall {
    fn from(fc: &crate::lang::ast::FnCall) -> Self {
        CacheableFnCall {
            function: Box::new(CacheableValue::from(fc.function.as_ref())),
            args: fc.args.iter().map(CacheableFnCallArg::from).collect(),
            result_path: fc.result_path.as_ref().map(CacheableDeepPath::from),
            src: fc.src.as_ref().map(CacheableSource::from),
        }
    }
}

impl From<&crate::lang::ast::Flow> for CacheableFlow {
    fn from(f: &crate::lang::ast::Flow) -> Self {
        CacheableFlow {
            flow_type: format!("{:?}", f.flow_type),
            expressions: f.expressions.iter().map(CacheableValue::from).collect(),
            result_modifier: f.result_modifier.as_ref().map(|rm| format!("{:?}", rm)),
            src: f.src.as_ref().map(CacheableSource::from),
            aliases: f
                .aliases
                .iter()
                .map(|(pos, alias, source)| (*pos, format!("{}", alias), format!("{}", source)))
                .collect(),
        }
    }
}

impl From<&crate::lang::ast::FnArg> for CacheableFnArg {
    fn from(arg: &crate::lang::ast::FnArg) -> Self {
        CacheableFnArg {
            var: CacheableVar::from(&arg.var),
            lazy: arg.lazy,
            type_annotation: arg.type_annotation.clone(),
        }
    }
}

impl From<&crate::lang::ast::FnArgs> for CacheableFnArgs {
    fn from(args: &crate::lang::ast::FnArgs) -> Self {
        CacheableFnArgs {
            args: args.args.iter().map(CacheableFnArg::from).collect(),
            variadic: args.variadic,
        }
    }
}

impl From<&crate::lang::ast::FnDef> for CacheableFnDef {
    fn from(fd: &crate::lang::ast::FnDef) -> Self {
        CacheableFnDef {
            args: CacheableFnArgs::from(&fd.args),
            body: CacheableValue::from(&fd.body),
            return_type: fd.return_type.clone(),
        }
    }
}

impl From<&crate::lang::ast::Lambda> for CacheableLambda {
    fn from(l: &crate::lang::ast::Lambda) -> Self {
        CacheableLambda {
            args: CacheableFnArgs::from(&l.args),
            body: Box::new(CacheableValue::from(l.body.as_ref())),
            flow_type: l.flow_type.as_ref().map(|ft| format!("{:?}", ft)),
        }
    }
}

impl From<&crate::lang::ast::TemplatePart> for CacheableTemplatePart {
    fn from(tp: &crate::lang::ast::TemplatePart) -> Self {
        use crate::lang::ast::TemplatePart;
        match tp {
            TemplatePart::Text(t) => CacheableTemplatePart::Text { text: t.clone() },
            TemplatePart::Expression(e) => CacheableTemplatePart::Expression {
                expr: Box::new(CacheableValue::from(e.as_ref())),
            },
        }
    }
}

impl From<&crate::lang::ast::TemplateLiteral> for CacheableTemplateLiteral {
    fn from(tl: &crate::lang::ast::TemplateLiteral) -> Self {
        CacheableTemplateLiteral {
            parts: tl.parts.iter().map(CacheableTemplatePart::from).collect(),
        }
    }
}

impl From<&crate::lang::ast::TypeField> for CacheableTypeField {
    fn from(tf: &crate::lang::ast::TypeField) -> Self {
        CacheableTypeField {
            name: tf.name.name().to_string(),
            type_annotation: tf.type_annotation.clone(),
        }
    }
}

impl From<&crate::lang::ast::TypeImplementation> for CacheableTypeImplementation {
    fn from(ti: &crate::lang::ast::TypeImplementation) -> Self {
        CacheableTypeImplementation {
            source_type: ti.source_type.clone(),
            target_type: ti.target_type.clone(),
            implementation: Box::new(CacheableValue::from(&ti.implementation)),
            src: ti.src.clone(),
        }
    }
}

impl From<&crate::lang::ast::TypeDef> for CacheableTypeDef {
    fn from(td: &crate::lang::ast::TypeDef) -> Self {
        CacheableTypeDef {
            name: td.name.name().to_string(),
            fields: td
                .fields
                .as_ref()
                .map(|fs| fs.iter().map(CacheableTypeField::from).collect()),
            type_alias: td.type_alias.clone(),
            variants: td.variants.as_ref().map(|vs| {
                vs.iter()
                    .map(|v| CacheableVariantDef {
                        name: v.name.name().to_string(),
                        type_ref: v.type_ref.clone(),
                    })
                    .collect()
            }),
            constructor_functions: td
                .constructor_functions
                .as_ref()
                .map(|cfs| cfs.iter().map(CacheableFnDef::from).collect()),
            implementations: td
                .implementations
                .as_ref()
                .map(|is| is.iter().map(CacheableTypeImplementation::from).collect()),
            is_open: td.is_open,
        }
    }
}

impl From<&crate::lang::ast::Value> for CacheableValue {
    fn from(value: &crate::lang::ast::Value) -> Self {
        use crate::lang::ast::Value;

        match value {
            Value::Val(val, type_info) => CacheableValue::Val {
                val: TaggedVal::from(val),
                type_info: type_info.clone(),
            },
            Value::Ref(r) => CacheableValue::Ref {
                ref_data: CacheableRef::from(r),
            },
            Value::FnCall(fc) => CacheableValue::FnCall {
                fn_call: CacheableFnCall::from(fc),
            },
            Value::Flow(f) => CacheableValue::Flow {
                flow: CacheableFlow::from(f),
            },
            Value::Fn(defs) => CacheableValue::Fn {
                fn_defs: defs.iter().map(CacheableFnDef::from).collect(),
            },
            Value::TypeDef(td) => CacheableValue::TypeDef {
                type_def: CacheableTypeDef::from(td),
            },
            Value::TypeImplementation(ti) => CacheableValue::TypeImplementation {
                impl_def: CacheableTypeImplementation::from(ti.as_ref()),
            },
            Value::Unbound(u) => CacheableValue::Unbound {
                name: u.name.clone(),
            },
            Value::Cond(label, cond, flow) => CacheableValue::Cond {
                label: label.clone(),
                condition: Box::new(CacheableValue::from(cond.as_ref())),
                flow: CacheableFlow::from(flow),
            },
            Value::CondDefault(flow) => CacheableValue::CondDefault {
                flow: CacheableFlow::from(flow),
            },
            Value::TemplateLiteral(tl) => CacheableValue::TemplateLiteral {
                template: CacheableTemplateLiteral::from(tl),
            },
            Value::Raw(inner) => CacheableValue::Raw {
                inner: Box::new(CacheableValue::from(inner.as_ref())),
            },
            Value::Do(inner) => CacheableValue::Do {
                inner: Box::new(CacheableValue::from(inner.as_ref())),
            },
            Value::VariadicExpansion(name) => {
                CacheableValue::VariadicExpansion { name: name.clone() }
            }
            Value::MultipleValues(values) => CacheableValue::MultipleValues {
                values: values.iter().map(CacheableValue::from).collect(),
            },
            Value::Lambda(l) => CacheableValue::Lambda {
                lambda: CacheableLambda::from(l),
            },
            Value::Match(m) => CacheableValue::Match {
                match_expr: CacheableMatchExpr {
                    value: Box::new(CacheableValue::from(m.value.as_ref())),
                    arms: m
                        .arms
                        .iter()
                        .map(|arm| CacheableMatchArm {
                            type_name: arm.type_name.clone(),
                            variant: arm.variant.clone(),
                            value_literal: arm
                                .value_literal
                                .as_ref()
                                .map(|v| Box::new(CacheableValue::from(v.as_ref()))),
                            binding: arm.binding.clone(),
                            body: Box::new(CacheableValue::from(&arm.body)),
                            src: arm.src.as_ref().map(CacheableSource::from),
                        })
                        .collect(),
                    match_all: m.match_all,
                    result_modifier: m.result_modifier.as_ref().map(|rm| match rm {
                        crate::lang::ast::ResultModifier::One => CacheableResultModifier::One,
                        crate::lang::ast::ResultModifier::Map => CacheableResultModifier::Map,
                        crate::lang::ast::ResultModifier::Vec => CacheableResultModifier::Vec,
                    }),
                    src: m.src.as_ref().map(CacheableSource::from),
                },
            },
            Value::MatchArm(arm) => CacheableValue::MatchArm {
                arm: Box::new(CacheableMatchArm {
                    type_name: arm.type_name.clone(),
                    variant: arm.variant.clone(),
                    value_literal: arm
                        .value_literal
                        .as_ref()
                        .map(|v| Box::new(CacheableValue::from(v.as_ref()))),
                    binding: arm.binding.clone(),
                    body: Box::new(CacheableValue::from(&arm.body)),
                    src: arm.src.as_ref().map(CacheableSource::from),
                }),
            },
            Value::MapWithSpread {
                base_entries,
                spread_entries,
            } => CacheableValue::MapWithSpread {
                base_entries: base_entries
                    .iter()
                    .map(|(k, v)| (TaggedVal::from(k), TaggedVal::from(v)))
                    .collect(),
                spread_entries: spread_entries
                    .iter()
                    .map(|(idx, val)| (*idx, CacheableValue::from(val)))
                    .collect(),
            },
            Value::Placeholder(n) => CacheableValue::Placeholder { index: *n },
        }
    }
}

// =============================================================================
// Conversion: Cacheable -> AST
// =============================================================================

impl TryFrom<CacheableSource> for crate::lang::ast::Source {
    type Error = String;

    fn try_from(cs: CacheableSource) -> Result<Self, Self::Error> {
        Ok(crate::lang::ast::Source::new(
            cs.file,
            cs.line,
            cs.column,
            cs.position,
            cs.length,
        ))
    }
}

impl TryFrom<CacheableDeepPath> for crate::lang::ast::DeepPath {
    type Error = String;

    fn try_from(cdp: CacheableDeepPath) -> Result<Self, Self::Error> {
        use crate::lang::ast::DeepPath;
        match cdp {
            CacheableDeepPath::Index { i } => Ok(DeepPath::Index(i)),
            CacheableDeepPath::Key { k } => Ok(DeepPath::Key(k)),
            CacheableDeepPath::DynamicIndex { var } => Ok(DeepPath::DynamicIndex(var)),
            CacheableDeepPath::Chain { left, right } => Ok(DeepPath::Chain(
                Box::new(DeepPath::try_from(*left)?),
                Box::new(DeepPath::try_from(*right)?),
            )),
            CacheableDeepPath::Append => Ok(DeepPath::Append),
        }
    }
}

impl TryFrom<CacheableMeta> for crate::lang::ast::Meta {
    type Error = String;

    fn try_from(cm: CacheableMeta) -> Result<Self, Self::Error> {
        Ok(crate::lang::ast::Meta {
            val: Val::try_from(cm.val)?,
        })
    }
}

impl TryFrom<CacheableVar> for crate::lang::ast::Var {
    type Error = String;

    fn try_from(cv: CacheableVar) -> Result<Self, Self::Error> {
        use crate::lang::ast::{DeepPath, Meta, Source, Sym, Var};
        Ok(Var {
            sym: Sym::String(cv.sym),
            deep_set: cv.deep_set.map(DeepPath::try_from).transpose()?,
            deep_path: cv.deep_path.map(DeepPath::try_from).transpose()?,
            meta: cv.meta.map(Meta::try_from).transpose()?,
            src: cv.src.map(Source::try_from).transpose()?,
        })
    }
}

impl TryFrom<CacheableVarData> for crate::lang::ast::VarData {
    type Error = String;

    fn try_from(cvd: CacheableVarData) -> Result<Self, Self::Error> {
        Ok(crate::lang::ast::VarData {
            type_annotation: cvd.type_annotation,
        })
    }
}

impl TryFrom<CacheableVarRef> for crate::lang::ast::VarRef {
    type Error = String;

    fn try_from(cvr: CacheableVarRef) -> Result<Self, Self::Error> {
        use crate::lang::ast::{Source, Var, VarData, VarRef};
        Ok(VarRef {
            var: Var::try_from(cvr.var)?,
            data: cvr.data.map(VarData::try_from).transpose()?,
            src: cvr.src.map(Source::try_from).transpose()?,
        })
    }
}

impl TryFrom<CacheableNsRef> for crate::lang::ast::NsRef {
    type Error = String;

    fn try_from(cnr: CacheableNsRef) -> Result<Self, Self::Error> {
        use crate::lang::ast::{NsPath, NsRef, Source};
        Ok(NsRef {
            ns: NsPath::from_string(&cnr.ns),
            src: cnr.src.map(Source::try_from).transpose()?,
            function_name: cnr.function_name,
        })
    }
}

impl TryFrom<CacheableRef> for crate::lang::ast::Ref {
    type Error = String;

    fn try_from(cr: CacheableRef) -> Result<Self, Self::Error> {
        use crate::lang::ast::{NsRef, Ref, VarRef};
        match cr {
            CacheableRef::Var { var_ref } => Ok(Ref::Var(VarRef::try_from(var_ref)?)),
            CacheableRef::Ns { ns_ref } => Ok(Ref::Ns(NsRef::try_from(ns_ref)?)),
        }
    }
}

impl TryFrom<CacheableFnCallArg> for crate::lang::ast::FnCallArg {
    type Error = String;

    fn try_from(cfa: CacheableFnCallArg) -> Result<Self, Self::Error> {
        use crate::lang::ast::{FnCallArg, Source, Value};
        Ok(FnCallArg {
            value: Value::try_from(cfa.value)?,
            lazy: cfa.lazy,
            spread: cfa.spread,
            src: cfa.src.map(Source::try_from).transpose()?,
        })
    }
}

impl TryFrom<CacheableFnCall> for crate::lang::ast::FnCall {
    type Error = String;

    fn try_from(cfc: CacheableFnCall) -> Result<Self, Self::Error> {
        use crate::lang::ast::{DeepPath, FnCall, FnCallArg, Source, Value};
        Ok(FnCall {
            function: Box::new(Value::try_from(*cfc.function)?),
            args: cfc
                .args
                .into_iter()
                .map(FnCallArg::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            result_path: cfc.result_path.map(DeepPath::try_from).transpose()?,
            src: cfc.src.map(Source::try_from).transpose()?,
        })
    }
}

fn parse_flow_type(s: &str) -> crate::lang::ast::FlowType {
    use crate::lang::ast::FlowType;
    match s {
        "Serial" => FlowType::Serial,
        "Parallel" => FlowType::Parallel,
        "Pipe" => FlowType::Pipe,
        "Cond" => FlowType::Cond,
        "CondAll" => FlowType::CondAll,
        "Match" => FlowType::Match,
        "MatchAll" => FlowType::MatchAll,
        "MatchShort" => FlowType::MatchShort,
        "MatchAllShort" => FlowType::MatchAllShort,
        _ => FlowType::Serial, // Default fallback
    }
}

fn parse_result_modifier(s: &str) -> crate::lang::ast::ResultModifier {
    use crate::lang::ast::ResultModifier;
    match s {
        "One" => ResultModifier::One,
        "Vec" => ResultModifier::Vec,
        "Map" => ResultModifier::Map,
        _ => ResultModifier::One, // Default fallback
    }
}

impl TryFrom<CacheableFlow> for crate::lang::ast::Flow {
    type Error = String;

    fn try_from(cf: CacheableFlow) -> Result<Self, Self::Error> {
        use crate::lang::ast::{Flow, NsPath, Source, Value};
        Ok(Flow {
            flow_type: parse_flow_type(&cf.flow_type),
            expressions: cf
                .expressions
                .into_iter()
                .map(Value::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            result_modifier: cf.result_modifier.map(|s| parse_result_modifier(&s)),
            src: cf.src.map(|s| Source {
                file: s.file,
                line: s.line,
                column: s.column,
                position: s.position,
                length: s.length,
            }),
            aliases: cf
                .aliases
                .into_iter()
                .map(|(pos, alias, source)| (pos, NsPath::from(&alias), NsPath::from(&source)))
                .collect(),
        })
    }
}

impl TryFrom<CacheableFnArg> for crate::lang::ast::FnArg {
    type Error = String;

    fn try_from(cfa: CacheableFnArg) -> Result<Self, Self::Error> {
        use crate::lang::ast::{FnArg, Var};
        Ok(FnArg {
            var: Var::try_from(cfa.var)?,
            lazy: cfa.lazy,
            type_annotation: cfa.type_annotation,
        })
    }
}

impl TryFrom<CacheableFnArgs> for crate::lang::ast::FnArgs {
    type Error = String;

    fn try_from(cfa: CacheableFnArgs) -> Result<Self, Self::Error> {
        use crate::lang::ast::{FnArg, FnArgs};
        Ok(FnArgs {
            args: cfa
                .args
                .into_iter()
                .map(FnArg::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            variadic: cfa.variadic,
        })
    }
}

impl TryFrom<CacheableFnDef> for crate::lang::ast::FnDef {
    type Error = String;

    fn try_from(cfd: CacheableFnDef) -> Result<Self, Self::Error> {
        use crate::lang::ast::{FnArgs, FnDef, Value};
        Ok(FnDef {
            args: FnArgs::try_from(cfd.args)?,
            body: Value::try_from(cfd.body)?,
            return_type: cfd.return_type,
        })
    }
}

impl TryFrom<CacheableLambda> for crate::lang::ast::Lambda {
    type Error = String;

    fn try_from(cl: CacheableLambda) -> Result<Self, Self::Error> {
        use crate::lang::ast::{FnArgs, Lambda, Value};
        Ok(Lambda {
            args: FnArgs::try_from(cl.args)?,
            body: Box::new(Value::try_from(*cl.body)?),
            flow_type: cl.flow_type.map(|s| parse_flow_type(&s)),
        })
    }
}

impl TryFrom<CacheableTemplatePart> for crate::lang::ast::TemplatePart {
    type Error = String;

    fn try_from(ctp: CacheableTemplatePart) -> Result<Self, Self::Error> {
        use crate::lang::ast::{TemplatePart, Value};
        match ctp {
            CacheableTemplatePart::Text { text } => Ok(TemplatePart::Text(text)),
            CacheableTemplatePart::Expression { expr } => {
                Ok(TemplatePart::Expression(Box::new(Value::try_from(*expr)?)))
            }
        }
    }
}

impl TryFrom<CacheableTemplateLiteral> for crate::lang::ast::TemplateLiteral {
    type Error = String;

    fn try_from(ctl: CacheableTemplateLiteral) -> Result<Self, Self::Error> {
        use crate::lang::ast::{TemplateLiteral, TemplatePart};
        Ok(TemplateLiteral {
            parts: ctl
                .parts
                .into_iter()
                .map(TemplatePart::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl TryFrom<CacheableTypeField> for crate::lang::ast::TypeField {
    type Error = String;

    fn try_from(ctf: CacheableTypeField) -> Result<Self, Self::Error> {
        use crate::lang::ast::{Sym, TypeField};
        Ok(TypeField {
            name: Sym::String(ctf.name),
            type_annotation: ctf.type_annotation,
        })
    }
}

impl TryFrom<CacheableTypeImplementation> for crate::lang::ast::TypeImplementation {
    type Error = String;

    fn try_from(cti: CacheableTypeImplementation) -> Result<Self, Self::Error> {
        use crate::lang::ast::{TypeImplementation, Value};
        Ok(TypeImplementation {
            source_type: cti.source_type,
            target_type: cti.target_type,
            implementation: Value::try_from(*cti.implementation)?,
            src: cti.src,
        })
    }
}

impl TryFrom<CacheableTypeDef> for crate::lang::ast::TypeDef {
    type Error = String;

    fn try_from(ctd: CacheableTypeDef) -> Result<Self, Self::Error> {
        use crate::lang::ast::{FnDef, Sym, TypeDef, TypeField, TypeImplementation, VariantDef};
        Ok(TypeDef {
            name: Sym::String(ctd.name),
            fields: ctd
                .fields
                .map(|fs| {
                    fs.into_iter()
                        .map(TypeField::try_from)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?,
            type_alias: ctd.type_alias,
            variants: ctd.variants.map(|vs| {
                vs.into_iter()
                    .map(|v| VariantDef {
                        name: Sym::String(v.name),
                        type_ref: v.type_ref,
                    })
                    .collect()
            }),
            constructor_functions: ctd
                .constructor_functions
                .map(|cfs| {
                    cfs.into_iter()
                        .map(FnDef::try_from)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?,
            implementations: ctd
                .implementations
                .map(|is| {
                    is.into_iter()
                        .map(TypeImplementation::try_from)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?,
            is_open: ctd.is_open,
        })
    }
}

impl TryFrom<CacheableValue> for crate::lang::ast::Value {
    type Error = String;

    fn try_from(cv: CacheableValue) -> Result<Self, Self::Error> {
        use crate::lang::ast::{
            Flow, FnCall, FnDef, Lambda, Ref, TemplateLiteral, TypeDef, TypeImplementation,
            Unbound, Value,
        };

        match cv {
            CacheableValue::Val { val, type_info } => {
                Ok(Value::Val(Val::try_from(val)?, type_info))
            }
            CacheableValue::Ref { ref_data } => Ok(Value::Ref(Ref::try_from(ref_data)?)),
            CacheableValue::FnCall { fn_call } => Ok(Value::FnCall(FnCall::try_from(fn_call)?)),
            CacheableValue::Flow { flow } => Ok(Value::Flow(Flow::try_from(flow)?)),
            CacheableValue::Fn { fn_defs } => {
                let defs: Result<Vec<FnDef>, String> =
                    fn_defs.into_iter().map(FnDef::try_from).collect();
                Ok(Value::Fn(defs?))
            }
            CacheableValue::TypeDef { type_def } => {
                Ok(Value::TypeDef(TypeDef::try_from(type_def)?))
            }
            CacheableValue::TypeImplementation { impl_def } => Ok(Value::TypeImplementation(
                Box::new(TypeImplementation::try_from(impl_def)?),
            )),
            CacheableValue::Unbound { name } => Ok(Value::Unbound(Unbound { name })),
            CacheableValue::Cond {
                label,
                condition,
                flow,
            } => {
                let cond = Value::try_from(*condition)?;
                let f = Flow::try_from(flow)?;
                Ok(Value::Cond(label, Box::new(cond), f))
            }
            CacheableValue::CondDefault { flow } => Ok(Value::CondDefault(Flow::try_from(flow)?)),
            CacheableValue::TemplateLiteral { template } => {
                Ok(Value::TemplateLiteral(TemplateLiteral::try_from(template)?))
            }
            CacheableValue::Raw { inner } => {
                let v = Value::try_from(*inner)?;
                Ok(Value::Raw(Box::new(v)))
            }
            CacheableValue::Do { inner } => {
                let v = Value::try_from(*inner)?;
                Ok(Value::Do(Box::new(v)))
            }
            CacheableValue::VariadicExpansion { name } => Ok(Value::VariadicExpansion(name)),
            CacheableValue::MultipleValues { values } => {
                let vs: Result<Vec<Value>, String> =
                    values.into_iter().map(Value::try_from).collect();
                Ok(Value::MultipleValues(vs?))
            }
            CacheableValue::Lambda { lambda } => Ok(Value::Lambda(Lambda::try_from(lambda)?)),
            CacheableValue::Match { match_expr } => {
                use crate::lang::ast::{MatchArm, MatchExpr, Source};
                let arms: Result<Vec<MatchArm>, String> = match_expr
                    .arms
                    .into_iter()
                    .map(|arm| {
                        Ok(MatchArm {
                            type_name: arm.type_name,
                            variant: arm.variant,
                            value_literal: arm
                                .value_literal
                                .map(|v| -> Result<Box<Value>, String> {
                                    Ok(Box::new(Value::try_from(*v)?))
                                })
                                .transpose()?,
                            binding: arm.binding,
                            body: Value::try_from(*arm.body)?,
                            src: arm.src.map(Source::try_from).transpose()?,
                        })
                    })
                    .collect();
                Ok(Value::Match(MatchExpr {
                    value: Box::new(Value::try_from(*match_expr.value)?),
                    arms: arms?,
                    match_all: match_expr.match_all,
                    result_modifier: match_expr.result_modifier.map(|rm| match rm {
                        CacheableResultModifier::One => crate::lang::ast::ResultModifier::One,
                        CacheableResultModifier::Map => crate::lang::ast::ResultModifier::Map,
                        CacheableResultModifier::Vec => crate::lang::ast::ResultModifier::Vec,
                    }),
                    src: match_expr.src.map(Source::try_from).transpose()?,
                }))
            }
            CacheableValue::MatchArm { arm } => {
                use crate::lang::ast::{MatchArm, Source};
                Ok(Value::MatchArm(Box::new(MatchArm {
                    type_name: arm.type_name,
                    variant: arm.variant,
                    value_literal: arm
                        .value_literal
                        .map(|v| -> Result<Box<Value>, String> {
                            Ok(Box::new(Value::try_from(*v)?))
                        })
                        .transpose()?,
                    binding: arm.binding,
                    body: Value::try_from(*arm.body)?,
                    src: arm.src.map(Source::try_from).transpose()?,
                })))
            }
            CacheableValue::MapWithSpread {
                base_entries,
                spread_entries,
            } => {
                let base: Result<indexmap::IndexMap<Val, Val>, String> = base_entries
                    .into_iter()
                    .map(|(k, v)| Ok((Val::try_from(k)?, Val::try_from(v)?)))
                    .collect();
                let spreads: Result<Vec<(usize, Value)>, String> = spread_entries
                    .into_iter()
                    .map(|(idx, cv)| Ok((idx, Value::try_from(cv)?)))
                    .collect();
                Ok(Value::MapWithSpread {
                    base_entries: base?,
                    spread_entries: spreads?,
                })
            }
            CacheableValue::Placeholder { index } => Ok(Value::Placeholder(index)),
        }
    }
}

// =============================================================================
// Namespace Serialization
// =============================================================================

/// Cacheable scope entry (Var -> Value pair)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableScopeEntry {
    var: CacheableVar,
    value: CacheableValue,
}

/// Cacheable representation of a Scope
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableScope {
    vars: Vec<CacheableScopeEntry>,
}

impl From<&crate::lang::ast::Scope> for CacheableScope {
    fn from(scope: &crate::lang::ast::Scope) -> Self {
        CacheableScope {
            vars: scope
                .vars
                .iter()
                .map(|(var, value)| CacheableScopeEntry {
                    var: CacheableVar::from(var),
                    value: CacheableValue::from(value),
                })
                .collect(),
        }
    }
}

impl TryFrom<CacheableScope> for crate::lang::ast::Scope {
    type Error = String;

    fn try_from(cacheable: CacheableScope) -> Result<Self, Self::Error> {
        use crate::lang::ast::{Scope, Value, Var};

        let mut vars = IndexMap::new();
        for entry in cacheable.vars {
            let var: Var = Var::try_from(entry.var)?;
            let value: Value = Value::try_from(entry.value)?;
            vars.insert(var, value);
        }
        Ok(Scope { vars })
    }
}

/// Cacheable representation of a Namespace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheableNamespace {
    path: String, // NsPath as string
    scope: CacheableScope,
    meta: Option<CacheableMeta>,
    source_file: Option<String>,
    aliases: serde_json::Value, // NamespaceAliases as JSON (simple enough)
}

impl From<&crate::lang::ast::Namespace> for CacheableNamespace {
    fn from(ns: &crate::lang::ast::Namespace) -> Self {
        // Convert aliases to Vec<(NsPath, NsPath)> for JSON-safe serialization
        // This matches the custom serializer in ast.rs namespace_aliases_serde
        let aliases_vec: Vec<(crate::lang::ast::NsPath, crate::lang::ast::NsPath)> = ns
            .aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        CacheableNamespace {
            path: ns.path.to_string(),
            scope: CacheableScope::from(&ns.scope),
            meta: ns.meta.as_ref().map(CacheableMeta::from),
            source_file: ns
                .source_file
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            aliases: serde_json::to_value(&aliases_vec).unwrap_or(serde_json::json!([])),
        }
    }
}

impl TryFrom<CacheableNamespace> for crate::lang::ast::Namespace {
    type Error = String;

    fn try_from(cacheable: CacheableNamespace) -> Result<Self, Self::Error> {
        use crate::lang::ast::{Meta, Namespace, NamespaceAliases, NsPath, Scope};

        let path = NsPath::from_string(&cacheable.path);
        let scope = Scope::try_from(cacheable.scope)?;
        let meta = cacheable.meta.map(Meta::try_from).transpose()?;
        let source_file = cacheable.source_file.map(std::path::PathBuf::from);

        // Aliases are serialized as Vec<(NsPath, NsPath)> for JSON compatibility
        let aliases: NamespaceAliases = if cacheable.aliases.is_null()
            || cacheable.aliases.as_array().is_some_and(|a| a.is_empty())
        {
            NamespaceAliases::new()
        } else {
            let vec: Vec<(NsPath, NsPath)> = serde_json::from_value(cacheable.aliases)
                .map_err(|e| format!("Failed to deserialize NamespaceAliases: {}", e))?;
            vec.into_iter().collect()
        };

        Ok(Namespace {
            path,
            scope,
            meta,
            source_file,
            aliases,
        })
    }
}

/// Cache entry for parsed namespaces
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Cache format version (increment when format changes)
    pub version: u32,
    /// Namespaces in the parsed file
    pub namespaces: Vec<(String, CacheableNamespace)>, // (NsPath string, Namespace)
}

/// Current cache format version
pub const CACHE_VERSION: u32 = 3; // Bumped for namespace aliases serialization fix

/// Serialize namespaces to cacheable JSON
pub fn serialize_namespaces(
    namespaces: &IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
) -> Result<Vec<u8>, String> {
    let cacheable_namespaces: Vec<(String, CacheableNamespace)> = namespaces
        .iter()
        .map(|(path, ns)| (path.to_string(), CacheableNamespace::from(ns)))
        .collect();

    let entry = CacheEntry {
        version: CACHE_VERSION,
        namespaces: cacheable_namespaces,
    };

    serde_json::to_vec(&entry).map_err(|e| format!("Failed to serialize cache entry: {}", e))
}

/// Deserialize namespaces from cached JSON
pub fn deserialize_namespaces(
    data: &[u8],
) -> Result<IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>, String> {
    let entry: CacheEntry = serde_json::from_slice(data)
        .map_err(|e| format!("Failed to deserialize cache entry: {}", e))?;

    if entry.version != CACHE_VERSION {
        return Err(format!(
            "Cache version mismatch: expected {}, got {}",
            CACHE_VERSION, entry.version
        ));
    }

    let mut result = IndexMap::new();
    for (path_str, cacheable_ns) in entry.namespaces {
        let path = crate::lang::ast::NsPath::from_string(&path_str);
        let ns = crate::lang::ast::Namespace::try_from(cacheable_ns)?;
        result.insert(path, ns);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::*;

    fn assert_val_roundtrip(val: &Val, description: &str) {
        let tagged = TaggedVal::from(val);
        let json = serde_json::to_string(&tagged).unwrap_or_else(|_| {
            panic!(
                "Failed to serialize TaggedVal for {}: {:?}",
                description, val
            )
        });
        let deserialized_tagged: TaggedVal = serde_json::from_str(&json).unwrap_or_else(|_| {
            panic!(
                "Failed to deserialize TaggedVal for {}: {}",
                description, json
            )
        });
        let roundtrip = Val::try_from(deserialized_tagged).unwrap_or_else(|_| {
            panic!(
                "Failed to convert TaggedVal to Val for {}: {}",
                description, json
            )
        });
        assert_eq!(
            *val, roundtrip,
            "Round-trip failed for {}: original={:?}, roundtrip={:?}",
            description, val, roundtrip
        );
    }

    #[test]
    fn test_tagged_val_map_int_keys() {
        // This is the critical test - maps with non-string keys must preserve key types
        let mut map = IndexMap::new();
        map.insert(Val::Int(1), Val::from("one"));
        map.insert(Val::Int(2), Val::from("two"));
        assert_val_roundtrip(&Val::Map(Box::new(map)), "map with int keys");
    }

    #[test]
    fn test_cacheable_value_with_fn_call() {
        // Create a FnCall that contains a Val::Map with int keys
        let mut map = IndexMap::new();
        map.insert(Val::Int(1), Val::from("one"));
        map.insert(Val::Int(2), Val::from("two"));

        let fn_call = FnCall {
            function: Box::new(Value::Ref(Ref::Ns(NsRef {
                ns: NsPath::from_string("test::module"),
                src: None,
                function_name: Some("my-fn".to_string()),
            }))),
            args: vec![FnCallArg {
                value: Value::Val(Val::Map(Box::new(map.clone())), None),
                lazy: false,
                spread: false,
                src: None,
            }],
            result_path: None,
            src: None,
        };

        let value = Value::FnCall(fn_call);
        let cacheable = CacheableValue::from(&value);

        let json = serde_json::to_string_pretty(&cacheable).unwrap();
        let deserialized: CacheableValue = serde_json::from_str(&json).unwrap();
        let roundtrip = Value::try_from(deserialized).unwrap();

        // Verify the nested map preserved its int keys
        if let Value::FnCall(fc) = &roundtrip {
            if let Value::Val(Val::Map(m), _) = &fc.args[0].value {
                assert!(m.contains_key(&Val::Int(1)), "Should have Int(1) key");
                assert!(m.contains_key(&Val::Int(2)), "Should have Int(2) key");
            } else {
                panic!("Expected Val::Map in FnCall arg");
            }
        } else {
            panic!("Expected FnCall");
        }
    }

    #[test]
    fn test_cacheable_namespace_roundtrip() {
        let mut vars = IndexMap::new();

        // Add a variable with a Val::Map with integer keys
        let mut map = IndexMap::new();
        map.insert(Val::Int(1), Val::from("one"));
        map.insert(Val::Int(2), Val::from("two"));

        vars.insert(
            Var {
                sym: Sym::String("lookup".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                src: None,
            },
            Value::Val(Val::Map(Box::new(map)), None),
        );

        let ns = Namespace {
            path: NsPath::from_string("test::module"),
            scope: Scope { vars },
            meta: None,
            source_file: Some(std::path::PathBuf::from("test.hot")),
            aliases: NamespaceAliases::new(),
        };

        let cacheable = CacheableNamespace::from(&ns);
        let json = serde_json::to_string_pretty(&cacheable).unwrap();
        let deserialized: CacheableNamespace = serde_json::from_str(&json).unwrap();
        let roundtrip = Namespace::try_from(deserialized).unwrap();

        // Verify the map with int keys preserved
        let lookup_var = Var {
            sym: Sym::String("lookup".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            src: None,
        };

        let original_val = ns.scope.vars.get(&lookup_var).unwrap();
        let roundtrip_val = roundtrip.scope.vars.get(&lookup_var).unwrap();

        assert_eq!(
            original_val, roundtrip_val,
            "Map with int keys should round-trip correctly"
        );
    }

    #[test]
    fn test_serialize_deserialize_namespaces() {
        let mut namespaces = IndexMap::new();

        let mut vars = IndexMap::new();
        let mut map = IndexMap::new();
        map.insert(Val::Int(42), Val::Bool(true));

        vars.insert(
            Var {
                sym: Sym::String("data".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                src: None,
            },
            Value::Val(Val::Map(Box::new(map)), None),
        );

        namespaces.insert(
            NsPath::from_string("test"),
            Namespace {
                path: NsPath::from_string("test"),
                scope: Scope { vars },
                meta: None,
                source_file: None,
                aliases: NamespaceAliases::new(),
            },
        );

        let serialized = serialize_namespaces(&namespaces).unwrap();
        let deserialized = deserialize_namespaces(&serialized).unwrap();

        assert_eq!(namespaces.len(), deserialized.len());

        // Verify map with int keys
        let path = NsPath::from_string("test");
        let ns = deserialized.get(&path).unwrap();
        let data_var = Var {
            sym: Sym::String("data".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            src: None,
        };

        if let Value::Val(Val::Map(m), _) = ns.scope.vars.get(&data_var).unwrap() {
            assert!(m.contains_key(&Val::Int(42)), "Should have Int(42) key");
        } else {
            panic!("Expected Val::Map");
        }
    }
}
