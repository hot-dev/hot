// Metadata Formatter - Converts enriched metadata to readable Hot code syntax
//
// This module takes the enriched metadata from the emitter (which includes
// function signatures, type definitions, etc.) and formats it as Hot language
// syntax for display in the UI and LSP.

use crate::val::Val;

/// Format enriched metadata as Hot code syntax
///
/// Takes the enriched value (with runtime_value and metadata) and returns
/// a human-readable Hot syntax representation.
///
/// # Examples
///
/// For a function:
/// ```hot
/// greet fn (name: String) -> String
/// ```
///
/// For a type:
/// ```hot
/// Person type {
///   name: String,
///   age: Int
/// }
/// ```
pub fn format_as_hot_code(enriched_value: &Val) -> Option<String> {
    // Check if this is an enriched value with metadata
    let metadata = match enriched_value {
        Val::Map(m) => m.get(&Val::from("metadata"))?,
        _ => return None,
    };

    // Extract metadata fields
    let value_type = extract_string(metadata, "value_type")?;

    match value_type.as_str() {
        "function" => format_function(metadata),
        "type" => format_type(metadata),
        "literal" => format_literal(metadata),
        "var_ref" => format_var_ref(metadata),
        _ => None,
    }
}

/// Format a function signature in Hot syntax
fn format_function(metadata: &Val) -> Option<String> {
    let signatures = match metadata {
        Val::Map(m) => m.get(&Val::from("signatures"))?,
        _ => return None,
    };

    let signatures_vec = match signatures {
        Val::Vec(v) => v,
        _ => return None,
    };

    if signatures_vec.is_empty() {
        return Some("fn ()".to_string());
    }

    // Format all signatures (for overloaded functions)
    let mut formatted_sigs: Vec<String> = Vec::new();

    for sig in signatures_vec.iter() {
        if let Some(sig_str) = format_single_signature(sig) {
            // Extract arity for display
            let arity = if let Val::Map(sig_map) = sig {
                sig_map.get(&Val::from("arity")).and_then(|v| match v {
                    Val::Int(n) => Some(*n),
                    _ => None,
                })
            } else {
                None
            };

            if signatures_vec.len() > 1 {
                // For multiple overloads, show arity comment (no fn prefix - matches source syntax)
                if let Some(a) = arity {
                    formatted_sigs.push(format!("{} // arity: {}", sig_str, a));
                } else {
                    formatted_sigs.push(sig_str);
                }
            } else {
                formatted_sigs.push(sig_str);
            }
        }
    }

    if formatted_sigs.is_empty() {
        return Some("fn ()".to_string());
    }

    // For multiple signatures, show "fn" on its own line followed by each signature
    // This matches the source syntax: fn\n(args1) {...},\n(args2) {...}
    if signatures_vec.len() > 1 {
        Some(format!("fn\n{}", formatted_sigs.join("\n")))
    } else {
        Some(format!("fn {}", formatted_sigs[0]))
    }
}

/// Format a single function signature
fn format_single_signature(sig: &Val) -> Option<String> {
    let sig_map = match sig {
        Val::Map(m) => m,
        _ => return None,
    };

    let args = sig_map.get(&Val::from("args"))?;
    let variadic = sig_map
        .get(&Val::from("variadic"))
        .and_then(|v| match v {
            Val::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false);

    let args_vec = match args {
        Val::Vec(v) => v,
        _ => return None,
    };

    // Format arguments - always include type annotations
    let mut formatted_args: Vec<String> = args_vec
        .iter()
        .filter_map(|arg| {
            let arg_map = match arg {
                Val::Map(m) => m,
                _ => return None,
            };

            let name = extract_string_from_map(arg_map, "name")?;

            // Always show type annotation if available, otherwise show 'Any'
            let type_str =
                extract_string_from_map(arg_map, "type").unwrap_or_else(|| "Any".to_string());

            // Clean up Debug format wrappers like Named("TypeName") and Builtin(TypeName)
            let type_str = clean_type_string(&type_str);

            let lazy = extract_bool_from_map(arg_map, "lazy").unwrap_or(false);

            // Format with type annotation
            if lazy {
                Some(format!("lazy {}: {}", name, type_str))
            } else {
                Some(format!("{}: {}", name, type_str))
            }
        })
        .collect();

    // Handle variadic arguments
    if variadic {
        if let Some(last_arg) = formatted_args.last_mut() {
            // Prefix the last argument with ... for variadic functions
            *last_arg = format!("...{}", last_arg);
        } else {
            // No regular args, just variadic
            formatted_args.push("...rest".to_string());
        }
    }

    Some(format!("({})", formatted_args.join(", ")))
}

/// Format a type definition in Hot syntax
fn format_type(metadata: &Val) -> Option<String> {
    let type_name = extract_string(metadata, "type_name")?;

    let mut result = format!("{} type", type_name);

    // Check for constructor signatures
    if let Some(constructors) = get_field(metadata, "constructor_signatures")
        && let Val::Vec(constructors_vec) = constructors
        && !constructors_vec.is_empty()
    {
        // Format as: TypeName type fn\n(args): TypeName,\n(args): TypeName
        result.push_str(" fn\n");

        for (i, constructor) in constructors_vec.iter().enumerate() {
            if let Some(sig) = format_single_signature(constructor) {
                // Format as: (args): ReturnType
                result.push_str(&format!("{}: {}", sig, type_name));
                if i < constructors_vec.len() - 1 {
                    result.push_str(",\n");
                }
            }
        }
    }

    Some(result)
}

/// Format a literal value
fn format_literal(metadata: &Val) -> Option<String> {
    let literal_value = get_field(metadata, "literal_value")?;
    let type_annotation = extract_string(metadata, "type_annotation").unwrap_or("Any".to_string());

    Some(format!("{}: {}", literal_value, type_annotation))
}

/// Format a variable reference
fn format_var_ref(metadata: &Val) -> Option<String> {
    let ref_name = extract_string(metadata, "ref_name")?;
    Some(format!("${}", ref_name))
}

/// Extract a string field from Val::Map
fn extract_string(val: &Val, field: &str) -> Option<String> {
    match val {
        Val::Map(m) => extract_string_from_map(m, field),
        _ => None,
    }
}

/// Extract a string field from an IndexMap
fn extract_string_from_map(map: &indexmap::IndexMap<Val, Val>, field: &str) -> Option<String> {
    match map.get(&Val::from(field.to_string()))? {
        Val::Str(s) => Some((*s).to_string()),
        _ => None,
    }
}

/// Extract a bool field from an IndexMap
fn extract_bool_from_map(map: &indexmap::IndexMap<Val, Val>, field: &str) -> Option<bool> {
    match map.get(&Val::from(field.to_string()))? {
        Val::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Clean up Debug format wrappers from type strings
/// Removes Named("TypeName") -> TypeName and Builtin(TypeName) -> TypeName
fn clean_type_string(type_str: &str) -> String {
    use regex::Regex;

    // Pattern to match Named("...") - this matches the Debug format of TypeExpr::Named
    let named_pattern = Regex::new(r#"Named\("([^"]+)"\)"#).unwrap();

    // Pattern to match Builtin(...) - this matches the Debug format of TypeExpr::Builtin
    let builtin_pattern = Regex::new(r#"Builtin\(([^)]+)\)"#).unwrap();

    // Replace all Named("TypeName") with just TypeName
    let result = named_pattern.replace_all(type_str, "$1");

    // Replace all Builtin(TypeName) with just TypeName
    let result = builtin_pattern.replace_all(&result, "$1");

    result.to_string()
}

/// Get a field from Val::Map
fn get_field<'a>(val: &'a Val, field: &str) -> Option<&'a Val> {
    match val {
        Val::Map(m) => m.get(&Val::from(field.to_string())),
        _ => None,
    }
}

/// Extract documentation from metadata
pub fn extract_documentation(enriched_value: &Val) -> Option<String> {
    let metadata = match enriched_value {
        Val::Map(m) => m.get(&Val::from("metadata"))?,
        _ => return None,
    };

    extract_string(metadata, "doc").map(|s| s.trim().to_string())
}

/// Format metadata as a complete definition with documentation
///
/// This creates a full Hot-style definition including:
/// - Documentation comment
/// - Metadata annotations
/// - Function/type signature
pub fn format_complete_definition(var_name: &str, enriched_value: &Val) -> Option<String> {
    let mut result = String::new();

    // Extract and format documentation
    if let Some(doc) = extract_documentation(enriched_value) {
        // Format as multi-line comment if it contains newlines
        if doc.contains('\n') {
            result.push_str("/*\n");
            for line in doc.lines() {
                result.push_str(&format!(" * {}\n", line));
            }
            result.push_str(" */\n");
        } else {
            result.push_str(&format!("// {}\n", doc));
        }
    }

    // Extract and format metadata annotations
    let metadata = match enriched_value {
        Val::Map(m) => m.get(&Val::from("metadata"))?,
        _ => return None,
    };

    if let Some(meta_val) = get_field(metadata, "meta")
        && !matches!(meta_val, Val::Null)
    {
        // Try to parse and format the meta field
        if let Ok(meta_str) = serde_json::to_string(meta_val) {
            result.push_str(&format!("{} ", meta_str));
        }
    }

    // Add variable name
    result.push('$');
    result.push_str(var_name);
    result.push(' ');

    // Add the signature
    if let Some(signature) = format_as_hot_code(enriched_value) {
        result.push_str(&signature);
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::val;

    #[test]
    fn test_format_simple_function() {
        let enriched = val!({
            "metadata": {
                "value_type": "function",
                "signatures": [
                    {
                        "args": [
                            {
                                "name": "name",
                                "lazy": false,
                                "type": "String"
                            }
                        ],
                        "variadic": false,
                        "arity": 1
                    }
                ]
            }
        });

        let result = format_as_hot_code(&enriched);
        assert_eq!(result, Some("fn (name: String)".to_string()));
    }

    #[test]
    fn test_format_type_with_constructor() {
        let enriched = val!({
            "metadata": {
                "value_type": "type",
                "type_name": "Person",
                "constructor_signatures": [
                    {
                        "args": [
                            {
                                "name": "name",
                                "lazy": false,
                                "type": "String"
                            },
                            {
                                "name": "age",
                                "lazy": false,
                                "type": "Int"
                            }
                        ],
                        "variadic": false,
                        "arity": 2
                    }
                ]
            }
        });

        let result = format_as_hot_code(&enriched);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Person type"));
    }
}
