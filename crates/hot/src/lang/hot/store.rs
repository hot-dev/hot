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

fn maybe_embed(vm: &VirtualMachine, config: &StoreMapConfig, value: &Val) -> Option<Vec<f32>> {
    config.embedding_model.as_ref()?;
    let provider = vm.get_embedding_provider()?;

    let field = config.embedding_field.as_deref().unwrap_or("content");
    let text = extract_text_for_embedding(value, field)?;

    match block_on(provider.embed(&text)) {
        Ok(emb) => Some(emb),
        Err(e) => {
            tracing::warn!("Embedding failed: {e}");
            None
        }
    }
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
    let embedding = maybe_embed(vm, &config, &args[2]);
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

    let mut count = 0i64;
    for (k, v) in entries_map.iter() {
        let key_json = val_to_json(k);
        let value_json = val_to_json(v);
        let embedding = maybe_embed(vm, &config, v);
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
        if let Some(provider) = vm.get_embedding_provider() {
            match block_on(provider.embed(&query)) {
                Ok(emb) => Some(emb),
                Err(e) => return HotResult::Err(Val::from(format!("Embedding query failed: {e}"))),
            }
        } else if config.embedding_model.is_some() {
            return HotResult::Err(Val::from(
                "Embedding provider not configured but store has embeddings enabled",
            ));
        } else {
            None
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
