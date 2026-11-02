//! Keeping credentials out of what Hot records about a run.
//!
//! Records (call args, return values, errors and flows, run results and
//! failures, stream payloads, console output) pass through two independent
//! filters before they are stored or sent:
//!
//! - **Registered secrets** ([`SecretValues`]): values a run marked as secret,
//!   wherever they appear inside a string, raw or encoded. Values shorter than
//!   [`MIN_SUBSTRING_LEN`] match only a whole value.
//! - **Credential fields** ([`is_credential_field`]): fields named like
//!   credentials, whether or not anything marked their values, in maps and in
//!   JSON, form-encoded or `name: value` text inside strings.
//!
//! The first is why registered values are held in the clear: matching a
//! substring needs the bytes. They are the values the run already holds in its
//! context, in the same process, for no longer than its records are pending.
//! [`SecretValues`] never serializes, so they don't reach a queue message, and
//! never prints them.

use crate::val::Val;
use aho_corasick::AhoCorasick;
use base64::Engine as _;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};

use ahash::AHashSet;

/// What a masked value reads as in a record.
pub const MASK: &str = "<secret>";

/// Shorter secrets match only a whole value: inside other text, they would
/// match by chance.
pub const MIN_SUBSTRING_LEN: usize = 8;

/// Longer secrets match only a whole value, to bound what the matcher holds.
/// No credential is this long.
const MAX_SUBSTRING_LEN: usize = 64 * 1024;

/// The hash of `value`, as recorded in `ExecutionContext::secret_value_hashes`.
pub fn value_hash(value: &Val) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Registered secrets
// ---------------------------------------------------------------------------

/// The secret values a run has registered.
///
/// Every clone of a run's `ExecutionContext` shares one registry, so a record
/// masked at flush time sees what was registered after its event was emitted,
/// and the values go when the last clone does.
#[derive(Clone, Default)]
pub struct SecretValues(Arc<RwLock<Registry>>);

#[derive(Default)]
struct Registry {
    values: AHashSet<Vec<u8>>,
    exact_hashes: AHashSet<u64>,
    /// Bumped by each new value, so a record can tell it was masked before it.
    generation: u64,
    matchers: Option<Arc<Matchers>>,
}

/// What matches the values registered at some moment.
#[derive(Default)]
struct Matchers {
    substrings: Option<SecretMatcher>,
    exact_hashes: AHashSet<u64>,
}

impl std::fmt::Debug for SecretValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let registry = self.0.read().unwrap_or_else(|error| error.into_inner());
        write!(f, "SecretValues({} values)", registry.values.len())
    }
}

// Contexts compare by what identifies a run; the registry isn't part of that.
impl PartialEq for SecretValues {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl SecretValues {
    /// Mask `value` and every leaf in it, as whole values and inside strings.
    pub fn register(&self, value: &Val) {
        let mut new_values = Vec::new();
        collect_values(value, &mut new_values);
        self.add(value, new_values);
    }

    /// Mask `value` and every leaf in it where it is the whole value only.
    pub fn register_exact(&self, value: &Val) {
        self.add(value, Vec::new());
    }

    fn add(&self, value: &Val, new_values: Vec<Vec<u8>>) {
        let mut new_hashes = AHashSet::new();
        crate::lang::hot::ctx::hash_secret_value_recursive(value, &mut new_hashes);
        let mut registry = self.0.write().unwrap_or_else(|error| error.into_inner());
        let before = registry.values.len() + registry.exact_hashes.len();
        registry.values.extend(new_values);
        registry.exact_hashes.extend(new_hashes);
        if registry.values.len() + registry.exact_hashes.len() != before {
            registry.generation += 1;
            registry.matchers = None;
        }
    }

    /// Register what in `value` has a hash in `hashes`, for values that only
    /// their hash identifies as secret when they get here.
    pub fn register_hashed(&self, value: &Val, hashes: &AHashSet<u64>) {
        if hashes.contains(&value_hash(value)) {
            self.register(value);
            return;
        }
        match value {
            Val::Map(map) => {
                for (_key, child) in map.iter() {
                    self.register_hashed(child, hashes);
                }
            }
            Val::Vec(items) => {
                for child in items.iter() {
                    self.register_hashed(child, hashes);
                }
            }
            _ => {}
        }
    }

    /// Changes when a new value is registered.
    pub fn generation(&self) -> u64 {
        self.0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .generation
    }

    /// The matchers for the values registered so far.
    fn matchers(&self) -> Arc<Matchers> {
        {
            let registry = self.0.read().unwrap_or_else(|error| error.into_inner());
            if let Some(matchers) = &registry.matchers {
                return Arc::clone(matchers);
            }
        }
        let mut registry = self.0.write().unwrap_or_else(|error| error.into_inner());
        if registry.matchers.is_none() {
            registry.matchers = Some(Arc::new(Matchers {
                substrings: SecretMatcher::new(&registry.values),
                exact_hashes: registry.exact_hashes.clone(),
            }));
        }
        registry.matchers.clone().expect("matchers were just built")
    }
}

/// Add what `value` contributes to matching: each string long enough to match
/// by chance rarely, and what that string is known to wrap.
fn collect_values(value: &Val, out: &mut Vec<Vec<u8>>) {
    match value {
        Val::Map(map) => {
            for (_key, child) in map.iter() {
                collect_values(child, out);
            }
        }
        Val::Vec(items) => {
            for child in items.iter() {
                collect_values(child, out);
            }
        }
        Val::Str(text) => collect_text(text, out),
        Val::Bytes(bytes) => push_value(bytes, out),
        Val::Int(number) => collect_text(&number.to_string(), out),
        Val::Dec(number) => collect_text(&number.to_string(), out),
        _ => {}
    }
}

fn collect_text(text: &str, out: &mut Vec<Vec<u8>>) {
    let trimmed = text.trim();
    push_value(text.as_bytes(), out);
    push_value(trimmed.as_bytes(), out);
    // A whole `Authorization` value: code that reads it uses the token alone.
    if let Some((scheme, credential)) = trimmed.split_once(char::is_whitespace)
        && ["bearer", "basic", "token"]
            .iter()
            .any(|known| scheme.eq_ignore_ascii_case(known))
    {
        push_value(credential.trim().as_bytes(), out);
    }
    // A credential file read into a string, like a service account's JSON:
    // code that parses it uses its fields alone.
    if trimmed.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Val>(trimmed)
    {
        collect_values(&parsed, out);
    }
}

fn push_value(value: &[u8], out: &mut Vec<Vec<u8>>) {
    if (MIN_SUBSTRING_LEN..=MAX_SUBSTRING_LEN).contains(&value.len())
        && value.iter().any(|byte| !byte.is_ascii_whitespace())
    {
        out.push(value.to_vec());
    }
}

/// Finds registered values, raw and encoded, in a single pass over a string.
struct SecretMatcher {
    automaton: AhoCorasick,
    /// By pattern: whether it is a piece of a base64 run, whose whole run the
    /// mask replaces, because the rest of the run encodes what surrounds the
    /// secret in the same value (`user:` in `Basic base64(user:secret)`).
    within_base64: Vec<bool>,
}

impl SecretMatcher {
    fn new(values: &AHashSet<Vec<u8>>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut raw: AHashSet<Vec<u8>> = AHashSet::new();
        let mut encoded: AHashSet<Vec<u8>> = AHashSet::new();
        for value in values {
            raw.insert(value.clone());
            for variant in text_encodings(value) {
                raw.insert(variant.into_bytes());
            }
            for variant in base64_fragments(value) {
                encoded.insert(variant.into_bytes());
            }
        }
        let encoded: Vec<Vec<u8>> = encoded.into_iter().filter(|p| !raw.contains(p)).collect();
        let mut within_base64 = vec![false; raw.len()];
        within_base64.extend(std::iter::repeat_n(true, encoded.len()));
        let patterns: Vec<Vec<u8>> = raw.into_iter().chain(encoded).collect();
        match AhoCorasick::new(&patterns) {
            Ok(automaton) => Some(Self {
                automaton,
                within_base64,
            }),
            Err(error) => {
                // Never expected: nothing above approaches the automaton's
                // size limits. Field redaction and exact matches still apply.
                tracing::error!("secret matcher failed to build: {}", error);
                None
            }
        }
    }

    /// The byte ranges of `text` to mask, in order and not overlapping.
    fn ranges(&self, text: &[u8]) -> Vec<(usize, usize)> {
        if !self.automaton.is_match(text) {
            return Vec::new();
        }
        let mut ranges: Vec<(usize, usize)> = self
            .automaton
            .find_overlapping_iter(text)
            .map(|found| {
                let (mut start, mut end) = (found.start(), found.end());
                if self.within_base64[found.pattern().as_usize()] {
                    while start > 0 && is_base64_byte(text[start - 1]) {
                        start -= 1;
                    }
                    while end < text.len() && is_base64_byte(text[end]) {
                        end += 1;
                    }
                }
                (start, end)
            })
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        merged
    }

    fn mask<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let ranges = self.ranges(text.as_bytes());
        if ranges.is_empty() {
            return Cow::Borrowed(text);
        }
        let bytes = text.as_bytes();
        let mut masked = Vec::with_capacity(text.len());
        let mut position = 0;
        for (start, end) in ranges {
            masked.extend_from_slice(&bytes[position..start]);
            masked.extend_from_slice(MASK.as_bytes());
            position = end;
        }
        masked.extend_from_slice(&bytes[position..]);
        // A registered value that isn't UTF-8 may match inside a character.
        Cow::Owned(String::from_utf8_lossy(&masked).into_owned())
    }
}

fn is_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_' | b'=')
}

/// `value` as URL or form encoding or a JSON string would carry it, where that
/// differs from `value`.
fn text_encodings(value: &[u8]) -> Vec<String> {
    let mut variants = vec![
        percent_encoding::percent_encode(value, COMPONENT).to_string(),
        form_urlencoded::byte_serialize(value).collect(),
    ];
    if let Ok(text) = std::str::from_utf8(value)
        && let Ok(quoted) = serde_json::to_string(text)
    {
        variants.push(quoted[1..quoted.len() - 1].to_string());
    }
    variants
}

const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// What base64 of any text containing `value` must contain, for each of the
/// three alignments `value` can start at within that text, in both alphabets.
///
/// This is how `Basic base64(user:secret)` is found without knowing `user`.
fn base64_fragments(value: &[u8]) -> Vec<String> {
    let mut fragments = Vec::with_capacity(6);
    for offset in 0..3usize {
        let mut aligned = vec![0u8; offset];
        aligned.extend_from_slice(value);
        let standard = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&aligned);
        // Characters that also encode bits of what precedes or follows.
        let leading = [0, 2, 3][offset];
        let trailing = usize::from(!aligned.len().is_multiple_of(3));
        let stable = &standard[leading..standard.len() - trailing];
        fragments.push(stable.to_string());
        fragments.push(stable.replace('+', "-").replace('/', "_"));
    }
    fragments
}

// ---------------------------------------------------------------------------
// Credential fields
// ---------------------------------------------------------------------------

/// The ends of field names that hold credentials, as lowercase words. A name
/// matches if its last words are one of these: `stripe_api_key`, `X-Api-Key`
/// and `apiKey` all end in `api key`, and `password` matches `db_password` but
/// not `password_hint`.
///
/// Aimed at credentials. A bare `token` is left out on purpose: it names
/// `next_page_token`-style cursors and token counts in API traces far more
/// often than a credential, and a real one is masked if it is registered. So
/// are bare `key` and `code`, for the same reason. `session_id` is left out as
/// trace data that identifies a session without granting it.
pub const CREDENTIAL_FIELDS: &[&[&str]] = &[
    // HTTP headers
    &["authorization"],
    &["cookie"],
    // Named secrets
    &["password"],
    &["passwd"],
    &["passphrase"],
    &["secret"],
    &["credential"],
    &["credentials"],
    // API keys
    &["apikey"],
    &["api", "key"],
    &["api", "secret"],
    &["api", "token"],
    &["access", "key"],
    &["secret", "key"],
    &["private", "key"],
    &["signing", "key"],
    &["encryption", "key"],
    // Tokens
    &["access", "token"],
    &["refresh", "token"],
    &["id", "token"],
    &["auth", "token"],
    &["bearer", "token"],
    &["session", "token"],
    &["security", "token"],
    &["private", "token"],
    &["vault", "token"],
    &["csrf", "token"],
    &["xsrf", "token"],
    // OAuth
    &["client", "assertion"],
    &["code", "verifier"],
];

/// Whether a field called `name` holds a credential, in any case or
/// separator style. See [`CREDENTIAL_FIELDS`].
pub fn is_credential_field(name: &str) -> bool {
    // Most names can't match; don't split them to find out.
    if !field_candidates().is_match(name) {
        return false;
    }
    let words = field_words(name);
    CREDENTIAL_FIELDS.iter().any(|field| {
        words.len() >= field.len()
            && words[words.len() - field.len()..]
                .iter()
                .zip(field.iter())
                .all(|(word, part)| word == part)
    })
}

/// Lowercase words of a field name: `X-Api-Key`, `x_api_key`, `x.api.key` and
/// `xApiKey` are all `x api key`.
fn field_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if !character.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower {
            words.push(std::mem::take(&mut word));
        }
        previous_lower = character.is_lowercase();
        word.extend(character.to_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Whether the mask replaces a credential field holding `value`. `null`,
/// `true` and empty values say a credential isn't there; they don't hold one.
fn holds_credential(value: &Val) -> bool {
    match value {
        Val::Null | Val::Bool(_) => false,
        Val::Str(text) => !text.is_empty(),
        Val::Bytes(bytes) => !bytes.is_empty(),
        Val::Vec(items) => !items.is_empty(),
        Val::Map(map) => !map.is_empty(),
        _ => true,
    }
}

/// Finds text that may name a credential field, in any separator style, to
/// keep the costlier checks off strings that can't.
fn field_candidates() -> &'static AhoCorasick {
    static CANDIDATES: Lazy<AhoCorasick> = Lazy::new(|| {
        let mut patterns: Vec<String> = Vec::new();
        for field in CREDENTIAL_FIELDS {
            patterns.push(field.concat());
            if field.len() > 1 {
                for separator in ["_", "-", "."] {
                    patterns.push(field.join(separator));
                }
            }
        }
        AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(patterns)
            .expect("credential field names are a valid pattern set")
    });
    &CANDIDATES
}

/// A credential field in text: JSON that didn't parse (a stream's lines, a
/// JSON body after a message), `name=value` pairs (a form body, a query
/// string) or `name: value` lines (a header dump).
static TEXT_FIELD: Lazy<Regex> = Lazy::new(|| {
    let names: Vec<String> = CREDENTIAL_FIELDS
        .iter()
        .map(|field| field.join(r"[-_.]?"))
        .collect();
    Regex::new(&format!(
        r#"(?xi)
        (?P<key> \b (?: [a-z0-9]+ [-_.] )* (?: {names} ) \b )
        (?P<close> \\? ["']? )
        (?:
            (?P<colon> \s* : \s* )
            (?:
                (?P<escaped> \\" (?: [^"\\] | \\ [^"] )* \\" )
                | (?P<quoted> " (?: [^"\\] | \\ . )* " )
                | (?P<single> ' [^']* ' )
                | (?P<bare> [^\r\n,}}\]]+ )
            )
            | (?P<equals> \s* = \s* ) (?P<form> [^&\s"'\#]+ )
        )"#,
        names = names.join("|")
    ))
    .expect("credential field pattern is valid")
});

/// A password in a URL's userinfo.
static URL_PASSWORD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://[^/\s:@]*:)[^/\s@]+(@)")
        .expect("URL password pattern is valid")
});

/// Mask credential fields found in JSON, form or `name: value` text.
fn redact_text_fields(text: &str) -> Cow<'_, str> {
    let mut text = Cow::Borrowed(text);
    if text.contains("://")
        && text.contains('@')
        && let Cow::Owned(masked) = URL_PASSWORD.replace_all(&text, format!("${{1}}{MASK}${{2}}"))
    {
        text = Cow::Owned(masked);
    }
    if !field_candidates().is_match(text.as_bytes()) {
        return text;
    }
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        return if redact_json_fields(&mut parsed) {
            match serde_json::to_string(&parsed) {
                Ok(json) => Cow::Owned(json),
                // Never expected of a value just parsed. Fail closed.
                Err(_) => Cow::Borrowed(MASK),
            }
        } else {
            text
        };
    }
    match TEXT_FIELD.replace_all(&text, redact_text_field) {
        Cow::Owned(masked) => Cow::Owned(masked),
        Cow::Borrowed(_) => text,
    }
}

fn redact_text_field(captures: &Captures<'_>) -> String {
    let key = &captures["key"];
    let close = &captures["close"];
    if let Some(equals) = captures.name("equals") {
        return format!("{key}{close}{}{MASK}", equals.as_str());
    }
    let colon = &captures["colon"];
    let masked = if captures.name("escaped").is_some() {
        format!("\\\"{MASK}\\\"")
    } else if captures.name("quoted").is_some() {
        format!("\"{MASK}\"")
    } else if captures.name("single").is_some() {
        format!("'{MASK}'")
    } else {
        let bare = captures
            .name("bare")
            .map_or("", |bare| bare.as_str().trim());
        if matches!(bare, "null" | "true" | "false" | "") {
            return captures[0].to_string();
        }
        MASK.to_string()
    };
    format!("{key}{close}{colon}{masked}")
}

/// Whether `bytes`, read as text, hold a credential field or URL password.
fn bytes_name_a_credential(bytes: &[u8]) -> bool {
    (field_candidates().is_match(bytes) || bytes.windows(3).any(|window| window == b"://"))
        && matches!(
            redact_text_fields(&String::from_utf8_lossy(bytes)),
            Cow::Owned(_)
        )
}

/// Mask credential fields in parsed JSON, and in strings within it. Whether
/// anything changed.
fn redact_json_fields(value: &mut serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for (key, child) in map.iter_mut() {
                let empty = match child {
                    Value::Null | Value::Bool(_) => true,
                    Value::String(text) => text.is_empty(),
                    Value::Array(items) => items.is_empty(),
                    Value::Object(fields) => fields.is_empty(),
                    Value::Number(_) => false,
                };
                if !empty && is_credential_field(key) {
                    *child = Value::String(MASK.to_string());
                    changed = true;
                } else {
                    changed |= redact_json_fields(child);
                }
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_json_fields(item);
            }
            changed
        }
        Value::String(text) => {
            if let Cow::Owned(masked) = redact_text_fields(text) {
                *text = masked;
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Masking a record
// ---------------------------------------------------------------------------

/// What masks one record. Taken fresh for each, so it sees what was registered
/// up to that moment.
pub struct RecordMask<'a> {
    matchers: Arc<Matchers>,
    /// Hashes that came from elsewhere, like an API handler.
    extra_hashes: &'a AHashSet<u64>,
}

impl<'a> RecordMask<'a> {
    pub fn new(secret_values: &SecretValues, extra_hashes: &'a AHashSet<u64>) -> Self {
        Self {
            matchers: secret_values.matchers(),
            extra_hashes,
        }
    }

    fn substrings(&self) -> Option<&SecretMatcher> {
        self.matchers.substrings.as_ref()
    }

    fn has_exact(&self) -> bool {
        !self.matchers.exact_hashes.is_empty() || !self.extra_hashes.is_empty()
    }

    /// The whole value matching a registered value's hash.
    fn is_exact(&self, value: &Val) -> bool {
        self.has_exact() && {
            let hash = value_hash(value);
            self.matchers.exact_hashes.contains(&hash) || self.extra_hashes.contains(&hash)
        }
    }

    /// Mask a recorded value: registered secrets and credential fields.
    pub fn mask_val(&self, value: Val) -> Val {
        if self.is_exact(&value) {
            return Val::from(MASK);
        }
        match value {
            Val::Map(map) => {
                let masked: indexmap::IndexMap<Val, Val> = Arc::unwrap_or_clone(map)
                    .into_iter()
                    .map(|(key, child)| {
                        let credential = matches!(&key, Val::Str(name) if is_credential_field(name))
                            && holds_credential(&child);
                        let key = self.mask_val(key);
                        if credential {
                            (key, Val::from(MASK))
                        } else {
                            (key, self.mask_val(child))
                        }
                    })
                    .collect();
                Val::map(masked)
            }
            Val::Vec(items) => Val::vec(
                Arc::unwrap_or_clone(items)
                    .into_iter()
                    .map(|v| self.mask_val(v))
                    .collect(),
            ),
            Val::Str(text) => match self.mask_text(&text) {
                Cow::Borrowed(_) => Val::Str(text),
                Cow::Owned(masked) => Val::from(masked),
            },
            Val::Bytes(bytes) => {
                // A response's bytes repeat its body. They aren't masked in
                // part: nothing could read what was left.
                let holds_secret = self
                    .substrings()
                    .is_some_and(|matcher| matcher.automaton.is_match(&bytes));
                if holds_secret || bytes_name_a_credential(&bytes) {
                    Val::from(MASK)
                } else {
                    Val::Bytes(bytes)
                }
            }
            other => other,
        }
    }

    /// Mask a recorded string: registered secrets and credential fields.
    pub fn mask_text<'t>(&self, text: &'t str) -> Cow<'t, str> {
        // Registered secrets first: a field's value ends at characters that a
        // secret may hold, which would leave the rest of the secret behind.
        let Some(matcher) = self.substrings() else {
            return redact_text_fields(text);
        };
        match matcher.mask(text) {
            Cow::Borrowed(text) => redact_text_fields(text),
            Cow::Owned(masked) => match redact_text_fields(&masked) {
                Cow::Borrowed(_) => Cow::Owned(masked),
                Cow::Owned(redacted) => Cow::Owned(redacted),
            },
        }
    }

    /// Mask a recorded value serialized as JSON. Most records name no
    /// credential and the run has no secrets: those aren't parsed.
    pub fn mask_json(&self, json: &str) -> Option<String> {
        if self.substrings().is_none()
            && !self.has_exact()
            && !field_candidates().is_match(json)
            && !(json.contains("://") && json.contains('@'))
            // Bytes are in JSON as numbers, so nothing above sees inside them.
            && !json.contains(crate::val::HOT_BYTES_TYPE)
        {
            return None;
        }
        // Deeper than the parser goes, say. It can't be checked, so it isn't kept.
        let masked = match serde_json::from_str::<Val>(json) {
            Ok(value) => serde_json::to_string(&self.mask_val(value)).ok(),
            Err(_) => None,
        };
        Some(masked.unwrap_or_else(|| format!("\"{MASK}\"")))
    }

    /// Mask a registered secret in a payload delivered to a live consumer,
    /// leaving credential fields, which may be what it asked for.
    pub fn mask_delivered(&self, value: Val) -> Val {
        let Some(matcher) = self.substrings() else {
            return value;
        };
        match value {
            Val::Map(map) => Val::map(
                Arc::unwrap_or_clone(map)
                    .into_iter()
                    .map(|(key, child)| (self.mask_delivered(key), self.mask_delivered(child)))
                    .collect(),
            ),
            Val::Vec(items) => Val::vec(
                Arc::unwrap_or_clone(items)
                    .into_iter()
                    .map(|v| self.mask_delivered(v))
                    .collect(),
            ),
            Val::Str(text) => match matcher.mask(&text) {
                Cow::Borrowed(_) => Val::Str(text),
                Cow::Owned(masked) => Val::from(masked),
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::val;

    const SECRET: &str = "s3cr3t+Value/With=Stuff";
    const PLAIN: &str = "sentinelsecretvalue123";

    fn is_empty(registry: &SecretValues) -> bool {
        let matchers = registry.matchers();
        matchers.substrings.is_none() && matchers.exact_hashes.is_empty()
    }

    fn mask_for(values: &[&str]) -> (SecretValues, AHashSet<u64>) {
        let registry = SecretValues::default();
        for value in values {
            registry.register(&Val::from(*value));
        }
        (registry, AHashSet::new())
    }

    fn masked_text(values: &[&str], text: &str) -> String {
        let (registry, hashes) = mask_for(values);
        RecordMask::new(&registry, &hashes)
            .mask_text(text)
            .into_owned()
    }

    fn std_b64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    fn url_b64(text: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
    }

    #[test]
    fn masks_a_secret_inside_text() {
        assert_eq!(
            masked_text(&[PLAIN], &format!("Bearer {PLAIN}")),
            "Bearer <secret>"
        );
        assert_eq!(masked_text(&[PLAIN], PLAIN), MASK);
        assert_eq!(
            masked_text(&[PLAIN], &format!("{PLAIN} and {PLAIN}!")),
            "<secret> and <secret>!"
        );
    }

    #[test]
    fn leaves_text_without_a_secret_alone() {
        let text = "Bearer something-else";
        assert_eq!(masked_text(&[PLAIN], text), text);
    }

    #[test]
    fn masks_url_and_form_encodings() {
        let encoded = percent_encoding::percent_encode(SECRET.as_bytes(), COMPONENT).to_string();
        assert_ne!(encoded, SECRET);
        assert_eq!(
            masked_text(&[SECRET], &format!("code=x&k={encoded}")),
            "code=x&k=<secret>"
        );
        let spaced = "two words and more";
        let form: String = form_urlencoded::byte_serialize(spaced.as_bytes()).collect();
        assert_eq!(masked_text(&[spaced], &format!("k={form}")), "k=<secret>");
    }

    #[test]
    fn masks_json_escaped_values() {
        let quoted = r#"has "quotes" inside"#;
        let body = serde_json::json!({"note": quoted}).to_string();
        let masked = masked_text(&[quoted], &body);
        assert!(!masked.contains("quotes"), "{masked}");
    }

    #[test]
    fn masks_base64_at_every_alignment_and_the_whole_run() {
        for prefix in ["", "a", "ab", "abc", "user:", "somebody:"] {
            let composed = format!("{prefix}{PLAIN}:suffix");
            for encoded in [std_b64(&composed), url_b64(&composed)] {
                assert_eq!(
                    masked_text(&[PLAIN], &format!("Basic {encoded}")),
                    "Basic <secret>",
                    "prefix {prefix:?}, encoded {encoded}"
                );
            }
        }
        assert_eq!(masked_text(&[PLAIN], &std_b64(PLAIN)), MASK);
    }

    #[test]
    fn values_too_short_match_only_whole() {
        let registry = SecretValues::default();
        let short = Val::from("abc");
        registry.register(&short);
        let hashes = AHashSet::new();
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(mask.mask_val(short), Val::from(MASK));
        assert_eq!(mask.mask_val(Val::from("abcdef")), Val::from("abcdef"));
    }

    #[test]
    fn empty_and_boolean_values_register_nothing() {
        let registry = SecretValues::default();
        registry.register(&Val::Bool(true));
        registry.register(&Val::Null);
        registry.register(&Val::from(""));
        assert!(is_empty(&registry));
        let registry = SecretValues::default();
        registry.register(&val!({"a": true, "b": Val::Null, "c": ""}));
        let hashes = AHashSet::new();
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(mask.mask_val(Val::Bool(true)), Val::Bool(true));
        assert_eq!(mask.mask_val(Val::Null), Val::Null);
        assert_eq!(mask.mask_val(Val::from("")), Val::from(""));
    }

    #[test]
    fn exact_registration_matches_only_the_whole_value() {
        let registry = SecretValues::default();
        registry.register_exact(&Val::from("us-east-1"));
        let hashes = AHashSet::new();
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(
            mask.mask_text("s3.us-east-1.example.com"),
            "s3.us-east-1.example.com"
        );
        assert_eq!(mask.mask_val(Val::from("us-east-1")), Val::from(MASK));
    }

    #[test]
    fn registers_what_only_a_hash_identifies() {
        let leaf = Val::from(PLAIN);
        let hashes = AHashSet::from_iter([value_hash(&leaf)]);
        let registry = SecretValues::default();
        registry.register_hashed(
            &val!({"headers": {"x-sig": PLAIN, "accept": "text/plain"}}),
            &hashes,
        );
        let none = AHashSet::new();
        let mask = RecordMask::new(&registry, &none);
        assert_eq!(mask.mask_text(&format!("v1={PLAIN}")), "v1=<secret>");
        assert_eq!(mask.mask_text("text/plain"), "text/plain");
    }

    #[test]
    fn registers_leaves_and_what_a_value_wraps() {
        let registry = SecretValues::default();
        registry.register(&val!({"nested": [{"key": PLAIN}]}));
        registry.register(&Val::from("Bearer wrapped-credential"));
        registry.register(&Val::from(r#"{"private_key": "pem-body-of-a-key"}"#));
        let hashes = AHashSet::new();
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(mask.mask_text(&format!("x{PLAIN}x")), "x<secret>x");
        assert_eq!(mask.mask_text("wrapped-credential"), MASK);
        assert_eq!(mask.mask_text("pem-body-of-a-key"), MASK);
    }

    #[test]
    fn numbers_long_enough_register_as_text() {
        let registry = SecretValues::default();
        registry.register(&Val::Int(1234567890));
        registry.register(&Val::Int(42));
        let hashes = AHashSet::new();
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(mask.mask_text("pin=1234567890;"), "pin=<secret>;");
        assert_eq!(mask.mask_text("count=42"), "count=42");
    }

    #[test]
    fn overlapping_secrets_mask_their_union() {
        let masked = masked_text(&["abcdefghij", "ghijklmnop"], "xabcdefghijklmnopx");
        assert_eq!(masked, "x<secret>x");
    }

    #[test]
    fn registering_is_idempotent_and_generations_only_count_new_values() {
        let registry = SecretValues::default();
        registry.register(&Val::from(PLAIN));
        let generation = registry.generation();
        registry.register(&Val::from(PLAIN));
        assert_eq!(registry.generation(), generation);
        registry.register(&Val::from("another-secret-value"));
        assert_ne!(registry.generation(), generation);
    }

    #[test]
    fn clones_share_what_is_registered_but_debug_and_serde_do_not_show_it() {
        let registry = SecretValues::default();
        let clone = registry.clone();
        clone.register(&Val::from(PLAIN));
        assert!(!is_empty(&registry));
        let printed = format!("{registry:?}");
        assert!(!printed.contains(PLAIN), "{printed}");
    }

    #[test]
    fn recognizes_credential_fields() {
        for name in [
            "Authorization",
            "authorization",
            "PROXY-AUTHORIZATION",
            "Cookie",
            "set-cookie",
            "Set-Cookie",
            "X-Api-Key",
            "x-api-key",
            "api_key",
            "apiKey",
            "ApiKey",
            "APIKEY",
            "stripe.api.key",
            "x-goog-api-key",
            "X-Auth-Token",
            "X-Amz-Security-Token",
            "access_token",
            "accessToken",
            "refresh-token",
            "id_token",
            "client_secret",
            "clientSecret",
            "webhook_secret",
            "secret",
            "password",
            "db_password",
            "aws_secret_access_key",
            "private_key",
            "code_verifier",
            "credentials",
        ] {
            assert!(is_credential_field(name), "{name}");
        }
        for name in [
            "token",
            "next_page_token",
            "max_tokens",
            "input_tokens",
            "tokens",
            "valid_token",
            "paidToken",
            "key",
            "code",
            "session_id",
            "password_hint",
            "password_changed_at",
            "cookies_accepted",
            "secret_name",
            "api_version",
            "access_key_id",
            "aws_access_key_id",
            "authorized",
            "message",
        ] {
            assert!(!is_credential_field(name), "{name}");
        }
    }

    fn mask_plain(value: Val) -> Val {
        let registry = SecretValues::default();
        let hashes = AHashSet::new();
        RecordMask::new(&registry, &hashes).mask_val(value)
    }

    #[test]
    fn redacts_fields_without_anything_registered() {
        let masked = mask_plain(val!({
            "headers": {
                "Authorization": "Bearer unregistered",
                "X-Api-Key": "unregistered",
                "Set-Cookie": ["a=1", "b=2"],
                "Accept": "application/json",
            },
            "access_token": "unregistered",
            "token": "kept",
            "max_tokens": 100,
            "password": Val::Null,
            "secret": false,
            "api_key": "",
            "credentials": {"user": "unregistered"},
        }));
        assert_eq!(
            masked,
            val!({
                "headers": {
                    "Authorization": MASK,
                    "X-Api-Key": MASK,
                    "Set-Cookie": MASK,
                    "Accept": "application/json",
                },
                "access_token": MASK,
                "token": "kept",
                "max_tokens": 100,
                "password": Val::Null,
                "secret": false,
                "api_key": "",
                "credentials": MASK,
            })
        );
    }

    #[test]
    fn redacts_fields_in_json_bodies() {
        let body = r#"{"access_token":"unregistered","expires_in":3600,"nested":{"refresh_token":"unregistered"},"token_type":"bearer"}"#;
        let masked = mask_plain(Val::from(body));
        let Val::Str(masked) = masked else {
            panic!("expected a string");
        };
        assert!(!masked.contains("unregistered"), "{masked}");
        assert!(masked.contains("expires_in"), "{masked}");
        assert!(masked.contains("\"token_type\":\"bearer\""), "{masked}");
    }

    #[test]
    fn redacts_fields_in_text_that_is_not_json() {
        for (text, expected) in [
            (
                "client_secret=abc&grant_type=client_credentials",
                "client_secret=<secret>&grant_type=client_credentials",
            ),
            (
                "https://host/path?api_key=abc&page=2",
                "https://host/path?api_key=<secret>&page=2",
            ),
            (
                "grant_type=refresh_token&refresh_token=abc",
                "grant_type=refresh_token&refresh_token=<secret>",
            ),
            (
                r#"HTTP 400: {"access_token":"abc","x":1"#,
                r#"HTTP 400: {"access_token":"<secret>","x":1"#,
            ),
            (
                r#"data: {\"password\":\"abc\"}"#,
                r#"data: {\"password\":\"<secret>\"}"#,
            ),
            (
                "Authorization: Bearer abc\r\nAccept: */*",
                "Authorization: <secret>\r\nAccept: */*",
            ),
            ("Cookie: a=1; b=2", "Cookie: <secret>"),
            (
                "postgres://user:pa55@db.example.com/x",
                "postgres://user:<secret>@db.example.com/x",
            ),
        ] {
            assert_eq!(masked_text(&[], text), expected, "input {text}");
        }
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for text in [
            "grant_type=refresh_token",
            "the password is required",
            "token=kept&page=2",
            "password: null",
            "https://example.com/users/bob@example.com",
            "see the cookie policy",
            "input_tokens: 512",
        ] {
            assert_eq!(masked_text(&[], text), text);
        }
    }

    #[test]
    fn a_secret_with_field_delimiters_goes_whole() {
        let secret = "has space&and=more";
        assert_eq!(
            masked_text(&[secret], &format!("client_secret={secret}&page=2")),
            "client_secret=<secret>&page=2"
        );
    }

    #[test]
    fn masks_map_keys_that_hold_secrets() {
        let (registry, hashes) = mask_for(&[PLAIN]);
        let masked = RecordMask::new(&registry, &hashes).mask_val(val!({PLAIN: "x"}));
        assert_eq!(masked, val!({MASK: "x"}));
    }

    #[test]
    fn masks_bytes_holding_a_secret() {
        let (registry, hashes) = mask_for(&[PLAIN]);
        let mask = RecordMask::new(&registry, &hashes);
        let mut body = b"\xff\xfeprefix".to_vec();
        body.extend_from_slice(PLAIN.as_bytes());
        assert_eq!(mask.mask_val(Val::Bytes(body)), Val::from(MASK));
        assert_eq!(
            mask.mask_val(Val::Bytes(b"other".to_vec())),
            Val::Bytes(b"other".to_vec())
        );
    }

    #[test]
    fn masks_bytes_holding_a_credential_field() {
        assert_eq!(
            mask_plain(Val::Bytes(br#"{"access_token":"unregistered"}"#.to_vec())),
            Val::from(MASK)
        );
        assert_eq!(
            mask_plain(Val::Bytes(b"plain body".to_vec())),
            Val::Bytes(b"plain body".to_vec())
        );
    }

    #[test]
    fn delivered_payloads_lose_secrets_but_keep_fields() {
        let (registry, hashes) = mask_for(&[PLAIN]);
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(
            mask.mask_delivered(val!({"access_token": "issued", "note": format!("k {PLAIN}")})),
            val!({"access_token": "issued", "note": "k <secret>"})
        );
    }

    #[test]
    fn json_payloads_without_credentials_are_not_rewritten() {
        let registry = SecretValues::default();
        let hashes = AHashSet::new();
        let mask = RecordMask::new(&registry, &hashes);
        assert_eq!(mask.mask_json(r#"{"a": 1, "max_tokens": 5}"#), None);
        let bytes = serde_json::to_string(&Val::Bytes(b"password=abc".to_vec())).unwrap();
        assert_eq!(mask.mask_json(&bytes).unwrap(), format!("\"{MASK}\""));
        assert!(
            mask.mask_json(r#"{"password": "x"}"#)
                .unwrap()
                .contains(MASK)
        );
        let too_deep = format!("{}\"password\"{}", "[".repeat(1000), "]".repeat(1000));
        assert_eq!(mask.mask_json(&too_deep).unwrap(), format!("\"{MASK}\""));
    }

    #[test]
    fn matches_a_large_payload_quickly() {
        let (registry, hashes) = mask_for(&[PLAIN, SECRET, "another-secret-value"]);
        let mask = RecordMask::new(&registry, &hashes);
        let mut payload = "lorem ipsum dolor sit amet ".repeat(400_000);
        payload.push_str(PLAIN);
        let started = std::time::Instant::now();
        let masked = mask.mask_text(&payload);
        assert!(masked.ends_with(MASK));
        // Far above what it takes in practice, even in a debug build: this
        // catches a matcher that went quadratic, not a slow machine.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }
}
