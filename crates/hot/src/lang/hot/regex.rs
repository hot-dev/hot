use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::limits;
use crate::val::Val;
use crate::validate_args;
use ahash::AHashMap;
use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::{Regex, RegexBuilder};
use std::collections::VecDeque;
use std::sync::LazyLock;

const REGEX_CACHE_LIMIT: usize = 512;

struct RegexCache {
    map: AHashMap<String, Regex>,
    order: VecDeque<String>,
}

impl RegexCache {
    fn new() -> Self {
        Self {
            map: AHashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, pattern: &str) -> Option<Regex> {
        if let Some(regex) = self.map.get(pattern).cloned() {
            self.touch(pattern);
            return Some(regex);
        }
        None
    }

    fn insert(&mut self, pattern: String, regex: Regex) -> Regex {
        self.touch(&pattern);
        self.map.insert(pattern, regex.clone());
        if self.order.len() > REGEX_CACHE_LIMIT
            && let Some(evicted) = self.order.pop_front()
        {
            self.map.remove(&evicted);
        }
        regex
    }

    fn touch(&mut self, pattern: &str) {
        if let Some(pos) = self.order.iter().position(|p| p == pattern) {
            self.order.remove(pos);
        }
        self.order.push_back(pattern.to_string());
    }
}

// parking_lot::Mutex so a panic in a regex consumer can't poison the global
// cache and force every subsequent call to fail.
static REGEX_CACHE: LazyLock<Mutex<RegexCache>> = LazyLock::new(|| Mutex::new(RegexCache::new()));

/// Compile (or fetch from cache) a regex with the configured pattern-length
/// and per-regex compile-memory budgets enforced. Returns a Hot-friendly
/// `Val` error so callers can wrap it directly into `HotResult::Err`.
fn compile_user_regex(op: &str, pattern: &str) -> Result<Regex, Val> {
    limits::check_regex_pattern(op, pattern.len())?;

    {
        let mut cache = REGEX_CACHE.lock();
        if let Some(regex) = cache.get(pattern) {
            return Ok(regex);
        }
    }

    let mut builder = RegexBuilder::new(pattern);
    builder.size_limit(limits::max_regex_compile_bytes());
    builder.dfa_size_limit(limits::max_regex_compile_bytes());
    let compiled = builder
        .build()
        .map_err(|e| Val::from(format!("{}: invalid regex pattern: {}", op, e)))?;

    let mut cache = REGEX_CACHE.lock();
    Ok(cache.insert(pattern.to_string(), compiled))
}

/// `start`/`end` byte offsets fit in `i64` for any string we'll actually see
/// (input is gated to `HOT_MAX_REGEX_INPUT_BYTES` ≪ `i64::MAX`), but we use
/// `try_from` rather than `as i64` so this can't silently wrap if a future
/// caller skips the input gate.
fn pos(n: usize) -> Val {
    Val::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Test if a pattern matches a string (returns boolean)
pub fn is_match(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/is-match", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("is-match", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("is-match", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            HotResult::Ok(Val::Bool(re.is_match(val)))
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("is-match pattern must be a string")),
        _ => HotResult::Err(Val::from("is-match value must be a string")),
    }
}

/// Find the first match of a pattern in a string (returns the match text or null)
/// Note: Rust function is regex_match since `match` is a reserved keyword
pub fn regex_match(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/match", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("match", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("match", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            match re.find(val) {
                Some(m) => HotResult::Ok(Val::from(m.as_str())),
                None => HotResult::Ok(Val::Null),
            }
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("match pattern must be a string")),
        _ => HotResult::Err(Val::from("match value must be a string")),
    }
}

/// Find the first match with position information
pub fn find(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/find", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("find", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("find", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            match re.find(val) {
                Some(m) => {
                    let mut map = IndexMap::new();
                    map.insert(Val::from("match"), Val::from(m.as_str()));
                    map.insert(Val::from("start"), pos(m.start()));
                    map.insert(Val::from("end"), pos(m.end()));
                    HotResult::Ok(Val::Map(Box::new(map)))
                }
                None => HotResult::Ok(Val::Null),
            }
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("find pattern must be a string")),
        _ => HotResult::Err(Val::from("find value must be a string")),
    }
}

/// Find all matches with position information
pub fn find_all(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/find-all", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("find-all", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("find-all", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            // Collect with a soft cap so a tiny pattern over a huge haystack
            // can't produce a result Vec larger than the configured collection
            // size. We fail fast as soon as we'd exceed it.
            let max_matches = limits::max_collection_size();
            let mut matches: Vec<Val> = Vec::new();
            for m in re.find_iter(val) {
                if matches.len() >= max_matches {
                    return HotResult::Err(Val::from(format!(
                        "find-all: too many matches (limit {}). Raise HOT_MAX_COLLECTION_SIZE.",
                        max_matches
                    )));
                }
                let mut map = IndexMap::new();
                map.insert(Val::from("match"), Val::from(m.as_str()));
                map.insert(Val::from("start"), pos(m.start()));
                map.insert(Val::from("end"), pos(m.end()));
                matches.push(Val::Map(Box::new(map)));
            }
            HotResult::Ok(Val::Vec(matches))
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("find-all pattern must be a string")),
        _ => HotResult::Err(Val::from("find-all value must be a string")),
    }
}

/// Replace the first match of a pattern in a string
pub fn replace(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/replace", args, 3);

    match (&args[0], &args[1], &args[2]) {
        (Val::Str(val), Val::Str(pat), Val::Str(rep)) => {
            if let Err(e) = limits::check_regex_input("replace", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("replace", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            HotResult::Ok(Val::from(re.replace(val, &**rep).to_string()))
        }
        (Val::Str(_), Val::Str(_), _) => {
            HotResult::Err(Val::from("replace replacement must be a string"))
        }
        (Val::Str(_), _, _) => HotResult::Err(Val::from("replace pattern must be a string")),
        _ => HotResult::Err(Val::from("replace value must be a string")),
    }
}

/// Replace all matches of a pattern in a string
pub fn replace_all(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/replace-all", args, 3);

    match (&args[0], &args[1], &args[2]) {
        (Val::Str(val), Val::Str(pat), Val::Str(rep)) => {
            if let Err(e) = limits::check_regex_input("replace-all", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("replace-all", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            // The output of `replace_all` can be larger than the input when the
            // replacement is longer than each match. Bound the produced string.
            let out = re.replace_all(val, &**rep);
            if let Err(e) = limits::check_string_bytes("replace-all", out.len()) {
                return HotResult::Err(e);
            }
            HotResult::Ok(Val::from(out.to_string()))
        }
        (Val::Str(_), Val::Str(_), _) => HotResult::Err(Val::from(
            "replace-all replacement must be a string".to_string(),
        )),
        (Val::Str(_), _, _) => HotResult::Err(Val::from("replace-all pattern must be a string")),
        _ => HotResult::Err(Val::from("replace-all value must be a string")),
    }
}

/// Split a string by a regex pattern
pub fn split(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/split", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("split", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("split", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            let max_parts = limits::max_collection_size();
            let mut parts: Vec<Val> = Vec::new();
            for p in re.split(val) {
                if parts.len() >= max_parts {
                    return HotResult::Err(Val::from(format!(
                        "split: too many parts (limit {}). Raise HOT_MAX_COLLECTION_SIZE.",
                        max_parts
                    )));
                }
                parts.push(Val::from(p.to_string()));
            }
            HotResult::Ok(Val::Vec(parts))
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("split pattern must be a string")),
        _ => HotResult::Err(Val::from("split value must be a string")),
    }
}

/// Capture the first match with capture groups
pub fn capture(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/capture", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("capture", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("capture", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            match re.captures(val) {
                Some(caps) => {
                    let mut map = IndexMap::new();
                    if let Some(full) = caps.get(0) {
                        map.insert(Val::from("match"), Val::from(full.as_str()));
                    }
                    let groups: Vec<Val> = caps
                        .iter()
                        .skip(1)
                        .map(|cap| match cap {
                            Some(c) => Val::from(c.as_str()),
                            None => Val::Null,
                        })
                        .collect();
                    map.insert(Val::from("groups"), Val::Vec(groups));
                    HotResult::Ok(Val::Map(Box::new(map)))
                }
                None => HotResult::Ok(Val::Null),
            }
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("capture pattern must be a string")),
        _ => HotResult::Err(Val::from("capture value must be a string")),
    }
}

/// Capture all matches with capture groups
pub fn capture_all(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/capture-all", args, 2);

    match (&args[0], &args[1]) {
        (Val::Str(val), Val::Str(pat)) => {
            if let Err(e) = limits::check_regex_input("capture-all", val.len()) {
                return HotResult::Err(e);
            }
            let re = match compile_user_regex("capture-all", pat) {
                Ok(re) => re,
                Err(e) => return HotResult::Err(e),
            };
            let max_matches = limits::max_collection_size();
            let mut matches: Vec<Val> = Vec::new();
            for caps in re.captures_iter(val) {
                if matches.len() >= max_matches {
                    return HotResult::Err(Val::from(format!(
                        "capture-all: too many matches (limit {}). Raise HOT_MAX_COLLECTION_SIZE.",
                        max_matches
                    )));
                }
                let mut map = IndexMap::new();
                if let Some(full) = caps.get(0) {
                    map.insert(Val::from("match"), Val::from(full.as_str()));
                }
                let groups: Vec<Val> = caps
                    .iter()
                    .skip(1)
                    .map(|cap| match cap {
                        Some(c) => Val::from(c.as_str()),
                        None => Val::Null,
                    })
                    .collect();
                map.insert(Val::from("groups"), Val::Vec(groups));
                matches.push(Val::Map(Box::new(map)));
            }
            HotResult::Ok(Val::Vec(matches))
        }
        (Val::Str(_), _) => HotResult::Err(Val::from("capture-all pattern must be a string")),
        _ => HotResult::Err(Val::from("capture-all value must be a string")),
    }
}

/// Escape special regex characters in a string
pub fn escape(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::regex/escape", args, 1);

    match &args[0] {
        Val::Str(s) => HotResult::Ok(Val::from(regex::escape(s))),
        _ => HotResult::Err(Val::from("escape requires a string input")),
    }
}
