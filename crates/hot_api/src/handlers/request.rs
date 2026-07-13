//! Shared request building and secret hashing for MCP and webhook handlers.
//!
//! Both MCP tools and webhook endpoints construct an `::hot::http/HttpRequest` value
//! that is injected as the `hot.request` context variable. This module provides the
//! unified builder and secret-masking helpers.

use ahash::AHashSet;
use axum::http::HeaderMap;
use hot::val::Val;
use std::collections::HashMap;
use uuid::Uuid;

use crate::auth::AuthContext;

/// Headers whose values are always treated as secrets and masked in logs.
/// Lowercased to match axum's HeaderMap key normalization.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authorization",
    "set-cookie",
];

/// Compute secret value hashes for only the sensitive parts of a request Val.
///
/// Instead of hashing every leaf (which masks innocuous values like "POST"),
/// this only hashes:
/// - The entire `auth` subtree (API key details, service key metadata, etc.)
/// - Values of headers in SENSITIVE_HEADERS or the user-declared `extra_secret_headers`
pub fn hash_sensitive_request_fields(
    request_val: &Val,
    extra_secret_headers: &[String],
) -> AHashSet<u64> {
    use hot::lang::hot::ctx::hash_secret_value_recursive;

    let mut hashes = AHashSet::new();

    let Val::Map(request_map) = request_val else {
        return hashes;
    };

    // Hash the entire auth subtree
    if let Some(auth_val) = request_map.get(&Val::from("auth")) {
        hash_secret_value_recursive(auth_val, &mut hashes);
    }

    // The original-url carries the webhook capability token verbatim
    // (URL-signing providers hash the exact configured URL); mask it in
    // run logs like any other secret.
    if let Some(original_url) = request_map.get(&Val::from(hot::webhook_url::ORIGINAL_URL_KEY)) {
        hash_secret_value_recursive(original_url, &mut hashes);
    }

    // Hash only sensitive header values
    if let Some(Val::Map(headers_map)) = request_map.get(&Val::from("headers")) {
        for (key, value) in headers_map.iter() {
            if let Val::Str(key_str) = key {
                let k = key_str.as_ref();
                if SENSITIVE_HEADERS.contains(&k)
                    || extra_secret_headers
                        .iter()
                        .any(|h| h.eq_ignore_ascii_case(k))
                {
                    hash_secret_value_recursive(value, &mut hashes);
                }
            }
        }
    }

    hashes
}

/// Build the event data Val for a hot:call event.
///
/// The structure must match what `call-event-handler` in hot-std expects:
///   `event.data.fn` - the fully qualified function name
///   `event.data.args` - a Vec of positional arguments
///   `event.data.caller` - (optional) caller identity for `hot.request` ctx injection
pub fn build_call_event_data(function_name: &str, args: Val, caller: Option<Val>) -> Val {
    let mut event_data = hot::val!({
        "fn": function_name.to_string(),
        "args": args
    });
    if let Some(caller_val) = caller
        && let Val::Map(ref mut map) = event_data
    {
        map.insert(Val::from("caller"), caller_val);
    }
    event_data
}

/// Read a proxy/forwarding header and return the first comma-separated value,
/// trimmed, or None when the header is absent or empty. Multi-proxy chains
/// append values; the first is the client-facing one.
pub fn first_forwarded_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Optional body content for the request builder.
/// Webhooks provide body/body-raw; MCP does not.
/// `body_bytes` carries the verbatim wire bytes only when they are not valid
/// UTF-8 — in that case `body_raw` is lossy and signature verification needs
/// the real bytes.
pub struct RequestBody {
    pub body: Val,
    pub body_raw: String,
    pub body_bytes: Option<Vec<u8>>,
}

/// Build an `::hot::http/HttpRequest` typed Val from HTTP request components.
///
/// Used by both MCP and webhook handlers to construct a unified request value.
/// The result is injected as the `hot.request` context variable (via the `caller`
/// field in event data) and, for webhooks, also used as the function argument.
///
/// Structure:
/// ```text
/// {
///   $type: "::hot::http/HttpRequest",
///   method: "POST",
///   url: "/mcp/org/env/service",
///   original-url: "https://...", // webhooks only: the URL as the caller requested it,
///                                // pre-rewrite, token and query intact (signature verification)
///   headers: { "content-type": "application/json", ... },
///   query: { ... },
///   body: { ... },            // webhooks only
///   body-raw: "...",          // webhooks only
///   body-bytes: Bytes,        // webhooks only, and only when the body is not valid UTF-8
///   ip: "1.2.3.4",           // from proxy headers
///   auth: {                   // present only if authenticated
///     type: "service-key" | "api-key" | "session",
///     service-key: { id: "...", name: "...", meta: { ... } }
///   }
/// }
/// ```
#[allow(clippy::too_many_arguments)]
pub fn build_request_val(
    method: &str,
    url_path: &str,
    original_url: Option<&str>,
    headers: &HeaderMap,
    query_params: &HashMap<String, String>,
    body: Option<RequestBody>,
    auth: Option<&(AuthContext, hot::db::api_key::ApiKey)>,
    org_id: &Uuid,
) -> Val {
    let mut request_val = hot::val!({
        "$type": "::hot::http/HttpRequest",
        "method": method.to_string(),
        "url": url_path.to_string()
    });

    let Val::Map(ref mut request_map) = request_val else {
        return request_val;
    };

    // Providers like Twilio and HubSpot sign the exact URL they were configured
    // with; `url` is a rewritten internal path, so verifiers need this instead.
    if let Some(original_url) = original_url {
        request_map.insert(
            Val::from("original-url"),
            Val::from(original_url.to_string()),
        );
    }

    // Headers (keys are already lowercase in axum's HeaderMap)
    let mut headers_val = hot::val!({});
    if let Val::Map(ref mut headers_map) = headers_val {
        for (key, value) in headers.iter() {
            if let Ok(v) = value.to_str() {
                headers_map.insert(
                    Val::from(key.as_str().to_string()),
                    Val::from(v.to_string()),
                );
            }
        }
    }
    request_map.insert(Val::from("headers"), headers_val);

    // Query params
    let mut query_val = hot::val!({});
    if let Val::Map(ref mut query_map) = query_val {
        for (key, value) in query_params.iter() {
            query_map.insert(Val::from(key.clone()), Val::from(value.clone()));
        }
    }
    request_map.insert(Val::from("query"), query_val);

    // Body (webhooks only)
    if let Some(request_body) = body {
        request_map.insert(Val::from("body"), request_body.body);
        request_map.insert(Val::from("body-raw"), Val::from(request_body.body_raw));
        if let Some(bytes) = request_body.body_bytes {
            request_map.insert(Val::from("body-bytes"), Val::Bytes(bytes));
        }
    }

    // Client IP from proxy headers
    let ip = first_forwarded_value(headers, "x-forwarded-for")
        .or_else(|| first_forwarded_value(headers, "x-real-ip"));
    if let Some(ip) = ip {
        request_map.insert(Val::from("ip"), Val::from(ip));
    }

    // Auth context (only if authenticated)
    if let Some((auth_ctx, _)) = auth {
        let auth_type = match auth_ctx {
            AuthContext::ApiKey(_) => "api-key",
            AuthContext::Session { .. } => "session",
            AuthContext::ServiceKey { .. } => "service-key",
        };

        let mut auth_val = hot::val!({
            "type": auth_type.to_string()
        });

        if let AuthContext::ServiceKey { service_key, .. } = auth_ctx {
            let mut ck_val = hot::val!({
                "id": service_key.service_key_id.to_string()
            });
            if let Some(ref name) = service_key.name
                && let Val::Map(ref mut ck_map) = ck_val
            {
                ck_map.insert(Val::from("name"), Val::from(name.clone()));
            }

            if let Ok(encryption) =
                hot::context_encryption::ContextEncryption::from_env_or_existing_dev_key()
                && let Ok(Some(meta)) = service_key.get_decrypted_metadata(&encryption, org_id)
            {
                let meta_val: Val = serde_json::from_value(meta).unwrap_or(Val::Null);
                if let Val::Map(ref mut ck_map) = ck_val {
                    ck_map.insert(Val::from("meta"), meta_val);
                }
            }

            if let Val::Map(ref mut auth_map) = auth_val {
                auth_map.insert(Val::from("service-key"), ck_val);
            }
        }

        request_map.insert(Val::from("auth"), auth_val);
    }

    request_val
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use hot::val::Val;
    use std::collections::HashMap;

    fn simple_request(headers: &HeaderMap, url: &str) -> Val {
        build_request_val(
            "POST",
            url,
            None,
            headers,
            &HashMap::new(),
            None,
            None,
            &Uuid::nil(),
        )
    }

    #[test]
    fn test_build_request_val_has_type_field() {
        let val = simple_request(&HeaderMap::new(), "/test");
        if let Val::Map(ref map) = val {
            assert_eq!(
                map.get(&Val::from("$type")),
                Some(&Val::from("::hot::http/HttpRequest")),
                "Must include fully qualified $type"
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_build_request_val_body_bytes() {
        let raw = vec![0xFF, 0x00, 0xFE];
        let body = RequestBody {
            body: Val::from(String::from_utf8_lossy(&raw).to_string()),
            body_raw: String::from_utf8_lossy(&raw).to_string(),
            body_bytes: Some(raw.clone()),
        };
        let val = build_request_val(
            "POST",
            "/test",
            None,
            &HeaderMap::new(),
            &HashMap::new(),
            Some(body),
            None,
            &Uuid::nil(),
        );
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("body-bytes")), Some(&Val::Bytes(raw)));
        } else {
            panic!("Expected Map");
        }

        // Absent for valid UTF-8 bodies
        let body = RequestBody {
            body: Val::from("hello"),
            body_raw: "hello".to_string(),
            body_bytes: None,
        };
        let val = build_request_val(
            "POST",
            "/test",
            None,
            &HeaderMap::new(),
            &HashMap::new(),
            Some(body),
            None,
            &Uuid::nil(),
        );
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("body-bytes")), None);
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_build_request_val_original_url() {
        let full = "https://api.hot.dev/webhook/org/env/svc/path/abcdef123456?x=1";
        let val = build_request_val(
            "POST",
            "/webhook/org/env/svc/path",
            Some(full),
            &HeaderMap::new(),
            &HashMap::new(),
            None,
            None,
            &Uuid::nil(),
        );
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("original-url")), Some(&Val::from(full)));
        } else {
            panic!("Expected Map");
        }

        // Absent when the ingress couldn't reconstruct it (never an empty string)
        let val = simple_request(&HeaderMap::new(), "/test");
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("original-url")), None);
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_build_request_val_with_body() {
        let headers = HeaderMap::new();
        let body = RequestBody {
            body: Val::from("hello"),
            body_raw: "hello".to_string(),
            body_bytes: None,
        };
        let val = build_request_val(
            "POST",
            "/webhook/org/env/svc/path",
            None,
            &headers,
            &HashMap::new(),
            Some(body),
            None,
            &Uuid::nil(),
        );
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("body")), Some(&Val::from("hello")));
            assert_eq!(map.get(&Val::from("body-raw")), Some(&Val::from("hello")));
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_build_request_val_without_body() {
        let val = simple_request(&HeaderMap::new(), "/mcp/org/env/svc");
        if let Val::Map(ref map) = val {
            assert!(
                map.get(&Val::from("body")).is_none(),
                "No body for MCP-style request"
            );
            assert!(
                map.get(&Val::from("body-raw")).is_none(),
                "No body-raw for MCP-style request"
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_build_request_val_query_params() {
        let mut query = HashMap::new();
        query.insert("status".to_string(), "active".to_string());
        query.insert("page".to_string(), "2".to_string());

        let val = build_request_val(
            "GET",
            "/webhook/org/env/svc/path",
            None,
            &HeaderMap::new(),
            &query,
            None,
            None,
            &Uuid::nil(),
        );
        if let Val::Map(ref map) = val {
            assert_eq!(map.get(&Val::from("method")), Some(&Val::from("GET")));
            if let Some(Val::Map(q)) = map.get(&Val::from("query")) {
                assert_eq!(q.get(&Val::from("status")), Some(&Val::from("active")));
                assert_eq!(q.get(&Val::from("page")), Some(&Val::from("2")));
            } else {
                panic!("Expected query to be a Map");
            }
        }
    }

    #[test]
    fn test_build_request_val_ip_extraction() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());

        let val = simple_request(&headers, "/test");
        if let Val::Map(ref map) = val {
            assert_eq!(
                map.get(&Val::from("ip")),
                Some(&Val::from("1.2.3.4")),
                "Should use first IP from x-forwarded-for"
            );
        }
    }

    #[test]
    fn test_build_call_event_data_with_caller() {
        let args = Val::Vec(vec![Val::from("arg1")]);
        let caller = Val::from("caller-val");
        let event_data = build_call_event_data("::ns/fn", args, Some(caller));

        if let Val::Map(ref map) = event_data {
            assert!(map.get(&Val::from("fn")).is_some());
            assert!(map.get(&Val::from("args")).is_some());
            assert_eq!(
                map.get(&Val::from("caller")),
                Some(&Val::from("caller-val")),
                "Caller should be in event data"
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_build_call_event_data_without_caller() {
        let args = Val::Vec(vec![]);
        let event_data = build_call_event_data("::ns/fn", args, None);

        if let Val::Map(ref map) = event_data {
            assert!(
                map.get(&Val::from("caller")).is_none(),
                "No caller key when None"
            );
        }
    }

    #[test]
    fn test_hash_sensitive_body_not_hashed() {
        let headers = HeaderMap::new();
        let body = RequestBody {
            body: Val::from("secret-body-content"),
            body_raw: "secret-body-content".to_string(),
            body_bytes: None,
        };
        let val = build_request_val(
            "POST",
            "/test",
            None,
            &headers,
            &HashMap::new(),
            Some(body),
            None,
            &Uuid::nil(),
        );
        let hashes = hash_sensitive_request_fields(&val, &[]);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Val::from("secret-body-content").hash(&mut hasher);
        assert!(
            !hashes.contains(&hasher.finish()),
            "Body content should not be hashed as a secret"
        );
    }
}
