use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::{validate_args, validate_args_range};
use indexmap::IndexMap;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use url::Url;

/// Characters that are NOT percent-encoded when encoding a URI component.
/// This encodes everything except unreserved characters (RFC 3986 Section 2.3):
/// ALPHA / DIGIT / "-" / "." / "_" / "~"
const URI_COMPONENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

// ---------------------------------------------------------------------------
// Uri type helpers
// ---------------------------------------------------------------------------

const URI_TYPE: &str = "::hot::uri/Uri";

/// Build a Uri typed map from parsed Url components
fn url_to_uri_map(parsed: &Url) -> Val {
    let mut map = IndexMap::new();
    map.insert(Val::from("$type"), Val::from(URI_TYPE));

    map.insert(Val::from("scheme"), Val::from(parsed.scheme()));

    // userinfo
    match parsed.username() {
        "" => {
            map.insert(Val::from("userinfo"), Val::Null);
        }
        user => {
            let userinfo = match parsed.password() {
                Some(pw) => format!("{}:{}", user, pw),
                None => user.to_string(),
            };
            map.insert(Val::from("userinfo"), Val::from(userinfo));
        }
    }

    // host
    match parsed.host_str() {
        Some(h) => {
            map.insert(Val::from("host"), Val::from(h));
        }
        None => {
            map.insert(Val::from("host"), Val::Null);
        }
    }

    // port
    match parsed.port() {
        Some(p) => {
            map.insert(Val::from("port"), Val::Int(p as i64));
        }
        None => {
            map.insert(Val::from("port"), Val::Null);
        }
    }

    // path
    map.insert(Val::from("path"), Val::from(parsed.path()));

    // query
    match parsed.query() {
        Some(q) => {
            map.insert(Val::from("query"), Val::from(q));
        }
        None => {
            map.insert(Val::from("query"), Val::Null);
        }
    }

    // fragment
    match parsed.fragment() {
        Some(f) => {
            map.insert(Val::from("fragment"), Val::from(f));
        }
        None => {
            map.insert(Val::from("fragment"), Val::Null);
        }
    }

    Val::Map(Box::new(map))
}

/// Reconstruct a URI string from a Uri typed map.
/// Used by the Uri -> Str coercion and by join.
fn uri_map_to_string(map: &IndexMap<Val, Val>) -> HotResult<Val> {
    let scheme = match map.get(&Val::from("scheme")) {
        Some(Val::Str(s)) => s.clone(),
        _ => return HotResult::Err(Val::from("Uri: scheme is required")),
    };

    let mut result = format!("{}:", scheme);

    let host = match map.get(&Val::from("host")) {
        Some(Val::Str(s)) => Some(s.clone()),
        _ => None,
    };

    if let Some(ref host) = host {
        result.push_str("//");

        // userinfo
        if let Some(Val::Str(userinfo)) = map.get(&Val::from("userinfo")) {
            result.push_str(userinfo);
            result.push('@');
        }

        result.push_str(host);

        // port
        if let Some(Val::Int(port)) = map.get(&Val::from("port")) {
            result.push(':');
            result.push_str(&port.to_string());
        }
    }

    // path — omit a lone "/" when there's a host (it's the default empty path,
    // not an intentional root path). This avoids the surprising round-trip where
    // Uri("https://example.com") -> Str produces "https://example.com/" and
    // then `${Str(uri)}/api` yields a double-slash "https://example.com//api".
    let query = map.get(&Val::from("query"));
    let fragment = map.get(&Val::from("fragment"));
    if let Some(Val::Str(path)) = map.get(&Val::from("path")) {
        let is_default_root = host.is_some()
            && &**path == "/"
            && !matches!(query, Some(Val::Str(_)))
            && !matches!(fragment, Some(Val::Str(_)));
        if !is_default_root {
            result.push_str(path);
        }
    }

    // query
    if let Some(Val::Str(query)) = map.get(&Val::from("query")) {
        result.push('?');
        result.push_str(query);
    }

    // fragment
    if let Some(Val::Str(fragment)) = map.get(&Val::from("fragment")) {
        result.push('#');
        result.push_str(fragment);
    }

    HotResult::Ok(Val::from(result))
}

// ---------------------------------------------------------------------------
// Public API functions
// ---------------------------------------------------------------------------

/// Uri constructor: parse a string into a Uri typed map.
///
/// Usage in Hot: Uri("https://example.com/path?key=val#frag")
pub fn uri_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/Uri", args, 1);

    let uri_str = match &args[0] {
        Val::Str(s) => s,
        Val::Map(m) => {
            // If already a Uri map, return as-is
            if let Some(Val::Str(t)) = m.get(&Val::from("$type"))
                && &**t == URI_TYPE
            {
                return HotResult::Ok(args[0].clone());
            }
            // If a plain map of components, build from it
            return build(args);
        }
        _ => {
            return HotResult::Err(Val::from("::hot::uri/Uri: expected a string or map"));
        }
    };

    match Url::parse(uri_str) {
        Ok(parsed) => HotResult::Ok(url_to_uri_map(&parsed)),
        Err(e) => HotResult::Err(Val::from(format!("::hot::uri/Uri: invalid URI: {}", e))),
    }
}

/// Uri -> Str coercion: reconstruct a URI string from a Uri typed map.
pub fn uri_to_str(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/Uri->Str", args, 1);

    match &args[0] {
        Val::Map(m) => {
            if let Some(Val::Str(t)) = m.get(&Val::from("$type"))
                && &**t == URI_TYPE
            {
                return uri_map_to_string(m);
            }
            HotResult::Err(Val::from("::hot::uri/Uri->Str: expected a Uri"))
        }
        _ => HotResult::Err(Val::from("::hot::uri/Uri->Str: expected a Uri")),
    }
}

/// Percent-encode a URI component string (RFC 3986).
///
/// Encodes everything except unreserved characters: A-Z a-z 0-9 - . _ ~
///
/// Usage in Hot: ::uri/encode("hello world") => "hello%20world"
pub fn encode(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/encode", args, 1);

    let value = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from("::hot::uri/encode: argument must be a string"));
        }
    };

    let encoded = utf8_percent_encode(value, URI_COMPONENT_ENCODE_SET).to_string();
    HotResult::Ok(Val::from(encoded))
}

/// Percent-decode a URI component string.
///
/// Usage in Hot: ::uri/decode("hello%20world") => "hello world"
pub fn decode(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/decode", args, 1);

    let value = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from("::hot::uri/decode: argument must be a string"));
        }
    };

    match percent_decode_str(value).decode_utf8() {
        Ok(decoded) => HotResult::Ok(Val::from(decoded.as_ref())),
        Err(e) => HotResult::Err(Val::from(format!(
            "::hot::uri/decode: invalid UTF-8: {}",
            e
        ))),
    }
}

/// Encode a Map into a query string (application/x-www-form-urlencoded).
///
/// Usage in Hot: ::uri/encode-query({city: "Salt Lake City", days: 3})
///            => "city=Salt+Lake+City&days=3"
pub fn encode_query(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/encode-query", args, 1);

    let map = match &args[0] {
        Val::Map(m) => m,
        _ => {
            return HotResult::Err(Val::from("::hot::uri/encode-query: argument must be a map"));
        }
    };

    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in map.iter() {
        let key_str = val_to_query_str(key);
        let val_str = val_to_query_str(value);
        serializer.append_pair(&key_str, &val_str);
    }

    let encoded = serializer.finish();
    HotResult::Ok(Val::from(encoded))
}

/// Decode a query string into a Map.
///
/// Usage in Hot: ::uri/decode-query("city=Salt+Lake+City&days=3")
///            => {city: "Salt Lake City", days: "3"}
pub fn decode_query(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/decode-query", args, 1);

    let query = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::uri/decode-query: argument must be a string",
            ));
        }
    };

    // Strip leading '?' if present
    let query_str = if let Some(stripped) = query.strip_prefix('?') {
        stripped
    } else {
        query.as_ref()
    };

    let mut map = IndexMap::new();
    for (key, value) in url::form_urlencoded::parse(query_str.as_bytes()) {
        map.insert(Val::from(key.as_ref()), Val::from(value.as_ref()));
    }

    HotResult::Ok(Val::Map(Box::new(map)))
}

/// Parse a URI string into a Uri typed map.
///
/// Usage in Hot: ::uri/parse("https://example.com:8080/path?q=1#top")
///            => Uri{scheme: "https", host: "example.com", port: 8080, ...}
pub fn parse(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/parse", args, 1);

    let uri_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from("::hot::uri/parse: argument must be a string"));
        }
    };

    match Url::parse(uri_str) {
        Ok(parsed) => HotResult::Ok(url_to_uri_map(&parsed)),
        Err(e) => HotResult::Err(Val::from(format!("::hot::uri/parse: invalid URI: {}", e))),
    }
}

/// Build a URI string from a map of components.
///
/// Usage in Hot: ::uri/build({scheme: "https", host: "example.com", path: "/users", query: "active=true"})
///            => "https://example.com/users?active=true"
///
/// Also accepts a `query` field as a Map, which will be form-encoded.
pub fn build(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/build", args, 1);

    let parts = match &args[0] {
        Val::Map(m) => m,
        _ => {
            return HotResult::Err(Val::from("::hot::uri/build: argument must be a map"));
        }
    };

    let scheme = match parts.get(&Val::from("scheme")) {
        Some(Val::Str(s)) => s.clone(),
        _ => {
            return HotResult::Err(Val::from("::hot::uri/build: scheme is required"));
        }
    };

    let mut result = format!("{}:", scheme);

    let host = match parts.get(&Val::from("host")) {
        Some(Val::Str(s)) => Some(s.clone()),
        _ => None,
    };

    if let Some(ref host) = host {
        result.push_str("//");

        // userinfo
        if let Some(Val::Str(userinfo)) = parts.get(&Val::from("userinfo")) {
            result.push_str(userinfo);
            result.push('@');
        }

        result.push_str(host);

        // port
        if let Some(Val::Int(port)) = parts.get(&Val::from("port")) {
            result.push(':');
            result.push_str(&port.to_string());
        }
    }

    // path
    match parts.get(&Val::from("path")) {
        Some(Val::Str(path)) => result.push_str(path),
        _ => {
            if host.is_some() {
                result.push('/');
            }
        }
    }

    // query — accept either a Str or a Map (which gets form-encoded)
    match parts.get(&Val::from("query")) {
        Some(Val::Str(query)) => {
            result.push('?');
            result.push_str(query);
        }
        Some(Val::Map(query_map)) => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in query_map.iter() {
                serializer.append_pair(&val_to_query_str(key), &val_to_query_str(value));
            }
            let encoded = serializer.finish();
            if !encoded.is_empty() {
                result.push('?');
                result.push_str(&encoded);
            }
        }
        _ => {}
    }

    // fragment
    if let Some(Val::Str(fragment)) = parts.get(&Val::from("fragment")) {
        result.push('#');
        result.push_str(fragment);
    }

    HotResult::Ok(Val::from(result))
}

/// Join/resolve URI segments against a base URI.
///
/// Usage in Hot: ::uri/join("https://api.example.com", "users", "123")
///            => "https://api.example.com/users/123"
///
/// With 2 args and a relative reference, resolves per RFC 3986:
///   ::uri/join("https://example.com/a/b", "../c") => "https://example.com/c"
pub fn join(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::uri/join", args, 1, 10);

    // Extract base as a String
    let base_string = match &args[0] {
        Val::Str(s) => s.to_string(),
        Val::Map(m) => {
            // Allow Uri typed map as base
            if let Some(Val::Str(t)) = m.get(&Val::from("$type")) {
                if &**t == URI_TYPE {
                    match uri_map_to_string(m) {
                        HotResult::Ok(Val::Str(s)) => s.to_string(),
                        HotResult::Ok(_) => {
                            return HotResult::Err(Val::from(
                                "::hot::uri/join: internal error converting Uri to string",
                            ));
                        }
                        HotResult::Err(e) => return HotResult::Err(e),
                    }
                } else {
                    return HotResult::Err(Val::from(
                        "::hot::uri/join: base must be a string or Uri",
                    ));
                }
            } else {
                return HotResult::Err(Val::from("::hot::uri/join: base must be a string or Uri"));
            }
        }
        _ => {
            return HotResult::Err(Val::from("::hot::uri/join: base must be a string or Uri"));
        }
    };

    if args.len() == 1 {
        return HotResult::Ok(Val::from(base_string));
    }

    let mut base = match Url::parse(&base_string) {
        Ok(u) => u,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::uri/join: invalid base URI: {}",
                e
            )));
        }
    };

    // Join each subsequent segment
    for arg in &args[1..] {
        let segment = match arg {
            Val::Str(s) => s.clone(),
            _ => {
                return HotResult::Err(Val::from("::hot::uri/join: all segments must be strings"));
            }
        };

        // Ensure the base path ends with '/' before appending a path segment
        // (unless the segment looks like a relative reference with ../ or ./)
        if !segment.starts_with("../") && !segment.starts_with("./") && !segment.contains(':') {
            // Simple path segment join — ensure trailing slash on base path
            let mut path = base.path().to_string();
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(segment.trim_start_matches('/'));
            base.set_path(&path);
        } else {
            // RFC 3986 relative resolution
            match base.join(&segment) {
                Ok(joined) => base = joined,
                Err(e) => {
                    return HotResult::Err(Val::from(format!(
                        "::hot::uri/join: failed to join '{}': {}",
                        segment, e
                    )));
                }
            }
        }
    }

    HotResult::Ok(Val::from(base.as_str()))
}

/// Check if a string is a valid URI.
///
/// Usage in Hot: ::uri/is-valid("https://example.com") => true
pub fn is_valid(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::uri/is-valid", args, 1);

    let uri_str = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Ok(Val::Bool(false)),
    };

    HotResult::Ok(Val::Bool(Url::parse(uri_str).is_ok()))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a Val to a string for use in query parameters
fn val_to_query_str(val: &Val) -> String {
    match val {
        Val::Str(s) => s.to_string(),
        Val::Int(i) => i.to_string(),
        Val::Dec(d) => d.to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Null => String::new(),
        other => format!("{}", other),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_basic() {
        let result = encode(&[Val::from("hello world")]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "hello%20world"),
            _ => panic!("Expected encoded string"),
        }
    }

    #[test]
    fn test_encode_special_chars() {
        let result = encode(&[Val::from("a=1&b=2")]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "a%3D1%26b%3D2"),
            _ => panic!("Expected encoded string"),
        }
    }

    #[test]
    fn test_encode_unreserved_not_encoded() {
        let result = encode(&[Val::from("hello-world_test.v2~ok")]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "hello-world_test.v2~ok"),
            _ => panic!("Expected unchanged string"),
        }
    }

    #[test]
    fn test_decode_basic() {
        let result = decode(&[Val::from("hello%20world")]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "hello world"),
            _ => panic!("Expected decoded string"),
        }
    }

    #[test]
    fn test_decode_plus_not_decoded() {
        // percent-decode does NOT treat + as space (that's form-urlencoded)
        let result = decode(&[Val::from("hello+world")]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "hello+world"),
            _ => panic!("Expected decoded string"),
        }
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = "hello world / foo@bar?baz=qux#frag";
        let encoded = encode(&[Val::from(original)]);
        let encoded_str = match &encoded {
            HotResult::Ok(Val::Str(s)) => s.clone(),
            _ => panic!("Expected encoded string"),
        };
        let decoded = decode(&[Val::from(encoded_str)]);
        match decoded {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, original),
            _ => panic!("Expected decoded string"),
        }
    }

    #[test]
    fn test_encode_query_basic() {
        let mut map = IndexMap::new();
        map.insert(Val::from("city"), Val::from("Salt Lake City"));
        map.insert(Val::from("days"), Val::Int(3));

        let result = encode_query(&[Val::Map(Box::new(map))]);
        match result {
            HotResult::Ok(Val::Str(s)) => {
                assert!(s.contains("city=Salt+Lake+City"));
                assert!(s.contains("days=3"));
            }
            _ => panic!("Expected encoded query string"),
        }
    }

    #[test]
    fn test_decode_query_basic() {
        let result = decode_query(&[Val::from("city=Salt+Lake+City&days=3")]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(
                    m.get(&Val::from("city")),
                    Some(&Val::from("Salt Lake City"))
                );
                assert_eq!(m.get(&Val::from("days")), Some(&Val::from("3")));
            }
            _ => panic!("Expected decoded map"),
        }
    }

    #[test]
    fn test_decode_query_strips_leading_question_mark() {
        let result = decode_query(&[Val::from("?foo=bar")]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("foo")), Some(&Val::from("bar")));
            }
            _ => panic!("Expected decoded map"),
        }
    }

    #[test]
    fn test_parse_full_url() {
        let result = parse(&[Val::from(
            "https://user:pass@example.com:8080/path?q=1#frag",
        )]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("scheme")), Some(&Val::from("https")));
                assert_eq!(m.get(&Val::from("userinfo")), Some(&Val::from("user:pass")));
                assert_eq!(m.get(&Val::from("host")), Some(&Val::from("example.com")));
                assert_eq!(m.get(&Val::from("port")), Some(&Val::Int(8080)));
                assert_eq!(m.get(&Val::from("path")), Some(&Val::from("/path")));
                assert_eq!(m.get(&Val::from("query")), Some(&Val::from("q=1")));
                assert_eq!(m.get(&Val::from("fragment")), Some(&Val::from("frag")));
            }
            _ => panic!("Expected parsed Uri map"),
        }
    }

    #[test]
    fn test_parse_simple_url() {
        let result = parse(&[Val::from("https://example.com")]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("scheme")), Some(&Val::from("https")));
                assert_eq!(m.get(&Val::from("host")), Some(&Val::from("example.com")));
                assert_eq!(m.get(&Val::from("port")), Some(&Val::Null));
                assert_eq!(m.get(&Val::from("query")), Some(&Val::Null));
                assert_eq!(m.get(&Val::from("fragment")), Some(&Val::Null));
            }
            _ => panic!("Expected parsed Uri map"),
        }
    }

    #[test]
    fn test_parse_mailto() {
        let result = parse(&[Val::from("mailto:alice@example.com")]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("scheme")), Some(&Val::from("mailto")));
                assert_eq!(
                    m.get(&Val::from("path")),
                    Some(&Val::from("alice@example.com"))
                );
                assert_eq!(m.get(&Val::from("host")), Some(&Val::Null));
            }
            _ => panic!("Expected parsed Uri map"),
        }
    }

    #[test]
    fn test_parse_invalid() {
        let result = parse(&[Val::from("://broken")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_build_basic() {
        let mut parts = IndexMap::new();
        parts.insert(Val::from("scheme"), Val::from("https"));
        parts.insert(Val::from("host"), Val::from("example.com"));
        parts.insert(Val::from("path"), Val::from("/users"));

        let result = build(&[Val::Map(Box::new(parts))]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "https://example.com/users"),
            _ => panic!("Expected built URI string"),
        }
    }

    #[test]
    fn test_build_with_query_map() {
        let mut query = IndexMap::new();
        query.insert(Val::from("active"), Val::Bool(true));
        query.insert(Val::from("page"), Val::Int(1));

        let mut parts = IndexMap::new();
        parts.insert(Val::from("scheme"), Val::from("https"));
        parts.insert(Val::from("host"), Val::from("example.com"));
        parts.insert(Val::from("path"), Val::from("/users"));
        parts.insert(Val::from("query"), Val::Map(Box::new(query)));

        let result = build(&[Val::Map(Box::new(parts))]);
        match result {
            HotResult::Ok(Val::Str(s)) => {
                assert!(s.starts_with("https://example.com/users?"));
                assert!(s.contains("active=true"));
                assert!(s.contains("page=1"));
            }
            _ => panic!("Expected built URI string"),
        }
    }

    #[test]
    fn test_build_with_port() {
        let mut parts = IndexMap::new();
        parts.insert(Val::from("scheme"), Val::from("https"));
        parts.insert(Val::from("host"), Val::from("example.com"));
        parts.insert(Val::from("port"), Val::Int(8080));
        parts.insert(Val::from("path"), Val::from("/api"));

        let result = build(&[Val::Map(Box::new(parts))]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "https://example.com:8080/api"),
            _ => panic!("Expected built URI string"),
        }
    }

    #[test]
    fn test_join_path_segments() {
        let result = join(&[
            Val::from("https://api.example.com"),
            Val::from("users"),
            Val::from("123"),
        ]);
        match result {
            HotResult::Ok(Val::Str(s)) => {
                assert_eq!(&*s, "https://api.example.com/users/123");
            }
            _ => panic!("Expected joined URI string"),
        }
    }

    #[test]
    fn test_join_relative_reference() {
        let result = join(&[Val::from("https://example.com/a/b"), Val::from("../c")]);
        match result {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "https://example.com/c"),
            _ => panic!("Expected joined URI string"),
        }
    }

    #[test]
    fn test_is_valid_true() {
        let result = is_valid(&[Val::from("https://example.com")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(true))));
    }

    #[test]
    fn test_is_valid_false() {
        let result = is_valid(&[Val::from("://broken")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));
    }

    #[test]
    fn test_is_valid_non_string() {
        let result = is_valid(&[Val::Int(42)]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));
    }

    #[test]
    fn test_uri_constructor_string() {
        let result = uri_constructor(&[Val::from("https://example.com/path")]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("$type")), Some(&Val::from(URI_TYPE)));
                assert_eq!(m.get(&Val::from("scheme")), Some(&Val::from("https")));
                assert_eq!(m.get(&Val::from("host")), Some(&Val::from("example.com")));
                assert_eq!(m.get(&Val::from("path")), Some(&Val::from("/path")));
            }
            _ => panic!("Expected Uri map"),
        }
    }

    #[test]
    fn test_uri_constructor_invalid() {
        let result = uri_constructor(&[Val::from("://broken")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_uri_to_str() {
        let uri = uri_constructor(&[Val::from("https://example.com:8080/path?q=1#frag")]);
        let uri_val = match uri {
            HotResult::Ok(v) => v,
            _ => panic!("Expected Uri"),
        };

        let result = uri_to_str(&[uri_val]);
        match result {
            HotResult::Ok(Val::Str(s)) => {
                assert_eq!(&*s, "https://example.com:8080/path?q=1#frag");
            }
            _ => panic!("Expected string"),
        }
    }

    #[test]
    fn test_encode_query_decode_query_roundtrip() {
        let mut map = IndexMap::new();
        map.insert(Val::from("name"), Val::from("Alice & Bob"));
        map.insert(Val::from("q"), Val::from("hello=world"));

        let encoded = encode_query(&[Val::Map(Box::new(map))]);
        let encoded_str = match &encoded {
            HotResult::Ok(Val::Str(s)) => s.clone(),
            _ => panic!("Expected encoded string"),
        };

        let decoded = decode_query(&[Val::from(encoded_str)]);
        match decoded {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("name")), Some(&Val::from("Alice & Bob")));
                assert_eq!(m.get(&Val::from("q")), Some(&Val::from("hello=world")));
            }
            _ => panic!("Expected decoded map"),
        }
    }
}
