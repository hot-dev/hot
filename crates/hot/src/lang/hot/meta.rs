//! Metadata functions for the Hot language implementation
//!
//! These functions provide access to metadata attached to variables and functions

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;

/// Get metadata for a function by name
/// This is a VM-aware function that accesses the namespace registry
pub fn get(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    tracing::debug!("::hot::meta/get: Function called with {} args", args.len());
    validate_args!("::hot::meta/get", args, 1);

    let input = &args[0];
    tracing::debug!("::hot::meta/get: Input type: {:?}", input);

    // Handle lazy lambdas (from lazy parameters)
    // For the `meta` function, we need special handling because we want to inspect the
    // unevaluated expression, not its value. With lambda-based lazy evaluation, we can
    // extract the variable name from the lambda's bytecode instructions.
    let mut evaluated_lambda: Val = Val::Null;
    let actual_input = match input {
        Val::Box(boxed) => {
            // Check if this is a lambda (zero-argument lambda for lazy evaluation)
            if let Some(lambda_info) = boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    // This is a lazy thunk - inspect its bytecode to extract the variable reference
                    tracing::debug!(
                        "::hot::meta/get: Found lazy lambda, inspecting bytecode: {:?}",
                        lambda_info.instructions
                    );

                    // Try to extract variable name from LoadVar instruction
                    for instruction in &lambda_info.instructions {
                        if let crate::lang::bytecode::Instruction::LoadVar { var_name, .. } =
                            instruction
                        {
                            // Found a LoadVar - var_name is a ConstantId, need to look it up
                            if let Some(crate::lang::bytecode::Constant::Val(Val::Str(name_str))) =
                                vm.program.constants.get(*var_name as usize)
                            {
                                tracing::debug!(
                                    "::hot::meta/get: Extracted variable name from lazy lambda: {}",
                                    name_str
                                );
                                evaluated_lambda = Val::from(name_str.clone());
                                break;
                            }
                        }
                    }

                    // Check if we found a variable name
                    if let Val::Str(_) = evaluated_lambda {
                        &evaluated_lambda
                    } else {
                        // If we couldn't extract a variable name, evaluate the lambda
                        tracing::debug!(
                            "::hot::meta/get: Could not extract variable from bytecode, evaluating lambda"
                        );
                        match vm.execute_lambda(input, &[]) {
                            Ok(result) => {
                                tracing::debug!(
                                    "::hot::meta/get: Lazy lambda evaluated to: {:?}",
                                    result
                                );
                                evaluated_lambda = result;
                                &evaluated_lambda
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "::hot::meta/get: Could not evaluate lazy lambda: {}",
                                    e
                                );
                                return HotResult::Ok(Val::Null);
                            }
                        }
                    }
                } else {
                    input
                }
            } else {
                input
            }
        }
        _ => input,
    };

    // Accept function name string, function ref, deferred ref, or local variable name
    let function_name_opt: Option<String> = match actual_input {
        Val::Str(name) => Some((**name).to_owned()),
        Val::Box(b) => {
            // Check if it's a FunctionRef
            if let Some(fnref) = b
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                Some(fnref.name.clone())
            }
            // Check if it's a deferred reference (AST node)
            else if let Some(ast_node) = b.as_any().downcast_ref::<crate::lang::ast::AstNode>() {
                // Extract the namespace reference from the deferred AST
                match &ast_node.0 {
                    crate::lang::ast::Value::Ref(crate::lang::ast::Ref::Ns(ns_ref)) => {
                        let ns_parts: Vec<String> = ns_ref
                            .ns
                            .0
                            .iter()
                            .map(|part| match part {
                                crate::lang::ast::NsPathPart::Sym(sym) => sym.name().to_string(),
                            })
                            .collect();

                        if let Some(var_name) = &ns_ref.function_name {
                            Some(format!("::{}/{}", ns_parts.join("::"), var_name))
                        } else {
                            Some(format!("::{}", ns_parts.join("::")))
                        }
                    }
                    _ => None,
                }
            } else {
                tracing::debug!("::hot::meta/get: Box input is not FunctionRef or AstNode");
                None
            }
        }
        _ => {
            tracing::debug!(
                "::hot::meta/get: Input is not Str or Box, type: {:?}",
                std::mem::discriminant(input)
            );
            None
        }
    };

    // If a function or variable name/reference was provided, look up metadata from the Hot AST.
    if let Some(function_name) = function_name_opt {
        tracing::debug!(
            "::hot::meta/get: Looking up metadata for function/variable: '{}'",
            function_name
        );

        // The AST preserves metadata that is not present in runtime values.
        if let Some(hot_ast) = vm.get_hot_ast() {
            // Parse the function name to extract namespace and variable name
            if function_name.starts_with("::")
                && function_name.contains('/')
                && let Some((ns_path_stripped, local_name)) =
                    function_name.trim_start_matches("::").rsplit_once('/')
            {
                // Convert namespace path to NsPath format
                let ns_parts: Vec<crate::lang::ast::NsPathPart> = ns_path_stripped
                    .split("::")
                    .filter(|s| !s.is_empty())
                    .map(|part| {
                        crate::lang::ast::NsPathPart::Sym(crate::lang::ast::Sym::from(part))
                    })
                    .collect();

                let ns_path = crate::lang::ast::NsPath(ns_parts);

                // Look up in Hot AST namespaces
                if let Some(namespace) = hot_ast.namespaces.get(&ns_path) {
                    // Search for the variable in the namespace scope
                    for (var, _value) in namespace.scope.vars.iter() {
                        if var.sym.name() == local_name
                            && let Some(meta) = &var.meta
                        {
                            tracing::debug!(
                                "::hot::meta/get: Found metadata in Hot AST for '{}': {:?}",
                                function_name,
                                meta.val
                            );
                            return HotResult::Ok(meta.val.clone());
                        }
                    }
                }
            }
        }

        // Fallback to namespace registry lookup (original approach)
        let namespace_registry = &vm.program.namespaces;
        for namespace_path in namespace_registry.get_namespace_paths() {
            if let Some(vars) = namespace_registry.get_variables(&namespace_path) {
                for var_info in vars {
                    let local_name = var_info.name.as_str();
                    if local_name == function_name
                        || format!("{}/{}", namespace_path, local_name) == function_name
                    {
                        let metadata = var_info.metadata.clone().unwrap_or(Val::Null);
                        if let Val::Str(json_str) = &metadata
                            && let Ok(parsed_val) = parse_json_to_val(json_str)
                        {
                            return HotResult::Ok(parsed_val);
                        }
                        return HotResult::Ok(metadata);
                    }
                }
            }
        }
        // Not found as a function name; fall through to local var handling
    }

    // Treat input as a local variable name (string only)
    if let Val::Str(var_name) = actual_input {
        // First check for a hidden metadata variable that the compiler emits for locals
        let hidden = format!("__meta__{}", var_name);
        if let Ok(hidden_meta) = vm.lookup_variable(&hidden) {
            return HotResult::Ok(hidden_meta);
        }

        // Check current lexical scope for inline $meta on map values
        if let Ok(val) = vm.lookup_variable(var_name)
            && let Val::Map(m) = &val
            && let Some(meta) = m.get(&Val::from("$meta"))
        {
            return HotResult::Ok(meta.clone());
        }

        // Fallback: return basic shape with name/type if variable exists
        if let Ok(val) = vm.unified_variable_lookup(var_name) {
            let mut map = IndexMap::new();
            map.insert(Val::from("name"), Val::from(var_name.clone()));
            let type_name = match &val {
                Val::Null => "Null",
                Val::Bool(_) => "Bool",
                Val::Int(_) => "Int",
                Val::Dec(_) => "Dec",
                Val::Str(_) => "Str",
                Val::Vec(_) => "Vec",
                Val::Map(_) => "Map",
                Val::Box(_) => "Box",
                Val::Byte(_) => "Byte",
                Val::Bytes(_) => "Bytes",
            };
            map.insert(Val::from("type"), Val::from(type_name.to_string()));
            return HotResult::Ok(Val::Map(Box::new(map)));
        }
    }

    // Support typed Fn input: {"$type": "::hot::type/Fn", "$val": "::ns::path/func"}
    if let Val::Map(m) = input
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(Val::Str(func_name)) = m.get(&Val::from("$val"))
    {
        // Recurse as if we were given a string function name
        return get(vm, &[Val::from(func_name.clone())]);
    }

    HotResult::Ok(Val::Null)
}

/// Check if a function is a test function by examining its metadata
/// This is a VM-aware function that bypasses the Hot language is-test implementation
/// to work around variable scoping issues in conditional flows
pub fn is_test(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/is-test", args, 1);

    let function_name_val = &args[0];

    tracing::debug!("VM: is_test called with function: {:?}", function_name_val);

    // Get the metadata for the function
    let metadata_result = match get(vm, args) {
        HotResult::Ok(val) => val,
        HotResult::Err(err) => return HotResult::Err(err),
    };

    tracing::debug!("VM: is_test got metadata: {:?}", metadata_result);

    // Check if the metadata indicates this is a test function
    let is_test_result = match metadata_result {
        // If metadata is a vector, check if it contains "test"
        Val::Vec(items) => {
            for item in items {
                if let Val::Str(s) = item
                    && &*s == "test"
                {
                    return HotResult::Ok(Val::Bool(true));
                }
            }
            false
        }
        // If metadata is a map, check if it has a "test" key that's truthy
        Val::Map(map) => {
            if let Some(test_val) = map.get(&Val::from("test")) {
                match test_val {
                    Val::Bool(b) => *b,
                    Val::Null => false,
                    _ => true, // Any non-null, non-false value is truthy
                }
            } else {
                false
            }
        }
        // If metadata is a string, check if it equals "test"
        Val::Str(s) => &*s == "test",
        // Any other metadata type is not a test
        _ => false,
    };

    tracing::debug!("VM: is_test returning: {}", is_test_result);
    HotResult::Ok(Val::Bool(is_test_result))
}

/// Get source information for a function
pub fn source(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::meta/source", args, 1);

    let function_name_val = &args[0];

    tracing::debug!("VM: source called with function: {:?}", function_name_val);

    // Handle lazy lambdas (from lazy parameters)
    let evaluated_lambda: Val;
    let actual_input = match function_name_val {
        Val::Box(boxed) => {
            // Check if this is a lambda (zero-argument lambda for lazy evaluation)
            if let Some(lambda_info) = boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    // This is a lazy thunk - evaluate it to get the reference
                    tracing::debug!("source: Found lazy lambda, evaluating it");

                    match vm.execute_lambda(function_name_val, &[]) {
                        Ok(result) => {
                            tracing::debug!("source: Lazy lambda evaluated to: {:?}", result);
                            evaluated_lambda = result;
                            &evaluated_lambda
                        }
                        Err(e) => {
                            tracing::warn!("source: Could not evaluate lazy lambda: {}", e);
                            return HotResult::Ok(Val::Null);
                        }
                    }
                } else {
                    function_name_val
                }
            } else {
                function_name_val
            }
        }
        _ => function_name_val,
    };

    // Extract the function name from the argument
    let function_name = match actual_input {
        Val::Str(name) => name.clone(),
        _ => {
            tracing::debug!(
                "VM: source expects a string function name, got: {:?}",
                actual_input
            );
            return HotResult::Ok(Val::Null);
        }
    };

    tracing::debug!(
        "VM: source looking for source info of function: '{}'",
        function_name
    );

    // Search through all namespaces in the namespace registry to find the function
    let namespace_registry = &vm.program.namespaces;

    for namespace_path in namespace_registry.get_namespace_paths() {
        let functions = namespace_registry.get_functions_in_namespace(&namespace_path);
        for var_info in functions {
            // Extract the local function name (without namespace prefix)
            let local_name = if let Some(last_part) = var_info.name.split('/').next_back() {
                last_part
            } else {
                &var_info.name
            };

            if local_name == &*function_name || var_info.name == *function_name {
                tracing::debug!(
                    "VM: source found function '{}' in namespace '{:?}'",
                    var_info.name,
                    namespace_path
                );

                // Look up source information from the program's functions
                let function_info = vm.program.functions.iter().find(|f| {
                    f.name == var_info.name || f.name.ends_with(&format!("/{}", local_name))
                });

                if let Some(function_info) = function_info {
                    let mut source_map = indexmap::IndexMap::new();

                    // Add the type metadata to indicate this is a Source type
                    source_map.insert(Val::from("$type"), Val::from("::hot::meta/Source"));

                    // Try to get file information from the source map
                    // For now, we'll derive it from the namespace or use a default
                    let file_path = if let Some(file_content) =
                        vm.program.source_map.file_contents.keys().next()
                    {
                        file_content.clone()
                    } else {
                        // Derive filename from namespace if possible
                        let namespace_parts: Vec<&str> =
                            function_info.namespace.split("::").collect();
                        if namespace_parts.len() > 1 {
                            format!("{}.hot", namespace_parts[1..].join("/"))
                        } else {
                            "unknown.hot".to_string()
                        }
                    };

                    source_map.insert(Val::from("file"), Val::from(file_path.clone()));

                    // For now, use line 1 as default - in a full implementation,
                    // we would track actual line numbers during parsing
                    source_map.insert(Val::from("line"), Val::Int(1));
                    source_map.insert(Val::from("column"), Val::Int(1));
                    source_map.insert(Val::from("position"), Val::Int(0));

                    tracing::debug!(
                        "VM: source returning source info for '{}': file={}, line=1",
                        var_info.name,
                        file_path
                    );

                    return HotResult::Ok(Val::Map(Box::new(source_map)));
                }
            }
        }
    }

    tracing::debug!(
        "VM: source could not find function '{}', returning null source info",
        function_name
    );

    // Return null source info if function not found
    let mut source_map = indexmap::IndexMap::new();

    // Add the type metadata to indicate this is a Source type
    source_map.insert(Val::from("$type"), Val::from("::hot::meta/Source"));

    source_map.insert(Val::from("file"), Val::Null);
    source_map.insert(Val::from("line"), Val::Null);
    source_map.insert(Val::from("column"), Val::Null);
    source_map.insert(Val::from("position"), Val::Null);

    HotResult::Ok(Val::Map(Box::new(source_map)))
}

/// Parse a JSON string into a Val
/// This is a simple JSON parser for metadata strings
fn parse_json_to_val(json_str: &str) -> Result<Val, String> {
    let json_str = json_str.trim();

    if json_str.starts_with('[') && json_str.ends_with(']') {
        // Parse as array
        let inner = &json_str[1..json_str.len() - 1].trim();
        if inner.is_empty() {
            return Ok(Val::Vec(vec![]));
        }

        // Simple array parsing - split by comma and parse each element
        let mut elements = Vec::new();
        for element in inner.split(',') {
            let element = element.trim();
            if element.starts_with('"') && element.ends_with('"') {
                // String element
                let string_content = &element[1..element.len() - 1];
                elements.push(Val::from(string_content.to_string()));
            } else if element == "true" {
                elements.push(Val::Bool(true));
            } else if element == "false" {
                elements.push(Val::Bool(false));
            } else if element == "null" {
                elements.push(Val::Null);
            } else if let Ok(num) = element.parse::<i64>() {
                elements.push(Val::Int(num));
            } else {
                // Fallback to string
                elements.push(Val::from(element.to_string()));
            }
        }
        Ok(Val::Vec(elements))
    } else if json_str.starts_with('{') && json_str.ends_with('}') {
        // Parse as object
        let inner = &json_str[1..json_str.len() - 1].trim();
        if inner.is_empty() {
            return Ok(Val::map_empty());
        }

        // Simple object parsing - this is a basic implementation
        let mut map = IndexMap::new();

        // Split by comma, but be careful about nested structures
        let pairs: Vec<&str> = inner.split(',').collect();
        for pair in pairs {
            let pair = pair.trim();
            if let Some(colon_pos) = pair.find(':') {
                let key_part = pair[..colon_pos].trim();
                let value_part = pair[colon_pos + 1..].trim();

                // Parse key (should be a string)
                let key = if key_part.starts_with('"') && key_part.ends_with('"') {
                    Val::from(key_part[1..key_part.len() - 1].to_string())
                } else {
                    Val::from(key_part.to_string())
                };

                // Parse value
                let value = if value_part.starts_with('"') && value_part.ends_with('"') {
                    Val::from(value_part[1..value_part.len() - 1].to_string())
                } else if value_part == "true" {
                    Val::Bool(true)
                } else if value_part == "false" {
                    Val::Bool(false)
                } else if value_part == "null" {
                    Val::Null
                } else if let Ok(num) = value_part.parse::<i64>() {
                    Val::Int(num)
                } else {
                    Val::from(value_part.to_string())
                };

                map.insert(key, value);
            }
        }
        Ok(Val::Map(Box::new(map)))
    } else if json_str.starts_with('"') && json_str.ends_with('"') {
        // String
        Ok(Val::from(json_str[1..json_str.len() - 1].to_string()))
    } else if json_str == "true" {
        Ok(Val::Bool(true))
    } else if json_str == "false" {
        Ok(Val::Bool(false))
    } else if json_str == "null" {
        Ok(Val::Null)
    } else if let Ok(num) = json_str.parse::<i64>() {
        Ok(Val::Int(num))
    } else {
        Err(format!("Cannot parse JSON: {}", json_str))
    }
}
