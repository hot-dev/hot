//! Redaction helpers for secret-bearing webhook URLs.
//!
//! Webhook routes end in a capability token, and the `original-url` field on
//! webhook HttpRequests carries that token verbatim because URL-signing
//! providers (Twilio, HubSpot) hash the exact URL they were configured with.
//! The event row persisted to the database must not retain the token: the
//! event pipeline stores a token-redacted copy, the queue envelope carries
//! the raw value, and the worker re-attaches it at delivery time
//! (`restore_event_data_original_urls`). A delivery path that cannot
//! corroborate the raw value drops the field entirely so webhook verifiers
//! fail closed on their documented missing-URL path instead of hashing a
//! placeholder.

use crate::val::Val;
use std::sync::Arc;

/// Key under which webhook requests carry the caller-requested URL.
pub const ORIGINAL_URL_KEY: &str = "original-url";

/// Placeholder for the token path segment in persisted copies.
pub const REDACTED_SEGMENT: &str = "[redacted]";

/// Replace the last path segment of `url` (the webhook capability token)
/// with `[redacted]`, preserving scheme, host, the rest of the path, and the
/// query string. URLs without a path are returned unchanged. Idempotent.
pub fn redact_url_token(url: &str) -> String {
    // Only treat `?` as a query delimiter once we are past `scheme://authority`
    // (or past the first path `/` when an untrusted authority contained `?`,
    // such as `https://evil.com?/webhook/svc/secret`).
    let after_scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
    let first_slash = url[after_scheme..].find('/').map(|rel| after_scheme + rel);
    let query_search_start = first_slash.unwrap_or(after_scheme);
    let (base, query) = match url[query_search_start..].find('?') {
        Some(rel) => {
            let q = query_search_start + rel;
            (&url[..q], Some(&url[q + 1..]))
        }
        None => (url, None),
    };

    // The first path slash sits after "scheme://host"; a slash at or before
    // that boundary means there is no path to redact.
    let redacted = match first_slash {
        Some(first_slash) if first_slash < base.len() => {
            let trimmed = base.trim_end_matches('/');
            let trailing = &base[trimmed.len()..];
            if trimmed.len() <= first_slash {
                base.to_string()
            } else {
                match trimmed.rfind('/') {
                    Some(pos) if pos >= first_slash => {
                        format!("{}/{}{}", &trimmed[..pos], REDACTED_SEGMENT, trailing)
                    }
                    _ => base.to_string(),
                }
            }
        }
        _ => base.to_string(),
    };

    match query {
        Some(query) => format!("{}?{}", redacted, query),
        None => redacted,
    }
}

fn is_redacted(url: &str) -> bool {
    redact_url_token(url) == url
}

/// Clone `event_data` (a `hot:call` payload: `{fn, args, caller}`), replacing
/// every `original-url` in the call args and the caller with its
/// token-redacted form. This is the copy that may be persisted or displayed.
pub fn redact_event_data_original_urls(event_data: &Val) -> Val {
    let mut out = event_data.clone();
    let Val::Map(map) = &mut out else {
        return out;
    };
    let map = Arc::make_mut(map);
    if let Some(Val::Vec(args)) = map.get_mut(&Val::from("args")) {
        for arg in Arc::make_mut(args).iter_mut() {
            redact_request_original_url(arg);
        }
    }
    if let Some(caller) = map.get_mut(&Val::from("caller")) {
        redact_request_original_url(caller);
    }
    out
}

fn redact_request_original_url(request: &mut Val) {
    if let Val::Map(map) = request
        && let Some(value) = Arc::make_mut(map).get_mut(&Val::from(ORIGINAL_URL_KEY))
        && let Val::Str(url) = &*value
    {
        *value = Val::from(redact_url_token(url));
    }
}

/// Keep only raw URL hints needed to restore a DB-hydrated webhook request.
/// The queue must not copy request bodies, headers, auth, or other event data.
/// Argument positions are retained so a hint cannot move between call slots.
pub(crate) fn event_data_original_url_hints(event_data: &Val) -> Option<Val> {
    let request_hint = |request: &Val| match map_get(request, ORIGINAL_URL_KEY) {
        Some(Val::Str(url)) if !is_redacted(url) => {
            Some(crate::val!({ "original-url": url.to_string() }))
        }
        _ => None,
    };
    let mut hints = crate::val!({});
    let Val::Map(map) = &mut hints else {
        unreachable!();
    };
    let map = Arc::make_mut(map);
    if let Some(Val::Vec(args)) = map_get(event_data, "args") {
        let args: Vec<Val> = args
            .iter()
            .map(|arg| request_hint(arg).unwrap_or(Val::Null))
            .collect();
        if args.iter().any(|arg| !matches!(arg, Val::Null)) {
            map.insert(Val::from("args"), Val::vec(args));
        }
    }
    if let Some(caller) = map_get(event_data, "caller").and_then(request_hint) {
        map.insert(Val::from("caller"), caller);
    }
    if map.is_empty() { None } else { Some(hints) }
}

/// Re-attach raw `original-url` values from the queue envelope onto
/// DB-hydrated event data.
///
/// Only slots whose hydrated value is redacted are touched, and an envelope
/// value is accepted only if it redacts to the stored value — i.e. only the
/// token segment may differ, so a spoofed envelope cannot rewrite the URL's
/// host or path (and a wrong token merely fails the provider signature
/// check). Slots the envelope cannot corroborate are removed.
pub fn restore_event_data_original_urls(hydrated: &mut Val, envelope: &Val) {
    let Val::Map(map) = hydrated else {
        return;
    };
    let map = Arc::make_mut(map);
    if let Some(Val::Vec(args)) = map.get_mut(&Val::from("args")) {
        for (i, arg) in Arc::make_mut(args).iter_mut().enumerate() {
            let envelope_arg = match map_get(envelope, "args") {
                Some(Val::Vec(envelope_args)) => envelope_args.get(i),
                _ => None,
            };
            restore_request_original_url(arg, envelope_arg);
        }
    }
    if let Some(caller) = map.get_mut(&Val::from("caller")) {
        restore_request_original_url(caller, map_get(envelope, "caller"));
    }
}

fn map_get<'a>(value: &'a Val, key: &str) -> Option<&'a Val> {
    match value {
        Val::Map(map) => map.get(&Val::from(key)),
        _ => None,
    }
}

fn restore_request_original_url(request: &mut Val, envelope_request: Option<&Val>) {
    let Val::Map(map) = request else {
        return;
    };
    let key = Val::from(ORIGINAL_URL_KEY);
    let Some(Val::Str(stored)) = map.get(&key) else {
        return;
    };
    let stored = stored.to_string();
    if !is_redacted(&stored) {
        // Raw value already present (in-memory delivery); leave it alone.
        return;
    }

    let raw = envelope_request
        .and_then(|request| map_get(request, ORIGINAL_URL_KEY))
        .and_then(|value| match value {
            Val::Str(url) => Some(url.to_string()),
            _ => None,
        })
        .filter(|url| redact_url_token(url) == stored);

    let map = Arc::make_mut(map);
    match raw {
        Some(url) => {
            map.insert(key, Val::from(url));
        }
        None => {
            map.shift_remove(&key);
        }
    }
}

/// Internal event_data key carrying endpoint-declared `secret-headers` names
/// (never values) so API/UI event readers can redact custom secret headers
/// while leaving them intact in Postgres for `hot_worker` hydration.
pub const SECRET_HEADERS_META_KEY: &str = "_hot_secret_headers";

const DISPLAY_REDACTED_VALUE: &str = "[redacted]";

const DISPLAY_SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "cookie",
    "set-cookie",
];

/// Redact sensitive HTTP request headers, decrypted `auth.service-key.meta`,
/// and webhook capability tokens in a `serde_json::Value` `event_data` payload
/// before returning it from `/v1/events` or dashboard UI endpoints.
///
/// Leaves non-`HttpRequest` MCP tool arguments untouched and strips the
/// internal `_hot_secret_headers` helper key from the returned JSON.
pub fn redact_event_data_json_for_display(value: &mut serde_json::Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    let extra_secret_headers: Vec<String> = root
        .remove(SECRET_HEADERS_META_KEY)
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if let Some(args) = root.get_mut("args").and_then(|v| v.as_array_mut()) {
        for arg in args.iter_mut() {
            let is_http_req = arg
                .get("$type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t == "::hot::http/HttpRequest");
            if is_http_req {
                redact_request_json_for_display(arg, &extra_secret_headers);
            }
        }
    }
    if let Some(caller) = root.get_mut("caller") {
        redact_request_json_for_display(caller, &extra_secret_headers);
    }
}

fn redact_request_json_for_display(
    request: &mut serde_json::Value,
    extra_secret_headers: &[String],
) {
    let Some(req) = request.as_object_mut() else {
        return;
    };
    if let Some(url) = req.get(ORIGINAL_URL_KEY).and_then(|v| v.as_str()) {
        let redacted = redact_url_token(url);
        req.insert(
            ORIGINAL_URL_KEY.to_string(),
            serde_json::Value::String(redacted),
        );
    }
    if let Some(headers) = req.get_mut("headers").and_then(|v| v.as_object_mut()) {
        for (name, val) in headers.iter_mut() {
            let sensitive = DISPLAY_SENSITIVE_HEADERS
                .iter()
                .any(|h| h.eq_ignore_ascii_case(name))
                || extra_secret_headers
                    .iter()
                    .any(|h| h.eq_ignore_ascii_case(name));
            if sensitive {
                *val = serde_json::Value::String(DISPLAY_REDACTED_VALUE.to_string());
            }
        }
    }
    if let Some(service_key) = req
        .get_mut("auth")
        .and_then(|v| v.as_object_mut())
        .and_then(|auth| auth.get_mut("service-key"))
        .and_then(|v| v.as_object_mut())
    {
        service_key.remove("meta");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_url_token() {
        assert_eq!(
            redact_url_token("https://hooks.acme.dev/webhook/twilio/sms/tok_abc123"),
            "https://hooks.acme.dev/webhook/twilio/sms/[redacted]"
        );
        assert_eq!(
            redact_url_token("https://h.dev/webhook/t/sms/tok?bodySHA256=beef"),
            "https://h.dev/webhook/t/sms/[redacted]?bodySHA256=beef"
        );
        // Single-segment path.
        assert_eq!(
            redact_url_token("https://h.dev/tok"),
            "https://h.dev/[redacted]"
        );
        // No path: nothing to redact.
        assert_eq!(redact_url_token("https://h.dev"), "https://h.dev");
        // Idempotent.
        let once = redact_url_token("https://h.dev/a/b/tok");
        assert_eq!(redact_url_token(&once), once);
        // Trailing slashes still redact the token segment (never leak tok/[redacted]).
        assert_eq!(
            redact_url_token("https://h.dev/webhook/svc/tok/"),
            "https://h.dev/webhook/svc/[redacted]/"
        );
        assert_eq!(
            redact_url_token("https://h.dev/webhook/svc/tok///"),
            "https://h.dev/webhook/svc/[redacted]///"
        );
        assert_eq!(
            redact_url_token("https://h.dev/webhook/svc/tok/?sig=abc"),
            "https://h.dev/webhook/svc/[redacted]/?sig=abc"
        );
        assert_eq!(
            redact_url_token("https://h.dev/tok/"),
            "https://h.dev/[redacted]/"
        );
        // Root-only slash has no token segment.
        assert_eq!(redact_url_token("https://h.dev/"), "https://h.dev/");
        // Trailing-slash redaction is idempotent.
        let trailing_once = redact_url_token("https://h.dev/webhook/svc/tok/");
        assert_eq!(redact_url_token(&trailing_once), trailing_once);
    }

    fn call_event(url: Option<&str>) -> Val {
        let mut request = crate::val!({
            "$type": "::hot::http/HttpRequest",
            "method": "POST",
            "url": "/webhook/twilio/sms"
        });
        if let (Val::Map(map), Some(url)) = (&mut request, url) {
            Arc::make_mut(map).insert(Val::from(ORIGINAL_URL_KEY), Val::from(url));
        }
        crate::val!({
            "fn": "::app/handle",
            "args": Val::vec(vec![request.clone()]),
            "caller": request
        })
    }

    fn as_str(value: &Val) -> Option<String> {
        match value {
            Val::Str(s) => Some(s.to_string()),
            _ => None,
        }
    }

    fn original_urls(event_data: &Val) -> (Option<String>, Option<String>) {
        let arg = map_get(event_data, "args")
            .and_then(|args| match args {
                Val::Vec(v) => v.first(),
                _ => None,
            })
            .and_then(|r| map_get(r, ORIGINAL_URL_KEY))
            .and_then(as_str);
        let caller = map_get(event_data, "caller")
            .and_then(|r| map_get(r, ORIGINAL_URL_KEY))
            .and_then(as_str);
        (arg, caller)
    }

    #[test]
    fn test_redact_and_restore_roundtrip() {
        let raw = "https://hooks.acme.dev/webhook/twilio/sms/tok_secret";
        let envelope = call_event(Some(raw));
        let mut persisted = redact_event_data_original_urls(&envelope);

        let (arg, caller) = original_urls(&persisted);
        assert_eq!(
            arg.as_deref(),
            Some("https://hooks.acme.dev/webhook/twilio/sms/[redacted]")
        );
        assert_eq!(arg, caller);

        restore_event_data_original_urls(&mut persisted, &envelope);
        let (arg, caller) = original_urls(&persisted);
        assert_eq!(arg.as_deref(), Some(raw));
        assert_eq!(caller.as_deref(), Some(raw));
    }

    #[test]
    fn test_restore_drops_uncorroborated_slots() {
        let raw = "https://hooks.acme.dev/webhook/twilio/sms/tok_secret";
        let mut persisted = redact_event_data_original_urls(&call_event(Some(raw)));

        // Envelope without an original-url (e.g. replay reconstructed from
        // the DB): the placeholder must not reach handler code.
        restore_event_data_original_urls(&mut persisted, &call_event(None));
        let (arg, caller) = original_urls(&persisted);
        assert_eq!(arg, None);
        assert_eq!(caller, None);
    }

    #[test]
    fn test_restore_rejects_mismatched_envelope_url() {
        let raw = "https://hooks.acme.dev/webhook/twilio/sms/tok_secret";
        let mut persisted = redact_event_data_original_urls(&call_event(Some(raw)));

        // A spoofed envelope may only vary the token segment; a different
        // host/path must not be re-attached.
        restore_event_data_original_urls(
            &mut persisted,
            &call_event(Some("https://evil.dev/webhook/twilio/sms/tok_x")),
        );
        let (arg, _) = original_urls(&persisted);
        assert_eq!(arg, None);
    }

    #[test]
    fn test_restore_leaves_raw_values_untouched() {
        let raw = "https://hooks.acme.dev/webhook/twilio/sms/tok_secret";
        let mut in_memory = call_event(Some(raw));
        restore_event_data_original_urls(&mut in_memory, &call_event(None));
        let (arg, _) = original_urls(&in_memory);
        assert_eq!(arg.as_deref(), Some(raw));
    }

    #[test]
    fn test_redact_event_data_json_for_display_strips_headers_meta_and_custom_secrets() {
        let mut json = serde_json::json!({
            "fn": "::app/handle",
            "_hot_secret_headers": ["X-Stripe-Sig"],
            "args": [{
                "$type": "::hot::http/HttpRequest",
                "original-url": "https://hooks.acme.dev/webhook/svc/tok_abc/",
                "headers": {
                    "authorization": "Bearer secret_tok",
                    "x-stripe-sig": "t=1,v1=sig",
                    "content-type": "application/json"
                },
                "auth": {
                    "type": "service-key",
                    "service-key": {
                        "id": "sk_1",
                        "meta": { "api_key": "customer_secret" }
                    }
                }
            }, {
                "headers": { "authorization": "non-http-tool-arg" }
            }],
            "caller": {
                "headers": {
                    "cookie": "session=abc",
                    "x-stripe-sig": "t=1,v1=sig"
                },
                "auth": {
                    "type": "service-key",
                    "service-key": {
                        "id": "sk_1",
                        "meta": { "api_key": "customer_secret" }
                    }
                }
            }
        });

        redact_event_data_json_for_display(&mut json);
        assert!(json.get(SECRET_HEADERS_META_KEY).is_none());
        assert_eq!(
            json["args"][0]["original-url"],
            "https://hooks.acme.dev/webhook/svc/[redacted]/"
        );
        assert_eq!(json["args"][0]["headers"]["authorization"], "[redacted]");
        assert_eq!(json["args"][0]["headers"]["x-stripe-sig"], "[redacted]");
        assert_eq!(
            json["args"][0]["headers"]["content-type"],
            "application/json"
        );
        assert!(json["args"][0]["auth"]["service-key"].get("meta").is_none());
        assert_eq!(
            json["args"][1]["headers"]["authorization"],
            "non-http-tool-arg"
        );
        assert_eq!(json["caller"]["headers"]["cookie"], "[redacted]");
        assert_eq!(json["caller"]["headers"]["x-stripe-sig"], "[redacted]");
        assert!(json["caller"]["auth"]["service-key"].get("meta").is_none());
    }

    #[test]
    fn redacts_token_even_if_authority_contains_question_mark() {
        assert_eq!(
            redact_url_token("https://evil.com?/webhook/svc/abcdef123456"),
            "https://evil.com?/webhook/svc/[redacted]"
        );
        assert_eq!(
            redact_url_token("https://evil.com?/webhook/svc/abcdef123456?foo=bar"),
            "https://evil.com?/webhook/svc/[redacted]?foo=bar"
        );
    }
}
