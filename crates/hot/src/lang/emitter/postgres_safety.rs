//! Postgres `jsonb` / `text` safety helpers.
//!
//! Postgres rejects a small, well-defined set of characters that JSON itself
//! and Rust `String`s otherwise accept. This module provides cheap, allocation-
//! free-on-the-fast-path helpers to scrub them before bind-time so we never
//! lose a whole transaction batch (and the trace data within it) because one
//! string from a container/webhook payload happened to contain a NUL byte.
//!
//! Forbidden character set we handle:
//!
//! 1. **NUL (U+0000).** Postgres can't store a NUL byte anywhere — `jsonb`,
//!    `text`, `varchar`. JSON encodes it as the 6-char escape `\u0000`; in
//!    raw text it appears as the byte `0x00`.
//! 2. **Lone UTF-16 surrogates (U+D800–U+DFFF, not in valid pairs).** `jsonb`
//!    rejects these via the same 22P05 error as NUL. They cannot appear as a
//!    Rust `char` (Rust enforces the Unicode-scalar-value invariant) but they
//!    *can* appear in a JSON-as-text string written by buggy upstream
//!    serializers as the 6-char escape `\uD800`. Properly-paired surrogates
//!    encoding non-BMP characters are preserved as-is.
//!
//! Both rewrites use the Unicode REPLACEMENT CHARACTER (U+FFFD) — the
//! character literally invented by Unicode to mark "an unrepresentable
//! character was here." `String::from_utf8_lossy` uses the same convention.

use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

/// JSON escape form of U+FFFD. Same length (6 chars) as the escapes it
/// replaces, so the surrounding JSON byte offsets stay stable.
const REPLACEMENT_ESCAPE: &str = r"\uFFFD";

/// UTF-8 form of U+FFFD (3 bytes: `0xEF 0xBF 0xBD`). Used for raw-text
/// columns where the forbidden char is a literal byte rather than a JSON
/// escape sequence.
const REPLACEMENT_CHAR: char = '\u{FFFD}';

/// Match Postgres-rejected `\uXXXX` JSON escapes:
///   - `\u0000` (NUL)
///   - `\u[dD][89aAbB][0-9a-fA-F]{2}` (high surrogate)
///   - `\u[dD][cCdDeEfF][0-9a-fA-F]{2}` (low surrogate)
///
/// Surrogates are matched here regardless of pairing; we keep paired ones
/// in [`replace_lone_surrogates`] by checking the lookahead manually.
static FORBIDDEN_ESCAPE_RE: OnceLock<Regex> = OnceLock::new();
fn forbidden_escape_re() -> &'static Regex {
    FORBIDDEN_ESCAPE_RE.get_or_init(|| {
        // `\\u(?:0000|[dD][0-9a-fA-F]{3})` — match anything that *might* be
        // forbidden; pairing logic decides which surrogates survive.
        Regex::new(r"\\u(?:0000|[dD][0-9a-fA-F]{3})")
            .expect("postgres_safety: forbidden-escape regex must compile")
    })
}

/// Sanitize a JSON-as-text string for safe insertion into a Postgres `jsonb`
/// column. Replaces `\u0000` and lone-surrogate escapes with `\uFFFD`.
///
/// Allocation-free fast path: returns `Cow::Borrowed(s)` if the input
/// contains no `\` byte at all (the only way our forbidden patterns can
/// appear in well-formed JSON output).
pub fn sanitize_json_for_jsonb(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().contains(&b'\\') {
        return Cow::Borrowed(s);
    }

    let re = forbidden_escape_re();
    if !re.is_match(s) {
        return Cow::Borrowed(s);
    }

    // We need to rewrite. Walk matches and decide per-match whether to
    // replace (NUL + lone surrogates) or keep (paired surrogates).
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let bytes = s.as_bytes();

    for m in re.find_iter(s) {
        out.push_str(&s[last..m.start()]);
        let matched = m.as_str();

        if matched == "\\u0000" {
            out.push_str(REPLACEMENT_ESCAPE);
        } else {
            // matched is `\uDXXX`. Distinguish high (D8–DB) vs low (DC–DF)
            // and check pairing.
            let nibble2 = matched.as_bytes()[3]; // first hex digit after `\uD`
            let is_high = matches!(nibble2, b'8' | b'9' | b'a' | b'A' | b'b' | b'B');
            let is_low = matches!(
                nibble2,
                b'c' | b'C' | b'd' | b'D' | b'e' | b'E' | b'f' | b'F'
            );

            if is_high {
                // Paired iff the next 6 bytes are `\uDC..` through `\uDF..`.
                let next = m.end();
                let paired = next + 6 <= bytes.len()
                    && bytes[next] == b'\\'
                    && bytes[next + 1] == b'u'
                    && (bytes[next + 2] == b'd' || bytes[next + 2] == b'D')
                    && matches!(
                        bytes[next + 3],
                        b'c' | b'C' | b'd' | b'D' | b'e' | b'E' | b'f' | b'F'
                    );
                if paired {
                    out.push_str(matched);
                } else {
                    out.push_str(REPLACEMENT_ESCAPE);
                }
            } else if is_low {
                // Paired iff the previous 6 bytes are `\uD8..` through `\uDB..`.
                let prev_start = m.start();
                let paired = prev_start >= 6
                    && bytes[prev_start - 6] == b'\\'
                    && bytes[prev_start - 5] == b'u'
                    && (bytes[prev_start - 4] == b'd' || bytes[prev_start - 4] == b'D')
                    && matches!(
                        bytes[prev_start - 3],
                        b'8' | b'9' | b'a' | b'A' | b'b' | b'B'
                    );
                if paired {
                    out.push_str(matched);
                } else {
                    out.push_str(REPLACEMENT_ESCAPE);
                }
            } else {
                // Shouldn't happen given the regex, but be defensive.
                out.push_str(matched);
            }
        }
        last = m.end();
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

/// Sanitize a plain text string (not JSON) for safe insertion into a
/// Postgres `text`/`varchar` column. Only NUL (U+0000) bytes are
/// disallowed; everything else valid UTF-8 is fine. Rust `String`s are
/// UTF-8 by construction, so we only need to scrub NULs.
///
/// Allocation-free fast path: returns `Cow::Borrowed(s)` if the input
/// contains no NUL.
pub fn sanitize_text_for_postgres(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().contains(&0) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace('\0', &REPLACEMENT_CHAR.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonb_no_op_when_input_clean() {
        let input = r#"{"msg":"hello world","n":42}"#;
        let out = sanitize_json_for_jsonb(input);
        assert!(matches!(out, Cow::Borrowed(_)), "should be borrowed");
        assert_eq!(out, input);
    }

    #[test]
    fn jsonb_no_op_when_no_backslashes() {
        // Common case for short trace payloads — bail before regex.
        let input = r#"{"x":1,"y":"abc"}"#;
        assert!(matches!(sanitize_json_for_jsonb(input), Cow::Borrowed(_)));
    }

    #[test]
    fn jsonb_no_op_when_only_safe_escapes() {
        // \n, \t, \", and BMP-escape \u00a0 are all valid in jsonb.
        let input = r#"{"s":"line1\nline2\t\"quoted\"\u00a0nbsp"}"#;
        assert!(matches!(sanitize_json_for_jsonb(input), Cow::Borrowed(_)));
    }

    #[test]
    fn jsonb_replaces_u0000() {
        let input = r#"{"out":"hi\u0000bye"}"#;
        let expected = r#"{"out":"hi\uFFFDbye"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn jsonb_replaces_multiple_u0000() {
        let input = r#"{"a":"\u0000","b":"x\u0000y\u0000z"}"#;
        let expected = r#"{"a":"\uFFFD","b":"x\uFFFDy\uFFFDz"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn jsonb_preserves_paired_surrogates() {
        // \uD83D\uDE00 = 😀 (U+1F600). Properly paired — must survive.
        let input = r#"{"emoji":"\uD83D\uDE00"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), input);
    }

    #[test]
    fn jsonb_replaces_lone_high_surrogate() {
        // \uD83D not followed by a low surrogate.
        let input = r#"{"bad":"\uD83Dxyz"}"#;
        let expected = r#"{"bad":"\uFFFDxyz"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn jsonb_replaces_lone_low_surrogate() {
        let input = r#"{"bad":"abc\uDE00"}"#;
        let expected = r#"{"bad":"abc\uFFFD"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn jsonb_replaces_two_lone_high_surrogates_in_a_row_neither_pairs() {
        // Two adjacent high surrogates: each is "next escape is also a
        // high surrogate, not a low", so neither qualifies as paired.
        let input = r#"{"bad":"\uD83D\uD83D"}"#;
        let expected = r#"{"bad":"\uFFFD\uFFFD"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn jsonb_handles_mixed_paired_and_lone() {
        // 😀 (paired) + lone high + 🌟 (paired)
        let input = r#"{"s":"\uD83D\uDE00\uD83D abc \uD83C\uDF1F"}"#;
        let expected = r#"{"s":"\uD83D\uDE00\uFFFD abc \uD83C\uDF1F"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn jsonb_handles_uppercase_escapes() {
        // serde always lowercases hex in escapes, but be tolerant of
        // hand-built JSON that uses uppercase.
        let input = r#"{"a":"\u0000","b":"\uD83D"}"#;
        let expected = r#"{"a":"\uFFFD","b":"\uFFFD"}"#;
        assert_eq!(sanitize_json_for_jsonb(input), expected);
    }

    #[test]
    fn text_no_op_when_clean() {
        let input = "hello world";
        assert!(matches!(
            sanitize_text_for_postgres(input),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn text_replaces_raw_nul() {
        let input = "before\0after";
        let out = sanitize_text_for_postgres(input);
        assert_eq!(out, "before\u{FFFD}after");
    }

    #[test]
    fn text_preserves_other_control_chars() {
        let input = "tab\there\nnewline";
        assert!(matches!(
            sanitize_text_for_postgres(input),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn jsonb_preserves_byte_offsets_for_replaced_escapes() {
        // \u0000 (6 chars) → \uFFFD (6 chars). Length must match exactly so
        // any downstream code that tracked offsets stays valid.
        let input = r#""hi\u0000bye""#;
        let out = sanitize_json_for_jsonb(input);
        assert_eq!(out.len(), input.len());
    }
}
