// Hidden Rust-backed bindings for `::hot::internal::tokenizer/*`.
//
// These are intentionally undocumented (no-doc) and are meant to be
// called only from the `::ai::tokenizer` Hot module. They expose the
// `tiktoken-rs` BPE tokenizer as Hot functions so that Hot code can
// produce accurate token counts for OpenAI-family models without
// shelling out or guessing with character heuristics.
//
// Each encoding name maps to one of the cached singletons that
// `tiktoken-rs` exposes; the singletons load their merge tables once
// per process and are cheap to call thereafter.
//
// Two functions are exposed:
//
//   ::hot::internal::tokenizer/count(text: Str, encoding: Str): Int
//   ::hot::internal::tokenizer/encodings(): Vec<Str>
//
// `count` returns the number of tokens that `text` would emit when
// encoded with the given encoding; unknown encodings return an
// `Err`. `encodings` returns the names this build of the runtime
// supports, so wrappers can advertise capability without hard-coding
// the list.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use tiktoken_rs::{
    CoreBPE, cl100k_base_singleton, o200k_base_singleton, o200k_harmony_singleton,
    p50k_base_singleton, p50k_edit_singleton, r50k_base_singleton,
};

/// Canonical list of encodings this runtime can tokenize with. The
/// names match the upstream `tiktoken` strings so that a model->encoding
/// lookup table in `::ai::tokenizer` can pass values straight through.
const SUPPORTED_ENCODINGS: &[&str] = &[
    "o200k_harmony",
    "o200k_base",
    "cl100k_base",
    "p50k_base",
    "p50k_edit",
    "r50k_base",
    "gpt2", // alias for r50k_base
];

fn bpe_for_encoding(name: &str) -> Option<&'static CoreBPE> {
    match name {
        "o200k_harmony" => Some(o200k_harmony_singleton()),
        "o200k_base" => Some(o200k_base_singleton()),
        "cl100k_base" => Some(cl100k_base_singleton()),
        "p50k_base" => Some(p50k_base_singleton()),
        "p50k_edit" => Some(p50k_edit_singleton()),
        "r50k_base" | "gpt2" => Some(r50k_base_singleton()),
        _ => None,
    }
}

fn extract_str(arg: &Val) -> Option<&str> {
    match arg {
        Val::Str(s) => Some(s.as_ref()),
        _ => None,
    }
}

/// `::hot::internal::tokenizer/count(text: Str, encoding: Str): Int`
///
/// Returns the token count for `text` using the named BPE encoding
/// (`"o200k_base"`, `"cl100k_base"`, etc.). Special tokens in `text`
/// are encoded as their special-token IDs.
pub fn count(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::internal::tokenizer/count", args, 2);
    let text = match extract_str(&args[0]) {
        Some(s) => s,
        None => {
            return HotResult::Err(Val::from(
                "::hot::internal::tokenizer/count: `text` must be a Str".to_string(),
            ));
        }
    };
    let encoding = match extract_str(&args[1]) {
        Some(s) => s,
        None => {
            return HotResult::Err(Val::from(
                "::hot::internal::tokenizer/count: `encoding` must be a Str".to_string(),
            ));
        }
    };
    let bpe = match bpe_for_encoding(encoding) {
        Some(b) => b,
        None => {
            return HotResult::Err(Val::from(format!(
                "::hot::internal::tokenizer/count: unknown encoding '{}' (supported: {})",
                encoding,
                SUPPORTED_ENCODINGS.join(", "),
            )));
        }
    };
    let tokens = bpe.encode_with_special_tokens(text);
    HotResult::Ok(Val::Int(tokens.len() as i64))
}

/// `::hot::internal::tokenizer/encodings(): Vec<Str>`
///
/// Returns the list of encoding names this runtime can tokenize with.
/// Useful for diagnostics and for `::ai::tokenizer` to validate a
/// model->encoding lookup against what the host actually ships.
pub fn encodings(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(
            "::hot::internal::tokenizer/encodings takes no arguments".to_string(),
        ));
    }
    let out: Vec<Val> = SUPPORTED_ENCODINGS.iter().map(|s| Val::from(*s)).collect();
    HotResult::Ok(Val::Vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_o200k_base_simple() {
        let result = count(&[Val::from("hello world"), Val::from("o200k_base")]);
        match result {
            HotResult::Ok(Val::Int(n)) => assert!(n >= 1, "expected positive token count, got {n}"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn count_cl100k_base_matches_known_value() {
        // "hello world" is famously 2 tokens in cl100k_base.
        let result = count(&[Val::from("hello world"), Val::from("cl100k_base")]);
        match result {
            HotResult::Ok(Val::Int(n)) => assert_eq!(n, 2),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn count_unknown_encoding_errs() {
        let result = count(&[Val::from("hi"), Val::from("definitely-not-real")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn count_arg_count_validation() {
        assert!(matches!(count(&[Val::from("hi")]), HotResult::Err(_)));
    }

    #[test]
    fn encodings_includes_known_names() {
        match encodings(&[]) {
            HotResult::Ok(Val::Vec(v)) => {
                let names: Vec<String> = v
                    .iter()
                    .filter_map(|x| match x {
                        Val::Str(s) => Some((**s).to_owned()),
                        _ => None,
                    })
                    .collect();
                assert!(names.contains(&"o200k_base".to_string()));
                assert!(names.contains(&"cl100k_base".to_string()));
                assert!(names.contains(&"r50k_base".to_string()));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn r50k_base_alias_gpt2_works() {
        let a = count(&[Val::from("hello"), Val::from("r50k_base")]);
        let b = count(&[Val::from("hello"), Val::from("gpt2")]);
        match (a, b) {
            (HotResult::Ok(Val::Int(x)), HotResult::Ok(Val::Int(y))) => assert_eq!(x, y),
            other => panic!("unexpected: {:?}", other),
        }
    }
}
