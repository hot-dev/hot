use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::store::{self, ListOptions, SearchMode, SearchOptions, StoreMapConfig};
use crate::val::Val;
use crate::{validate_args, validate_args_range};
use indexmap::IndexMap;
use serde_json::Value as JsonValue;

fn block_on<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Handle::current().block_on(future)
}

fn call_fn_with_vm(
    vm: &mut VirtualMachine,
    function_val: &Val,
    args: &[Val],
) -> Result<Val, String> {
    if let Val::Map(m) = function_val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
    {
        return call_fn_with_vm(vm, inner, args);
    }
    if let Val::Map(m) = function_val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/FunctionAlias"
        && let Some(Val::Str(target)) = m.get(&Val::from("$target"))
    {
        return call_fn_with_vm(vm, &Val::from(target.clone()), args);
    }
    match function_val {
        Val::Str(name) => {
            if let Some(fid) = vm.find_best_user_function_overload(name, args) {
                return vm
                    .execute_compiled_user_function(fid, args)
                    .map_err(|e| format!("{e:?}"));
            }
            vm.execute_function_call_by_name(name, args)
                .map_err(|e| format!("{e:?}"))
        }
        Val::Box(boxed) => {
            if boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
            {
                vm.execute_lambda(function_val, args)
                    .map_err(|e| format!("{e:?}"))
            } else if let Some(function_ref) = boxed
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>(
            ) {
                let name = function_ref.name();
                if let Some(fid) = vm.find_best_user_function_overload(name, args) {
                    return vm
                        .execute_compiled_user_function(fid, args)
                        .map_err(|e| format!("{e:?}"));
                }
                vm.execute_function_call_by_name(name, args)
                    .map_err(|e| format!("{e:?}"))
            } else {
                Err("Unsupported function type for store callback".to_string())
            }
        }
        _ => Err(format!("Expected a function, got: {function_val}")),
    }
}

fn val_to_json(val: &Val) -> JsonValue {
    JsonValue::from(val)
}

fn json_to_val(jv: JsonValue) -> Val {
    serde_json::from_value::<Val>(jv).unwrap_or(Val::Null)
}

fn get_store(vm: &VirtualMachine) -> Result<std::sync::Arc<dyn store::Store>, Val> {
    vm.get_store()
        .ok_or_else(|| Val::from("Store not configured"))
}

fn get_store_config(vm: &VirtualMachine, map_val: &Val) -> Result<StoreMapConfig, Val> {
    let mut config = store::store_map_config_from_val(map_val).map_err(Val::from)?;
    store::resolve_embedding_model(&mut config, Some(vm.get_conf()));

    if config.embedding_model.is_some()
        && config.embed_fn.is_none()
        && config.embed_batch_fn.is_none()
        && config.embedding_dimensions.is_none()
        && let Some(provider) = vm.get_embedding_provider()
    {
        config.embedding_dimensions = Some(provider.dimensions());
        store::refresh_embedding_conf(&mut config);
    }

    store::validate_provider_config(&config).map_err(Val::from)?;
    Ok(config)
}

fn ensure_store(
    vm: &VirtualMachine,
    map_val: &Val,
) -> Result<(std::sync::Arc<dyn store::Store>, StoreMapConfig), Val> {
    let store = get_store(vm)?;
    let config = get_store_config(vm, map_val)?;
    block_on(store.ensure_store(&config)).map_err(Val::from)?;
    Ok((store, config))
}

fn custom_embedding_is_strict(config: &StoreMapConfig) -> bool {
    (config.embed_fn.is_some() || config.embed_batch_fn.is_some())
        && config.embedding_on_error.as_deref() != Some("skip")
}

fn vector_from_val(value: Val, expected_dimensions: Option<u32>) -> Result<Vec<f32>, String> {
    let Val::Vec(items) = value else {
        return Err(format!(
            "Embedding function must return Vec<Dec | Int>, got {value}"
        ));
    };

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let n = match item {
            Val::Int(i) => i as f32,
            Val::Dec(d) => d.to_string().parse::<f32>().map_err(|e| {
                format!("Embedding vector contains a Dec that cannot be converted to f32: {e}")
            })?,
            Val::Byte(b) => b as f32,
            other => {
                return Err(format!(
                    "Embedding vector values must be Dec or Int, got {other}"
                ));
            }
        };
        out.push(n);
    }

    if let Some(dimensions) = expected_dimensions
        && out.len() != dimensions as usize
    {
        return Err(format!(
            "Embedding vector has {} dimensions, expected {dimensions}",
            out.len()
        ));
    }

    Ok(out)
}

fn batch_vectors_from_val(
    value: Val,
    expected_count: usize,
    expected_dimensions: Option<u32>,
) -> Result<Vec<Vec<f32>>, String> {
    let Val::Vec(items) = value else {
        return Err(format!(
            "Batch embedding function must return Vec<Vec<Dec | Int>>, got {value}"
        ));
    };

    if items.len() != expected_count {
        return Err(format!(
            "Batch embedding function returned {} vectors, expected {expected_count}",
            items.len()
        ));
    }

    items
        .into_iter()
        .map(|item| vector_from_val(item, expected_dimensions))
        .collect()
}

fn handle_embedding_error(
    config: &StoreMapConfig,
    message: String,
) -> Result<Option<Vec<f32>>, Val> {
    if custom_embedding_is_strict(config) {
        Err(Val::from(message))
    } else {
        tracing::warn!("{message}");
        Ok(None)
    }
}

fn embed_text(
    vm: &mut VirtualMachine,
    config: &StoreMapConfig,
    text: &str,
    strict_missing_provider: bool,
) -> Result<Option<Vec<f32>>, Val> {
    if config.embedding_model.is_none() {
        return Ok(None);
    }

    if let Some(embed_fn) = &config.embed_fn {
        if config.embedding_dimensions.is_none() {
            return Err(Val::from(
                "Custom store embedding functions require a positive 'dimensions' value",
            ));
        }

        return match call_fn_with_vm(vm, embed_fn, &[Val::from(text)]) {
            Ok(value) => vector_from_val(value, config.embedding_dimensions)
                .map(Some)
                .map_err(Val::from),
            Err(e) => handle_embedding_error(config, format!("Embedding failed: {e}")),
        };
    }

    if let Some(provider) = vm.get_embedding_provider() {
        match block_on(provider.embed(text)) {
            Ok(emb) => {
                if let Some(dimensions) = config.embedding_dimensions
                    && emb.len() != dimensions as usize
                {
                    return Err(Val::from(format!(
                        "Embedding provider returned {} dimensions, expected {dimensions}",
                        emb.len()
                    )));
                }
                Ok(Some(emb))
            }
            Err(e) => handle_embedding_error(config, format!("Embedding failed: {e}")),
        }
    } else if strict_missing_provider {
        Err(Val::from(
            "Embedding provider not configured but store has embeddings enabled",
        ))
    } else {
        tracing::warn!("Embedding provider not configured; storing value without embedding");
        Ok(None)
    }
}

fn embed_value(
    vm: &mut VirtualMachine,
    config: &StoreMapConfig,
    value: &Val,
) -> Result<Option<Vec<f32>>, Val> {
    let field = config.embedding_field.as_deref().unwrap_or("content");
    let Some(text) = extract_text_for_embedding(value, field) else {
        return Ok(None);
    };

    embed_text(vm, config, &text, false)
}

fn embed_values(
    vm: &mut VirtualMachine,
    config: &StoreMapConfig,
    values: &[&Val],
) -> Result<Vec<Option<Vec<f32>>>, Val> {
    if config.embedding_model.is_none() {
        return Ok(vec![None; values.len()]);
    }

    if let Some(embed_batch_fn) = &config.embed_batch_fn {
        if config.embedding_dimensions.is_none() {
            return Err(Val::from(
                "Custom store batch embedding functions require a positive 'dimensions' value",
            ));
        }

        let field = config.embedding_field.as_deref().unwrap_or("content");
        let mut text_positions = Vec::new();
        let mut texts = Vec::new();
        for (idx, value) in values.iter().enumerate() {
            if let Some(text) = extract_text_for_embedding(value, field) {
                text_positions.push(idx);
                texts.push(Val::from(text));
            }
        }

        if texts.is_empty() {
            return Ok(vec![None; values.len()]);
        }

        let batch_result = call_fn_with_vm(vm, embed_batch_fn, &[Val::Vec(texts)]);
        let vectors = match batch_result {
            Ok(value) => {
                batch_vectors_from_val(value, text_positions.len(), config.embedding_dimensions)
                    .map_err(Val::from)?
            }
            Err(e) => {
                return if custom_embedding_is_strict(config) {
                    Err(Val::from(format!("Batch embedding failed: {e}")))
                } else {
                    tracing::warn!("Batch embedding failed: {e}");
                    Ok(vec![None; values.len()])
                };
            }
        };

        let mut out = vec![None; values.len()];
        for (position, vector) in text_positions.into_iter().zip(vectors) {
            out[position] = Some(vector);
        }
        return Ok(out);
    }

    values
        .iter()
        .map(|value| embed_value(vm, config, value))
        .collect()
}

fn extract_text_for_embedding(value: &Val, field: &str) -> Option<String> {
    match value {
        Val::Str(s) => Some(s.to_string()),
        Val::Map(m) => {
            if let Some(Val::Str(s)) = m.get(&Val::from(field)) {
                Some(s.to_string())
            } else if let Some(Val::Map(inner)) = m.get(&Val::from("$val")) {
                if let Some(Val::Str(s)) = inner.get(&Val::from(field)) {
                    Some(s.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => Some(value.to_string()),
    }
}

fn extract_text_content(value: &Val, config: &StoreMapConfig) -> Option<String> {
    if !config.text_search && config.embedding_model.is_none() {
        return None;
    }
    let field = config.embedding_field.as_deref().unwrap_or("content");
    extract_text_for_embedding(value, field)
}

fn entry_to_val(entry: &store::StoreEntry) -> Val {
    let mut map = IndexMap::new();
    map.insert(Val::from("key"), json_to_val(entry.key.clone()));
    map.insert(Val::from("value"), json_to_val(entry.value.clone()));
    Val::Map(Box::new(map))
}

fn search_result_to_val(result: &store::SearchResult) -> Val {
    let mut map = IndexMap::new();
    map.insert(Val::from("key"), json_to_val(result.entry.key.clone()));
    map.insert(Val::from("value"), json_to_val(result.entry.value.clone()));
    map.insert(
        Val::from("score"),
        Val::Dec(fastnum::D256::from(result.score)),
    );
    Val::Map(Box::new(map))
}

// ---------------------------------------------------------------------------
// Public VmAwareFn implementations
// ---------------------------------------------------------------------------

pub fn put(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/put", args, 3);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let key_json = val_to_json(&args[1]);
    let value_json = val_to_json(&args[2]);
    let embedding = match embed_value(vm, &config, &args[2]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };
    let text_content = extract_text_content(&args[2], &config);

    match block_on(store.put(&config.name, key_json, value_json, embedding, text_content)) {
        Ok(entry) => HotResult::Ok(json_to_val(entry.value)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn get(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::store/get", args, 2, 3);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let key_json = val_to_json(&args[1]);

    match block_on(store.get(&config.name, &key_json)) {
        Ok(Some(entry)) => HotResult::Ok(json_to_val(entry.value)),
        Ok(None) => {
            if args.len() == 3 {
                HotResult::Ok(args[2].clone())
            } else {
                HotResult::Ok(Val::Null)
            }
        }
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn delete(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/delete", args, 2);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let key_json = val_to_json(&args[1]);

    match block_on(store.delete(&config.name, &key_json)) {
        Ok(deleted) => HotResult::Ok(Val::Bool(deleted)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn keys(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/keys", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.keys(&config.name)) {
        Ok(ks) => HotResult::Ok(Val::Vec(ks.into_iter().map(json_to_val).collect())),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn vals(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/vals", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.vals(&config.name)) {
        Ok(vs) => HotResult::Ok(Val::Vec(vs.into_iter().map(json_to_val).collect())),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn length(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/length", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.length(&config.name)) {
        Ok(n) => HotResult::Ok(Val::Int(n as i64)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn is_empty(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/is-empty", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.length(&config.name)) {
        Ok(n) => HotResult::Ok(Val::Bool(n == 0)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn first(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/first", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.first(&config.name)) {
        Ok(Some(entry)) => HotResult::Ok(entry_to_val(&entry)),
        Ok(None) => HotResult::Ok(Val::Null),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn last(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/last", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.last(&config.name)) {
        Ok(Some(entry)) => HotResult::Ok(entry_to_val(&entry)),
        Ok(None) => HotResult::Ok(Val::Null),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn merge(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/merge", args, 2);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let entries_map = match &args[1] {
        Val::Map(m) => m,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::store/merge expects a Map as second argument",
            ));
        }
    };

    let values: Vec<&Val> = entries_map.values().collect();
    let embeddings = match embed_values(vm, &config, &values) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let mut count = 0i64;
    for ((k, v), embedding) in entries_map.iter().zip(embeddings) {
        let key_json = val_to_json(k);
        let value_json = val_to_json(v);
        let text_content = extract_text_content(v, &config);
        match block_on(store.put(&config.name, key_json, value_json, embedding, text_content)) {
            Ok(_) => count += 1,
            Err(e) => return HotResult::Err(Val::from(e)),
        }
    }
    HotResult::Ok(Val::Int(count))
}

pub fn put_many(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/put-many", args, 2);
    merge(vm, args)
}

pub fn list(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::store/list", args, 1, 2);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let opts = if args.len() == 2 {
        let opts_map = &args[1];
        ListOptions {
            limit: opts_map.get("limit").and_then(|v| match v {
                Val::Int(i) => Some(i as usize),
                _ => None,
            }),
            offset: opts_map.get("offset").and_then(|v| match v {
                Val::Int(i) => Some(i as usize),
                _ => None,
            }),
        }
    } else {
        ListOptions::default()
    };

    match block_on(store.list(&config.name, opts)) {
        Ok(entries) => HotResult::Ok(Val::Vec(entries.iter().map(entry_to_val).collect())),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn search(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::store/search", args, 2, 3);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let query = match &args[1] {
        Val::Str(s) => s.to_string(),
        _ => return HotResult::Err(Val::from("::hot::store/search expects a Str query")),
    };

    let (limit, min_score, mode) = if args.len() == 3 {
        let opts = &args[2];
        let limit = opts.get("limit").and_then(|v| match v {
            Val::Int(i) => Some(i as usize),
            _ => None,
        });
        let min_score = opts.get("min-score").and_then(|v| match v {
            Val::Dec(d) => d.to_string().parse::<f64>().ok(),
            Val::Int(i) => Some(i as f64),
            _ => None,
        });
        let mode = opts
            .get("mode")
            .and_then(|v| match v {
                Val::Str(s) => match s.as_ref() {
                    "keyword" => Some(SearchMode::Keyword),
                    "hybrid" => Some(SearchMode::Hybrid),
                    _ => Some(SearchMode::Semantic),
                },
                _ => None,
            })
            .unwrap_or(SearchMode::Semantic);
        (limit, min_score, mode)
    } else {
        (None, None, SearchMode::Semantic)
    };

    let query_embedding = if matches!(mode, SearchMode::Semantic | SearchMode::Hybrid) {
        match embed_text(vm, &config, &query, true) {
            Ok(v) => v,
            Err(e) => return HotResult::Err(e),
        }
    } else {
        None
    };

    let options = SearchOptions {
        limit,
        min_score,
        mode,
    };

    match block_on(store.search(&config.name, Some(&query), query_embedding, options)) {
        Ok(results) => HotResult::Ok(Val::Vec(results.iter().map(search_result_to_val).collect())),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn filter(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/filter", args, 2);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let predicate = args[1].clone();

    let entries = match block_on(store.list(&config.name, ListOptions::default())) {
        Ok(e) => e,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    let mut results = Vec::new();
    for entry in &entries {
        let k = json_to_val(entry.key.clone());
        let v = json_to_val(entry.value.clone());
        let result = call_fn_with_vm(vm, &predicate, &[k.clone(), v.clone()]);
        match result {
            Ok(Val::Bool(true)) => results.push(entry_to_val(entry)),
            Ok(_) => {}
            Err(e) => return HotResult::Err(Val::from(e)),
        }
    }
    HotResult::Ok(Val::Vec(results))
}

pub fn find_first(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/find-first", args, 2);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let predicate = args[1].clone();

    let entries = match block_on(store.list(&config.name, ListOptions::default())) {
        Ok(e) => e,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    for entry in &entries {
        let k = json_to_val(entry.key.clone());
        let v = json_to_val(entry.value.clone());
        let result = call_fn_with_vm(vm, &predicate, &[k, v]);
        match result {
            Ok(Val::Bool(true)) => return HotResult::Ok(entry_to_val(entry)),
            Ok(_) => {}
            Err(e) => return HotResult::Err(Val::from(e)),
        }
    }
    HotResult::Ok(Val::Null)
}

pub fn some(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/some", args, 2);
    match find_first(vm, args) {
        HotResult::Ok(Val::Null) => HotResult::Ok(Val::Bool(false)),
        HotResult::Ok(_) => HotResult::Ok(Val::Bool(true)),
        HotResult::Err(e) => HotResult::Err(e),
    }
}

pub fn all(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/all", args, 2);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let predicate = args[1].clone();

    let entries = match block_on(store.list(&config.name, ListOptions::default())) {
        Ok(e) => e,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    for entry in &entries {
        let k = json_to_val(entry.key.clone());
        let v = json_to_val(entry.value.clone());
        let result = call_fn_with_vm(vm, &predicate, &[k, v]);
        match result {
            Ok(Val::Bool(true)) => {}
            Ok(_) => return HotResult::Ok(Val::Bool(false)),
            Err(e) => return HotResult::Err(Val::from(e)),
        }
    }
    HotResult::Ok(Val::Bool(true))
}

pub fn reduce(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/reduce", args, 3);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let reducer = args[1].clone();
    let mut acc = args[2].clone();

    let entries = match block_on(store.list(&config.name, ListOptions::default())) {
        Ok(e) => e,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    for entry in &entries {
        let k = json_to_val(entry.key.clone());
        let v = json_to_val(entry.value.clone());
        match call_fn_with_vm(vm, &reducer, &[acc, k, v]) {
            Ok(new_acc) => acc = new_acc,
            Err(e) => return HotResult::Err(Val::from(e)),
        }
    }
    HotResult::Ok(acc)
}

pub fn slice(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::store/slice", args, 2, 3);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    let start = match &args[1] {
        Val::Int(i) => *i as usize,
        _ => return HotResult::Err(Val::from("::hot::store/slice expects Int for start")),
    };

    let end = if args.len() == 3 {
        match &args[2] {
            Val::Int(i) => Some(*i as usize),
            _ => return HotResult::Err(Val::from("::hot::store/slice expects Int for end")),
        }
    } else {
        None
    };

    let limit = end.map(|e| e.saturating_sub(start));
    let opts = ListOptions {
        limit,
        offset: Some(start),
    };

    match block_on(store.list(&config.name, opts)) {
        Ok(entries) => HotResult::Ok(Val::Vec(entries.iter().map(entry_to_val).collect())),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn clear(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/clear", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.clear(&config.name)) {
        Ok(()) => HotResult::Ok(Val::Bool(true)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

pub fn destroy(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::store/destroy", args, 1);
    let (store, config) = match ensure_store(vm, &args[0]) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match block_on(store.destroy(&config.name)) {
        Ok(()) => HotResult::Ok(Val::Bool(true)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_from_val_accepts_numeric_vec() {
        let vector = vector_from_val(Val::Vec(vec![Val::Int(1), Val::Int(2)]), Some(2)).unwrap();
        assert_eq!(vector, vec![1.0, 2.0]);
    }

    #[test]
    fn vector_from_val_rejects_dimension_mismatch() {
        let err = vector_from_val(Val::Vec(vec![Val::Int(1)]), Some(2)).unwrap_err();
        assert!(err.contains("expected 2"));
    }

    #[test]
    fn batch_vectors_from_val_validates_count() {
        let batch = Val::Vec(vec![Val::Vec(vec![Val::Int(1), Val::Int(2)])]);
        let err = batch_vectors_from_val(batch, 2, Some(2)).unwrap_err();
        assert!(err.contains("expected 2"));
    }
}
