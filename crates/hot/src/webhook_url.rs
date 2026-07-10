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

/// Key under which webhook requests carry the caller-requested URL.
pub const ORIGINAL_URL_KEY: &str = "original-url";

/// Placeholder for the token path segment in persisted copies.
pub const REDACTED_SEGMENT: &str = "[redacted]";

/// Replace the last path segment of `url` (the webhook capability token)
/// with `[redacted]`, preserving scheme, host, the rest of the path, and the
/// query string. URLs without a path are returned unchanged. Idempotent.
pub fn redact_url_token(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };

    // The first path slash sits after "scheme://host"; a slash at or before
    // that boundary means there is no path to redact.
    let path_start = base.find("://").map(|i| i + 3).unwrap_or(0);
    let redacted = match base[path_start..].find('/') {
        Some(rel) => {
            let first_slash = path_start + rel;
            match base.rfind('/') {
                Some(pos) if pos >= first_slash => {
                    format!("{}/{}", &base[..pos], REDACTED_SEGMENT)
                }
                _ => base.to_string(),
            }
        }
        None => base.to_string(),
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
    if let Some(Val::Vec(args)) = map.get_mut(&Val::from("args")) {
        for arg in args.iter_mut() {
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
        && let Some(value) = map.get_mut(&Val::from(ORIGINAL_URL_KEY))
        && let Val::Str(url) = &*value
    {
        *value = Val::from(redact_url_token(url));
    }
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
    if let Some(Val::Vec(args)) = map.get_mut(&Val::from("args")) {
        for (i, arg) in args.iter_mut().enumerate() {
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

    match raw {
        Some(url) => {
            map.insert(key, Val::from(url));
        }
        None => {
            map.shift_remove(&key);
        }
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
    }

    fn call_event(url: Option<&str>) -> Val {
        let mut request = crate::val!({
            "$type": "::hot::http/HttpRequest",
            "method": "POST",
            "url": "/webhook/twilio/sms"
        });
        if let (Val::Map(map), Some(url)) = (&mut request, url) {
            map.insert(Val::from(ORIGINAL_URL_KEY), Val::from(url));
        }
        crate::val!({
            "fn": "::app/handle",
            "args": Val::Vec(vec![request.clone()]),
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
}
