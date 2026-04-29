//! Authenticated HTTP client for the hot.dev API.

use hot::val::Val;

use super::error::format_api_error;

#[derive(Debug, Clone)]
pub(crate) struct ApiClient {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) client: reqwest::Client,
}

impl ApiClient {
    pub(crate) fn from_config(conf: &Val) -> Result<Self, String> {
        let remote_name = conf.get_str("set.remote");

        if remote_name.is_empty() {
            return Err("No remote configured. Either:\n\
                 • Set hot.set.remote in your configuration, OR\n\
                 • Use --local to access the local database directly"
                .to_string());
        }

        let remote_config = conf
            .get("remote")
            .and_then(|remotes| remotes.get(&remote_name))
            .ok_or_else(|| {
                format!(
                    "Remote config '{}' not found. Either:\n\
                     • Configure hot.remote.{}.url and hot.remote.{}.key, OR\n\
                     • Use --local to access the local database directly",
                    remote_name, remote_name, remote_name
                )
            })?;

        let api_key: String = remote_config
            .get("key")
            .and_then(|k| match k {
                Val::Str(s) => Some((*s).to_string()),
                _ => None,
            })
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                format!(
                    "API key not configured for '{}'. Either:\n\
                     • Set the environment variable for hot.remote.{}.key, OR\n\
                     • Use --local to access the local database directly",
                    remote_name, remote_name
                )
            })?;

        let base_url = remote_config
            .get("url")
            .and_then(|u| match u {
                Val::Str(s) => Some((*s).to_string()),
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "API URL not configured for '{}'. Either:\n\
                     • Set hot.remote.{}.url, OR\n\
                     • Use --local to access the local database directly",
                    remote_name, remote_name
                )
            })?;

        Ok(ApiClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client: reqwest::Client::new(),
        })
    }

    /// Build an authenticated request builder for `method path`.
    /// Centralizes URL composition and the bearer header so verb helpers stay thin.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        self.client
            .request(method, &url)
            .header("Authorization", format!("Bearer {}", self.api_key))
    }

    /// Map a reqwest send error into a user-friendly String, distinguishing
    /// configuration errors (bad/missing key) from connectivity failures.
    fn map_send_err(&self, e: reqwest::Error) -> String {
        if e.is_builder() {
            "API request configuration error. This usually means the API key is invalid or empty.\n\
                 Please set the environment variable for your API key, OR use --local for local development.".to_string()
        } else if e.is_connect() {
            format!("Failed to connect to API at {}: {}", self.base_url, e)
        } else {
            format!("API request failed: {}", e)
        }
    }

    /// Convert a non-2xx response into an error String with consistent
    /// formatting (special-cased 401 messaging, JSON `error.message`
    /// extraction otherwise).
    async fn error_from_response(response: reqwest::Response) -> String {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if body.is_empty() {
                "Authentication failed (401 Unauthorized).\n\
                     The API key may be invalid, expired, or not found.\n\
                     Please check your hot.remote configuration or use --local for local development.".to_string()
            } else {
                format!("Authentication failed (401 Unauthorized): {}", body)
            }
        } else {
            format_api_error(status, &body)
        }
    }

    /// On 2xx, deserialize the JSON body into `T`. On non-2xx, return a
    /// formatted error.
    async fn parse_json<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, String> {
        if !response.status().is_success() {
            return Err(Self::error_from_response(response).await);
        }
        response
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))
    }

    /// On 2xx, discard the body and return `()`. On non-2xx, return a formatted
    /// error. Use for endpoints that return 204 No Content (e.g. DELETE).
    #[allow(dead_code)]
    async fn parse_no_content(response: reqwest::Response) -> Result<(), String> {
        if !response.status().is_success() {
            return Err(Self::error_from_response(response).await);
        }
        Ok(())
    }

    pub(crate) async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    pub(crate) async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::POST, path)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    pub(crate) async fn post_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    #[allow(dead_code)]
    pub(crate) async fn put<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::PUT, path)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    #[allow(dead_code)]
    pub(crate) async fn put_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::PUT, path)
            .json(body)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    #[allow(dead_code)]
    pub(crate) async fn patch<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::PATCH, path)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    #[allow(dead_code)]
    pub(crate) async fn patch_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::PATCH, path)
            .json(body)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }

    /// DELETE endpoint returning 204 No Content (the common case across hot's API).
    /// For DELETE endpoints that return a JSON body, use `delete_json` instead.
    #[allow(dead_code)]
    pub(crate) async fn delete(&self, path: &str) -> Result<(), String> {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_no_content(response).await
    }

    #[allow(dead_code)]
    pub(crate) async fn delete_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, String> {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .send()
            .await
            .map_err(|e| self.map_send_err(e))?;
        Self::parse_json(response).await
    }
}
