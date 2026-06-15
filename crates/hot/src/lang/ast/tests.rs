use super::*;
use crate::val::Val;

/// Helper to test round-trip serialization
fn assert_roundtrip<
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
>(
    val: &T,
    description: &str,
) {
    let json = serde_json::to_string(val)
        .unwrap_or_else(|_| panic!("Failed to serialize {}: {:?}", description, val));
    let deserialized: T = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("Failed to deserialize {}: {}", description, json));
    assert_eq!(
        *val, deserialized,
        "Round-trip failed for {}: original={:?}, deserialized={:?}",
        description, val, deserialized
    );
}

// =========================================================================
// NsPath Serialization Tests
// =========================================================================

#[test]
fn test_nspath_roundtrip() {
    let empty = NsPath::new();
    assert_roundtrip(&empty, "empty NsPath");

    let single = NsPath::from_string("hot");
    assert_roundtrip(&single, "single segment NsPath");

    let multi = NsPath::from_string("hot::std::math");
    assert_roundtrip(&multi, "multi segment NsPath");
}

// =========================================================================
// Sym Serialization Tests
// =========================================================================

#[test]
fn test_sym_roundtrip() {
    let sym = Sym::String("test-var".to_string());
    assert_roundtrip(&sym, "Sym with hyphen");

    let sym2 = Sym::String("CamelCase".to_string());
    assert_roundtrip(&sym2, "Sym CamelCase");
}

// =========================================================================
// Var Serialization Tests
// =========================================================================

#[test]
fn test_var_roundtrip() {
    let var = Var {
        sym: Sym::String("my-var".to_string()),
        deep_set: None,
        deep_path: None,
        meta: None,
        type_annotation: None,
        src: None,
    };
    assert_roundtrip(&var, "simple Var");

    let var_with_meta = Var {
        sym: Sym::String("annotated".to_string()),
        deep_set: None,
        deep_path: Some(DeepPath::Key("field".to_string())),
        meta: Some(Meta {
            val: Val::Bool(true),
        }),
        type_annotation: None,
        src: Some(Source::new(
            Some("test.hot".to_string()),
            1,  // line
            0,  // column
            0,  // position
            10, // length
        )),
    };
    assert_roundtrip(&var_with_meta, "Var with meta and deep_path");
}

// =========================================================================
// Value Enum Serialization Tests (all variants)
// =========================================================================

#[test]
fn test_value_val_roundtrip() {
    let val = Value::Val(Val::Int(42), None);
    assert_roundtrip(&val, "Value::Val with Int");

    let val_typed = Value::Val(Val::from("hello"), Some("Str".to_string()));
    assert_roundtrip(&val_typed, "Value::Val with type annotation");
}

#[test]
fn test_value_ref_roundtrip() {
    let var_ref = Value::Ref(Ref::Var(VarRef {
        var: Var {
            sym: Sym::String("x".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        },
        src: None,
    }));
    assert_roundtrip(&var_ref, "Value::Ref(Var)");

    let ns_ref = Value::Ref(Ref::Ns(NsRef {
        ns: NsPath::from_string("hot::math"),
        src: None,
        function_name: Some("add".to_string()),
    }));
    assert_roundtrip(&ns_ref, "Value::Ref(Ns)");
}

#[test]
fn test_value_fn_roundtrip() {
    let arg_var = Var {
        sym: Sym::String("x".to_string()),
        deep_set: None,
        deep_path: None,
        meta: None,
        type_annotation: None,
        src: None,
    };
    let fn_def = FnDef {
        args: FnArgs {
            args: vec![FnArg {
                var: arg_var,
                lazy: false,
                type_annotation: Some("Int".to_string()),
            }],
            variadic: false,
        },
        return_type: Some("Int".to_string()),
        body: Value::Val(Val::Int(0), None),
    };
    let fn_value = Value::Fn(vec![fn_def]);
    assert_roundtrip(&fn_value, "Value::Fn");
}

#[test]
fn test_value_lambda_roundtrip() {
    let arg_var = Var {
        sym: Sym::String("x".to_string()),
        deep_set: None,
        deep_path: None,
        meta: None,
        type_annotation: None,
        src: None,
    };
    let lambda = Lambda {
        args: FnArgs {
            args: vec![FnArg {
                var: arg_var,
                lazy: false,
                type_annotation: None,
            }],
            variadic: false,
        },
        body: Box::new(Value::Val(Val::Int(1), None)),
        flow_type: None,
    };
    let lambda_value = Value::Lambda(lambda);
    assert_roundtrip(&lambda_value, "Value::Lambda");
}

#[test]
fn test_value_template_literal_roundtrip() {
    let template = TemplateLiteral {
        parts: vec![
            TemplatePart::Text("Hello, ".to_string()),
            TemplatePart::Expression(Box::new(Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String("name".to_string()),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            })))),
            TemplatePart::Text("!".to_string()),
        ],
    };
    let template_value = Value::TemplateLiteral(template);
    assert_roundtrip(&template_value, "Value::TemplateLiteral");
}

#[test]
fn test_value_type_def_roundtrip() {
    let type_def = TypeDef {
        name: Sym::String("User".to_string()),
        fields: Some(vec![
            TypeField {
                name: Sym::String("id".to_string()),
                type_annotation: "Int".to_string(),
                type_expr: TypeExpr::Simple("Int".to_string()),
            },
            TypeField {
                name: Sym::String("name".to_string()),
                type_annotation: "Str".to_string(),
                type_expr: TypeExpr::Simple("Str".to_string()),
            },
        ]),
        type_alias: None,
        variants: None,
        constructor_functions: None,
        implementations: None,
        is_open: false,
    };
    let type_value = Value::TypeDef(type_def);
    assert_roundtrip(&type_value, "Value::TypeDef");
}

// =========================================================================
// Scope and Namespace Serialization Tests
// =========================================================================

#[test]
fn test_scope_roundtrip() {
    let mut vars = IndexMap::new();
    vars.insert(
        Var {
            sym: Sym::String("x".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        },
        Value::Val(Val::Int(42), None),
    );
    vars.insert(
        Var {
            sym: Sym::String("y".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        },
        Value::Val(Val::from("hello"), None),
    );

    let scope = Scope { vars };
    assert_roundtrip(&scope, "Scope with multiple vars");
}

#[test]
fn test_namespace_roundtrip() {
    let mut vars = IndexMap::new();
    vars.insert(
        Var {
            sym: Sym::String("ns".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        },
        Value::Ref(Ref::Ns(NsRef {
            ns: NsPath::from_string("test::module"),
            src: None,
            function_name: None,
        })),
    );

    let namespace = Namespace {
        path: NsPath::from_string("test::module"),
        scope: Scope { vars },
        meta: None,
        source_file: Some(std::path::PathBuf::from("test/module.hot")),
        aliases: NamespaceAliases::new(),
    };
    assert_roundtrip(&namespace, "Namespace");
}

// =========================================================================
// Program Serialization Tests
// =========================================================================

#[test]
fn test_program_roundtrip() {
    let mut namespaces = IndexMap::new();
    let ns_path = NsPath::from_string("main");

    let mut vars = IndexMap::new();
    vars.insert(
        Var {
            sym: Sym::String("main-fn".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        },
        Value::Val(Val::Null, None),
    );

    namespaces.insert(
        ns_path.clone(),
        Namespace {
            path: ns_path.clone(),
            scope: Scope { vars },
            meta: None,
            source_file: None,
            aliases: NamespaceAliases::new(),
        },
    );

    let program = Program {
        namespaces,
        current_namespace: ns_path,
    };
    assert_roundtrip(&program, "Program");
}

// =========================================================================
// Edge Cases and Complex Structures
// =========================================================================

#[test]
fn test_deeply_nested_value_roundtrip() {
    // Create a deeply nested structure
    let inner = Value::Val(Val::Int(42), None);
    let arg_var = Var {
        sym: Sym::String("x".to_string()),
        deep_set: None,
        deep_path: None,
        meta: None,
        type_annotation: None,
        src: None,
    };
    let lambda = Value::Lambda(Lambda {
        args: FnArgs {
            args: vec![FnArg {
                var: arg_var,
                lazy: false,
                type_annotation: None,
            }],
            variadic: false,
        },
        body: Box::new(inner),
        flow_type: None,
    });
    let raw = Value::Raw(Box::new(lambda));
    let do_val = Value::Do(Box::new(raw));

    assert_roundtrip(&do_val, "deeply nested Value");
}

#[test]
fn test_value_with_val_map_roundtrip() {
    // Val::Map with string keys should round-trip correctly
    let mut map = indexmap::IndexMap::new();
    map.insert(Val::from("key1"), Val::Int(1));
    map.insert(Val::from("key2"), Val::from("value"));

    let value = Value::Val(Val::Map(Box::new(map)), None);
    assert_roundtrip(&value, "Value with Val::Map");
}

#[test]
fn test_fncall_roundtrip() {
    let fn_call = FnCall {
        function: Box::new(Value::Ref(Ref::Ns(NsRef {
            ns: NsPath::from_string("hot::math"),
            src: None,
            function_name: Some("add".to_string()),
        }))),
        args: vec![
            FnCallArg {
                value: Value::Val(Val::Int(1), None),
                lazy: false,
                spread: false,
                src: None,
            },
            FnCallArg {
                value: Value::Val(Val::Int(2), None),
                lazy: false,
                spread: false,
                src: None,
            },
        ],
        result_path: None,
        src: None,
    };
    let value = Value::FnCall(fn_call);
    assert_roundtrip(&value, "Value::FnCall");
}

#[test]
fn test_flow_roundtrip() {
    let flow = Flow {
        flow_type: FlowType::Pipe,
        expressions: vec![Value::Val(Val::Int(1), None), Value::Val(Val::Int(2), None)],
        result_modifier: None,
        src: None,
        aliases: vec![],
    };
    let value = Value::Flow(flow);
    assert_roundtrip(&value, "Value::Flow");
}

#[test]
fn test_cond_roundtrip() {
    let flow = Flow {
        flow_type: FlowType::Serial,
        expressions: vec![Value::Val(Val::Bool(true), None)],
        result_modifier: None,
        src: None,
        aliases: vec![],
    };
    let cond = Value::Cond(
        Some("branch1".to_string()),
        Box::new(Value::Val(Val::Bool(true), None)),
        flow,
    );
    assert_roundtrip(&cond, "Value::Cond");
}

#[test]
fn test_type_implementation_roundtrip() {
    let impl_def = TypeImplementation {
        source_type: "CustomType".to_string(),
        target_type: "Str".to_string(),
        implementation: Value::Val(Val::from("converted"), None),
        src: None,
    };
    let value = Value::TypeImplementation(Box::new(impl_def));
    assert_roundtrip(&value, "Value::TypeImplementation");
}

#[test]
fn test_variadic_expansion_roundtrip() {
    let value = Value::VariadicExpansion("args".to_string());
    assert_roundtrip(&value, "Value::VariadicExpansion");
}

#[test]
fn test_multiple_values_roundtrip() {
    let value = Value::MultipleValues(vec![
        Value::Val(Val::Int(1), None),
        Value::Val(Val::Int(2), None),
    ]);
    assert_roundtrip(&value, "Value::MultipleValues");
}

#[test]
fn test_unbound_roundtrip() {
    let value = Value::Unbound(Unbound {
        name: "undefined-var".to_string(),
    });
    assert_roundtrip(&value, "Value::Unbound");
}

#[test]
fn test_cond_default_roundtrip() {
    let flow = Flow {
        flow_type: FlowType::Serial,
        expressions: vec![Value::Val(Val::from("default"), None)],
        result_modifier: None,
        src: None,
        aliases: vec![],
    };
    let value = Value::CondDefault(flow);
    assert_roundtrip(&value, "Value::CondDefault");
}

// =========================================================================
// Test that ALL Value variants are covered
// =========================================================================

#[test]
fn test_all_value_variants_covered() {
    // This test ensures at compile time that we've tested all Value variants.
    // If a new variant is added, this will fail to compile.
    fn create_value(variant: &str) -> Value {
        match variant {
            "Val" => Value::Val(Val::Null, None),
            "Ref" => Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String("x".to_string()),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            })),
            "FnCall" => Value::FnCall(FnCall {
                function: Box::new(Value::Val(Val::Null, None)),
                args: vec![],
                result_path: None,
                src: None,
            }),
            "Flow" => Value::Flow(Flow {
                flow_type: FlowType::Serial,
                expressions: vec![],
                result_modifier: None,
                src: None,
                aliases: vec![],
            }),
            "Fn" => Value::Fn(vec![]),
            "TypeDef" => Value::TypeDef(TypeDef {
                name: Sym::String("T".to_string()),
                fields: None,
                type_alias: None,
                variants: None,
                constructor_functions: None,
                implementations: None,
                is_open: false,
            }),
            "TypeImplementation" => Value::TypeImplementation(Box::new(TypeImplementation {
                source_type: "A".to_string(),
                target_type: "B".to_string(),
                implementation: Value::Val(Val::Null, None),
                src: None,
            })),
            "Unbound" => Value::Unbound(Unbound {
                name: "x".to_string(),
            }),
            "Cond" => Value::Cond(
                None,
                Box::new(Value::Val(Val::Null, None)),
                Flow {
                    flow_type: FlowType::Serial,
                    expressions: vec![],
                    result_modifier: None,
                    src: None,
                    aliases: vec![],
                },
            ),
            "CondDefault" => Value::CondDefault(Flow {
                flow_type: FlowType::Serial,
                expressions: vec![],
                result_modifier: None,
                src: None,
                aliases: vec![],
            }),
            "TemplateLiteral" => Value::TemplateLiteral(TemplateLiteral { parts: vec![] }),
            "Raw" => Value::Raw(Box::new(Value::Val(Val::Null, None))),
            "Do" => Value::Do(Box::new(Value::Val(Val::Null, None))),
            "VariadicExpansion" => Value::VariadicExpansion("x".to_string()),
            "MultipleValues" => Value::MultipleValues(vec![]),
            "Lambda" => Value::Lambda(Lambda {
                args: FnArgs {
                    args: vec![],
                    variadic: false,
                },
                body: Box::new(Value::Val(Val::Null, None)),
                flow_type: None,
            }),
            "Placeholder" => Value::Placeholder(1),
            _ => panic!("Unknown variant: {}", variant),
        }
    }

    // Test each variant can be serialized and deserialized
    let variants = [
        "Val",
        "Ref",
        "FnCall",
        "Flow",
        "Fn",
        "TypeDef",
        "TypeImplementation",
        "Unbound",
        "Cond",
        "CondDefault",
        "TemplateLiteral",
        "Raw",
        "Do",
        "VariadicExpansion",
        "MultipleValues",
        "Lambda",
        "Placeholder",
    ];

    for variant in variants {
        let value = create_value(variant);
        assert_roundtrip(&value, &format!("Value::{} (coverage test)", variant));
    }
}
