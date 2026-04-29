//! Shared formatting for non-2xx API responses.

/// Format an API error response, extracting the message from JSON if possible.
pub(crate) fn format_api_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(message) = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
    {
        return format!("API error ({}):\n{}", status, message);
    }

    if body.is_empty() {
        format!("API error ({})", status)
    } else {
        format!("API error ({}): {}", status, body)
    }
}
