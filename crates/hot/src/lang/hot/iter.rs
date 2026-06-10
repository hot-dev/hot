//! Iterator module - lazy iteration over sequences
//!
//! This module provides iterator support for Hot, enabling lazy evaluation
//! and streaming of data from sources like HTTP responses.

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::stream::StreamPublisher;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;
use std::any::Any;
use std::hash::Hasher;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

/// Trait for Hot iterators - provides a way to get the next value
pub trait HotIterator: Any + Send + Sync + std::fmt::Debug {
    /// Get the next value from the iterator
    /// Returns (value, done) where done is true when the iterator is exhausted
    fn next(&mut self) -> Result<(Val, bool), String>;

    /// Get the data type for stream emissions (e.g., "http:sse:chunk")
    fn stream_data_type(&self) -> Option<&str> {
        None
    }

    /// Check if this iterator should emit stream:data events
    fn should_emit_stream_data(&self) -> bool {
        false
    }
}

/// Wrapper to hold a HotIterator in Val::Box
#[derive(Debug)]
pub struct IteratorBox {
    pub inner: Arc<Mutex<Box<dyn HotIterator>>>,
    /// Optional: emit stream:data for each yielded value
    pub emit_stream_data: bool,
    /// Optional: data type for stream:data emissions
    pub stream_data_type: String,
}

impl IteratorBox {
    pub fn new(iter: Box<dyn HotIterator>) -> Self {
        let emit = iter.should_emit_stream_data();
        let data_type = iter.stream_data_type().unwrap_or("iter:value").to_string();
        Self {
            inner: Arc::new(Mutex::new(iter)),
            emit_stream_data: emit,
            stream_data_type: data_type,
        }
    }

    pub fn with_stream_data(mut self, emit: bool, data_type: &str) -> Self {
        self.emit_stream_data = emit;
        self.stream_data_type = data_type.to_string();
        self
    }
}

// Implement crate::val::ValBox for IteratorBox so it can be stored in Val::Box
impl crate::val::ValBox for IteratorBox {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn clone_box(&self) -> Box<dyn crate::val::ValBox> {
        Box::new(IteratorBox {
            inner: Arc::clone(&self.inner),
            emit_stream_data: self.emit_stream_data,
            stream_data_type: self.stream_data_type.clone(),
        })
    }

    fn equals(&self, other: &dyn crate::val::ValBox) -> bool {
        // Iterators are only equal if they point to the same underlying iterator
        if let Some(other_iter) = other.as_any().downcast_ref::<IteratorBox>() {
            Arc::ptr_eq(&self.inner, &other_iter.inner)
        } else {
            false
        }
    }

    fn hash(&self, _state: &mut dyn Hasher) {
        // Iterators don't support meaningful hashing
        // The ValBox trait requires this but we can't implement it properly for iterators
    }

    fn to_string(&self) -> String {
        format!("Iterator<{}>", self.stream_data_type)
    }

    fn compare(&self, _other: &dyn crate::val::ValBox) -> Option<std::cmp::Ordering> {
        // Iterators cannot be compared
        None
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        // Serialize as a special marker since iterators can't be fully serialized
        Ok(serde_json::json!({
            "$type": "Iterator",
            "stream_data_type": self.stream_data_type
        }))
    }

    fn type_name(&self) -> &'static str {
        "Iterator"
    }
}

/// Get the next value from an iterator
///
/// # Arguments
/// * `iter` - An iterator (Val::Box containing IteratorBox)
///
/// # Returns
/// * A map with `{value: Any, done: Bool}` or error
pub fn next(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::iter/next", args, 1);

    let iter_val = &args[0];

    // Extract the IteratorBox from Val::Box
    let iter_box = match iter_val {
        Val::Box(boxed) => {
            if let Some(iter) = boxed.as_any().downcast_ref::<IteratorBox>() {
                iter
            } else {
                return HotResult::Err(err_val(
                    "::hot::iter/next: argument is not an Iterator".to_string(),
                ));
            }
        }
        _ => {
            return HotResult::Err(err_val(
                "::hot::iter/next: argument must be an Iterator".to_string(),
            ));
        }
    };

    // Lock and get next value
    let (value, done) = {
        let mut guard = match iter_box.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                return HotResult::Err(err_val(format!(
                    "::hot::iter/next: failed to lock iterator: {}",
                    e
                )));
            }
        };
        match guard.next() {
            Ok(result) => result,
            Err(e) => {
                return HotResult::Err(err_val(format!("::hot::iter/next: {}", e)));
            }
        }
    };

    // Optionally emit stream:data (skip null values - they're just "no data yet" signals)
    if iter_box.emit_stream_data
        && !done
        && !matches!(value, Val::Null)
        && let Some(publisher) = vm.get_stream_publisher()
        && let Some(ctx) = vm.get_execution_context()
    {
        let stream_data_id = Uuid::now_v7();
        let payload_json: serde_json::Value = (&value).into();

        let stream_event = crate::stream::StreamEvent::StreamData {
            stream_data_id,
            run_id: ctx.run_id,
            env_id: ctx.env_id,
            stream_id: ctx.stream_id,
            data_type: iter_box.stream_data_type.clone(),
            payload: payload_json.clone(),
        };

        // Publish to stream pub/sub (fire-and-forget)
        // Use Handle::block_on directly since VM runs in spawn_blocking context
        let publisher_clone = publisher.clone();
        tokio::runtime::Handle::current().block_on(async {
            if let Err(e) = publisher_clone.publish(stream_event).await {
                tracing::warn!("Failed to publish iterator stream data: {}", e);
            }
        });

        // Also queue for DB persistence via emitter
        if let Some(emitter) = vm.get_emitter() {
            let payload_val: Val = serde_json::from_value(payload_json).unwrap_or(Val::Null);
            let event = crate::lang::emitter::EngineEvent::new(
                ctx.clone(),
                "stream:data".to_string(),
                crate::val!({
                    "stream_data_id": stream_data_id.to_string(),
                    "data_type": iter_box.stream_data_type.clone(),
                    "payload": payload_val,
                    "env_id": ctx.env_id.map(|id| id.to_string()).unwrap_or_default(),
                }),
            );
            emitter.emit(event);
        }
    }

    // Build typed Next result
    let mut result_map: IndexMap<Val, Val> = IndexMap::new();
    result_map.insert(Val::from("$type"), Val::from("::hot::iter/Next"));
    result_map.insert(Val::from("value"), value);
    result_map.insert(Val::from("done"), Val::Bool(done));

    HotResult::Ok(Val::Map(Box::new(result_map)))
}

/// Collect all values from an iterator into a vector
///
/// # Arguments
/// * `iter` - An iterator (Val::Box containing IteratorBox)
///
/// # Returns
/// * A vector of all values, or error
pub fn collect(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::iter/collect", args, 1);

    let mut results = Vec::new();

    loop {
        let next_result = match next(vm, args) {
            HotResult::Ok(val) => val,
            HotResult::Err(e) => return HotResult::Err(e),
        };

        if let Val::Map(map) = next_result {
            let done = map
                .get(&Val::from("done"))
                .map(|v| matches!(v, Val::Bool(true)))
                .unwrap_or(false);

            if done {
                break;
            }

            if let Some(value) = map.get(&Val::from("value")) {
                results.push(value.clone());
            }
        }
    }

    HotResult::Ok(Val::Vec(results))
}

/// Check if a value is an iterator
pub fn is_iterator(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::iter/is-iterator", args, 1);

    let is_iter = match &args[0] {
        Val::Box(boxed) => boxed.as_any().downcast_ref::<IteratorBox>().is_some(),
        _ => false,
    };

    HotResult::Ok(Val::Bool(is_iter))
}

/// Iterator over a Hot vector
#[derive(Debug)]
pub struct VecIterator {
    items: Vec<Val>,
    index: usize,
}

impl VecIterator {
    pub fn new(items: Vec<Val>) -> Self {
        Self { items, index: 0 }
    }
}

impl HotIterator for VecIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if self.index >= self.items.len() {
            Ok((Val::Null, true))
        } else {
            let val = self.items[self.index].clone();
            self.index += 1;
            Ok((val, false))
        }
    }
}

/// Iterator over a Hot map - yields [key, value] pairs
#[derive(Debug)]
pub struct MapIterator {
    pairs: Vec<(Val, Val)>,
    index: usize,
}

impl MapIterator {
    pub fn new(map: &IndexMap<Val, Val>) -> Self {
        let pairs: Vec<(Val, Val)> = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        Self { pairs, index: 0 }
    }
}

impl HotIterator for MapIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if self.index >= self.pairs.len() {
            Ok((Val::Null, true))
        } else {
            let (key, value) = self.pairs[self.index].clone();
            self.index += 1;
            Ok((Val::Vec(vec![key, value]), false))
        }
    }
}

/// Iterator over map keys only
#[derive(Debug)]
pub struct MapKeysIterator {
    keys: Vec<Val>,
    index: usize,
}

impl MapKeysIterator {
    pub fn new(map: &IndexMap<Val, Val>) -> Self {
        let keys: Vec<Val> = map.keys().cloned().collect();
        Self { keys, index: 0 }
    }
}

impl HotIterator for MapKeysIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if self.index >= self.keys.len() {
            Ok((Val::Null, true))
        } else {
            let key = self.keys[self.index].clone();
            self.index += 1;
            Ok((key, false))
        }
    }
}

/// Iterator over map values only
#[derive(Debug)]
pub struct MapValuesIterator {
    values: Vec<Val>,
    index: usize,
}

impl MapValuesIterator {
    pub fn new(map: &IndexMap<Val, Val>) -> Self {
        let values: Vec<Val> = map.values().cloned().collect();
        Self { values, index: 0 }
    }
}

impl HotIterator for MapValuesIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if self.index >= self.values.len() {
            Ok((Val::Null, true))
        } else {
            let val = self.values[self.index].clone();
            self.index += 1;
            Ok((val, false))
        }
    }
}

/// Iterator over a string - yields each character as a string
#[derive(Debug)]
pub struct StrIterator {
    chars: Vec<String>,
    index: usize,
}

impl StrIterator {
    pub fn new(s: &str) -> Self {
        let chars: Vec<String> = s.chars().map(|c| c.to_string()).collect();
        Self { chars, index: 0 }
    }
}

impl HotIterator for StrIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if self.index >= self.chars.len() {
            Ok((Val::Null, true))
        } else {
            let ch = self.chars[self.index].clone();
            self.index += 1;
            Ok((Val::from(ch), false))
        }
    }
}

/// Polymorphic iter() - create an iterator from any iterable type
pub fn iter(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::iter/Iter", args, 1);

    let iter_box = match &args[0] {
        Val::Vec(v) => {
            let iterator = VecIterator::new(v.clone());
            IteratorBox::new(Box::new(iterator))
        }
        Val::Map(m) => {
            let iterator = MapIterator::new(m);
            IteratorBox::new(Box::new(iterator))
        }
        Val::Str(s) => {
            let iterator = StrIterator::new(s);
            IteratorBox::new(Box::new(iterator))
        }
        Val::Box(boxed) => {
            // If it's already an iterator, return it as-is
            if boxed.as_any().downcast_ref::<IteratorBox>().is_some() {
                return HotResult::Ok(args[0].clone());
            }
            return HotResult::Err(err_val(
                "::hot::iter/Iter: argument must be a Vec, Map, Str, or Iterator".to_string(),
            ));
        }
        other => {
            let type_name = match other {
                Val::Int(_) => "Int",
                Val::Dec(_) => "Dec",
                Val::Bool(_) => "Bool",
                Val::Null => "Null",
                Val::Byte(_) => "Byte",
                Val::Bytes(_) => "Bytes",
                _ => "unknown",
            };
            return HotResult::Err(err_val(format!(
                "::hot::iter/Iter: cannot iterate over {}",
                type_name
            )));
        }
    };

    HotResult::Ok(Val::Box(Box::new(iter_box)))
}

/// Create an iterator over map keys
pub fn keys(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::iter/keys", args, 1);

    let map = match &args[0] {
        Val::Map(m) => m,
        _ => {
            return HotResult::Err(err_val(
                "::hot::iter/keys: argument must be a map".to_string(),
            ));
        }
    };

    let iterator = MapKeysIterator::new(map);
    let iter_box = IteratorBox::new(Box::new(iterator));

    HotResult::Ok(Val::Box(Box::new(iter_box)))
}

/// Create an iterator over map values
pub fn values(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::iter/values", args, 1);

    let map = match &args[0] {
        Val::Map(m) => m,
        _ => {
            return HotResult::Err(err_val(
                "::hot::iter/values: argument must be a map".to_string(),
            ));
        }
    };

    let iterator = MapValuesIterator::new(map);
    let iter_box = IteratorBox::new(Box::new(iterator));

    HotResult::Ok(Val::Box(Box::new(iter_box)))
}

/// Iterator over a numeric range
#[derive(Debug)]
pub struct RangeIterator {
    current: i64,
    end: i64,
    step: i64,
}

impl RangeIterator {
    pub fn new(start: i64, end: i64, step: i64) -> Self {
        Self {
            current: start,
            end,
            step,
        }
    }
}

impl HotIterator for RangeIterator {
    fn next(&mut self) -> Result<(Val, bool), String> {
        if (self.step > 0 && self.current >= self.end)
            || (self.step < 0 && self.current <= self.end)
        {
            Ok((Val::Null, true))
        } else {
            let val = Val::Int(self.current);
            self.current += self.step;
            Ok((val, false))
        }
    }
}

/// Create a lazy range iterator
pub fn range(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 3 {
        return HotResult::Err(err_val(
            "::hot::iter/range: expected 1 to 3 arguments".to_string(),
        ));
    }

    let (start, end, step) = match args.len() {
        1 => {
            let end = match &args[0] {
                Val::Int(n) => *n,
                _ => {
                    return HotResult::Err(err_val(
                        "::hot::iter/range: end must be an integer".to_string(),
                    ));
                }
            };
            (0, end, 1)
        }
        2 => {
            let start = match &args[0] {
                Val::Int(n) => *n,
                _ => {
                    return HotResult::Err(err_val(
                        "::hot::iter/range: start must be an integer".to_string(),
                    ));
                }
            };
            let end = match &args[1] {
                Val::Int(n) => *n,
                _ => {
                    return HotResult::Err(err_val(
                        "::hot::iter/range: end must be an integer".to_string(),
                    ));
                }
            };
            (start, end, 1)
        }
        3 => {
            let start = match &args[0] {
                Val::Int(n) => *n,
                _ => {
                    return HotResult::Err(err_val(
                        "::hot::iter/range: start must be an integer".to_string(),
                    ));
                }
            };
            let end = match &args[1] {
                Val::Int(n) => *n,
                _ => {
                    return HotResult::Err(err_val(
                        "::hot::iter/range: end must be an integer".to_string(),
                    ));
                }
            };
            let step = match &args[2] {
                Val::Int(n) => *n,
                _ => {
                    return HotResult::Err(err_val(
                        "::hot::iter/range: step must be an integer".to_string(),
                    ));
                }
            };
            if step == 0 {
                return HotResult::Err(err_val(
                    "::hot::iter/range: step cannot be zero".to_string(),
                ));
            }
            (start, end, step)
        }
        _ => unreachable!(),
    };

    let iterator = RangeIterator::new(start, end, step);
    let iter_box = IteratorBox::new(Box::new(iterator));

    HotResult::Ok(Val::Box(Box::new(iter_box)))
}
