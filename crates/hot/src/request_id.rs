//! Request IDs that tie one HTTP request's log lines together. A caller's
//! `x-request-id` is kept when it is a short token; otherwise a new one is
//! made. The ID goes on the request's tracing span, which the JSON log
//! format includes on every line logged inside it, and back to the caller in
//! the response, unless the response already names one.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

pub const HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Longest caller-supplied ID kept; longer ones are replaced.
const MAX_LEN: usize = 64;

/// The request's ID, in its extensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(pub String);

/// Letters, digits, `-`, `_`, and `.`: safe in logs and headers.
fn valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Add the request's ID to its extensions, and to the response's headers.
/// Layer it outside the trace layer, so the span can read it.
pub async fn middleware(mut request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get(&HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| valid(value))
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    request.extensions_mut().insert(RequestId(id.clone()));
    let mut response = next.run(request).await;
    if !response.headers().contains_key(&HEADER)
        && let Ok(value) = HeaderValue::from_str(&id)
    {
        response.headers_mut().insert(HEADER, value);
    }
    response
}

/// The ID `middleware` gave this request, for its span; empty without it.
pub fn of<B>(request: &axum::http::Request<B>) -> &str {
    request
        .extensions()
        .get::<RequestId>()
        .map_or("", |id| id.0.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route(
                "/",
                get(|Extension(id): Extension<RequestId>| async move { id.0 }),
            )
            .route(
                "/named",
                get(|| async { ([(HEADER, "from-the-handler")], "named") }),
            )
            .layer(axum::middleware::from_fn(middleware))
    }

    async fn call(path: &str, header: Option<&str>) -> (String, String) {
        let mut request = axum::http::Request::get(path);
        if let Some(value) = header {
            request = request.header(&HEADER, value);
        }
        let response = app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let returned = response.headers()[&HEADER].to_str().unwrap().to_string();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        (String::from_utf8(body.to_vec()).unwrap(), returned)
    }

    #[tokio::test]
    async fn a_valid_caller_id_is_kept_and_returned() {
        let (seen, returned) = call("/", Some("lb-7f3a.9_x")).await;
        assert_eq!(
            (seen.as_str(), returned.as_str()),
            ("lb-7f3a.9_x", "lb-7f3a.9_x")
        );
    }

    #[tokio::test]
    async fn a_missing_or_unsafe_id_is_replaced() {
        let (seen, returned) = call("/", None).await;
        assert!(uuid::Uuid::parse_str(&seen).is_ok(), "{seen}");
        assert_eq!(seen, returned);
        for unsafe_id in ["has space", "a\"quote", &"x".repeat(65)] {
            let (seen, _) = call("/", Some(unsafe_id)).await;
            assert_ne!(seen, unsafe_id);
        }
    }

    #[tokio::test]
    async fn a_response_that_names_its_own_id_keeps_it() {
        let (_, returned) = call("/named", Some("caller")).await;
        assert_eq!(returned, "from-the-handler");
    }
}
