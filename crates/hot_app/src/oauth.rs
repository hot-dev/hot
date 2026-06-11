use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    RedirectUrl, Scope, TokenResponse, TokenUrl, basic::BasicClient,
};
use serde::{Deserialize, Serialize};
use std::env;

/// OAuth provider configuration
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub google: Option<GoogleConfig>,
    pub github: Option<GitHubConfig>,
    pub redirect_base_url: String,
}

#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub client_id: String,
    pub client_secret: String,
}

type ConfiguredOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

impl OAuthConfig {
    /// Load OAuth configuration from environment variables
    pub fn from_env() -> Self {
        let google = if let (Ok(client_id), Ok(client_secret)) = (
            env::var("HOT_GOOGLE_CLIENT_ID"),
            env::var("HOT_GOOGLE_CLIENT_SECRET"),
        ) {
            Some(GoogleConfig {
                client_id,
                client_secret,
            })
        } else {
            None
        };

        let github = if let (Ok(client_id), Ok(client_secret)) = (
            env::var("HOT_GITHUB_CLIENT_ID"),
            env::var("HOT_GITHUB_CLIENT_SECRET"),
        ) {
            Some(GitHubConfig {
                client_id,
                client_secret,
            })
        } else {
            None
        };

        let redirect_base_url = env::var("HOT_OAUTH_REDIRECT_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:4680".to_string());

        Self {
            google,
            github,
            redirect_base_url,
        }
    }

    /// Check if any OAuth provider is configured
    pub fn is_any_configured(&self) -> bool {
        self.google.is_some() || self.github.is_some()
    }
}

/// Create a Google OAuth client
pub fn create_google_client(
    config: &GoogleConfig,
    redirect_url: &str,
) -> Result<ConfiguredOAuthClient, String> {
    let client_id = ClientId::new(config.client_id.clone());
    let client_secret = ClientSecret::new(config.client_secret.clone());
    let auth_url = AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
        .map_err(|e| format!("Invalid auth URL: {}", e))?;
    let token_url = TokenUrl::new("https://oauth2.googleapis.com/token".to_string())
        .map_err(|e| format!("Invalid token URL: {}", e))?;

    let redirect_url = RedirectUrl::new(redirect_url.to_string())
        .map_err(|e| format!("Invalid redirect URL: {}", e))?;

    Ok(BasicClient::new(client_id)
        .set_client_secret(client_secret)
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect_url))
}

/// Create a GitHub OAuth client
pub fn create_github_client(
    config: &GitHubConfig,
    redirect_url: &str,
) -> Result<ConfiguredOAuthClient, String> {
    let client_id = ClientId::new(config.client_id.clone());
    let client_secret = ClientSecret::new(config.client_secret.clone());
    let auth_url = AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
        .map_err(|e| format!("Invalid auth URL: {}", e))?;
    let token_url = TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
        .map_err(|e| format!("Invalid token URL: {}", e))?;

    let redirect_url = RedirectUrl::new(redirect_url.to_string())
        .map_err(|e| format!("Invalid redirect URL: {}", e))?;

    Ok(BasicClient::new(client_id)
        .set_client_secret(client_secret)
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect_url))
}

/// Google user info response
#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleUserInfo {
    pub id: String, // Google user ID
    pub email: String,
    pub verified_email: bool,
    pub name: Option<String>,
    pub picture: Option<String>,
}

/// GitHub user info response
#[derive(Debug, Deserialize, Serialize)]
pub struct GitHubUserInfo {
    pub id: i64, // GitHub user ID
    pub login: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    /// All VERIFIED emails on the GitHub account (primary first), resolved
    /// via `/user/emails`. Not part of the `/user` payload itself.
    #[serde(default)]
    pub verified_emails: Vec<String>,
}

/// GitHub email response (from /user/emails endpoint)
#[derive(Debug, Deserialize, Serialize)]
pub struct GitHubEmail {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
}

/// Fetch Google user info using access token
pub async fn fetch_google_user_info(access_token: &str) -> Result<GoogleUserInfo, String> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch Google user info: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Google user info request failed with status {}: {}",
            status, body
        ));
    }

    // Get the response text for logging
    let body_text = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    tracing::debug!("Google user info response: {}", body_text);

    // Parse the JSON
    serde_json::from_str::<GoogleUserInfo>(&body_text).map_err(|e| {
        format!(
            "Failed to parse Google user info: {} (body: {})",
            e, body_text
        )
    })
}

/// Fetch GitHub user info using access token
pub async fn fetch_github_user_info(access_token: &str) -> Result<GitHubUserInfo, String> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/user")
        .bearer_auth(access_token)
        .header("User-Agent", "hot-app")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch GitHub user info: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "GitHub user info request failed with status {}: {}",
            status, body
        ));
    }

    let mut user_info = response
        .json::<GitHubUserInfo>()
        .await
        .map_err(|e| format!("Failed to parse GitHub user info: {}", e))?;

    // Always resolve the email through /user/emails filtered to VERIFIED
    // addresses. The public profile email on /user carries no verification
    // guarantee, so it is never used — if no verified email exists, fail
    // and let the callback surface a clear message.
    let emails = fetch_github_emails(access_token).await?;
    user_info.email = select_github_email(&emails);
    user_info.verified_emails = verified_github_emails(&emails);

    Ok(user_info)
}

/// All verified emails, primary first, preserving GitHub's order otherwise.
/// Used by the callback's email-selection logic (invite match / picker).
pub fn verified_github_emails(emails: &[GitHubEmail]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(primary) = emails.iter().find(|e| e.primary && e.verified) {
        out.push(primary.email.clone());
    }
    for e in emails.iter().filter(|e| e.verified && !e.primary) {
        out.push(e.email.clone());
    }
    out
}

/// Pick the email to use from GitHub's `/user/emails` response:
/// primary+verified first, then any verified, else None.
///
/// Pure function (no I/O) so the selection policy is unit-testable.
pub fn select_github_email(emails: &[GitHubEmail]) -> Option<String> {
    emails
        .iter()
        .find(|e| e.primary && e.verified)
        .or_else(|| emails.iter().find(|e| e.verified))
        .map(|e| e.email.clone())
}

/// Fetch GitHub user emails
async fn fetch_github_emails(access_token: &str) -> Result<Vec<GitHubEmail>, String> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/user/emails")
        .bearer_auth(access_token)
        .header("User-Agent", "hot-app")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch GitHub emails: {}", e))?;

    if !response.status().is_success() {
        return Err("Failed to fetch GitHub emails".to_string());
    }

    response
        .json::<Vec<GitHubEmail>>()
        .await
        .map_err(|e| format!("Failed to parse GitHub emails: {}", e))
}

/// Generate authorization URL for Google
pub fn get_google_auth_url(
    client: &ConfiguredOAuthClient,
    invite_code: Option<&str>,
) -> Result<(url::Url, CsrfToken), String> {
    let mut auth_request = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .add_extra_param("prompt", "select_account");

    // Add invite code as state parameter if provided
    if let Some(code) = invite_code {
        auth_request = auth_request.add_extra_param("state_data", code);
    }

    Ok(auth_request.url())
}

/// Generate authorization URL for GitHub
pub fn get_github_auth_url(
    client: &ConfiguredOAuthClient,
    invite_code: Option<&str>,
) -> Result<(url::Url, CsrfToken), String> {
    let mut auth_request = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("user:email".to_string()));

    // Add invite code as state parameter if provided
    if let Some(code) = invite_code {
        auth_request = auth_request.add_extra_param("state_data", code);
    }

    Ok(auth_request.url())
}

/// Exchange authorization code for access token
pub async fn exchange_code_for_token(
    client: &ConfiguredOAuthClient,
    code: AuthorizationCode,
) -> Result<String, String> {
    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Failed to create OAuth HTTP client: {}", e))?;

    let token_result = client
        .exchange_code(code)
        .request_async(&http_client)
        .await
        .map_err(|e| format!("Failed to exchange code for token: {}", e))?;

    Ok(token_result.access_token().secret().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_github_email_prefers_primary_verified() {
        let emails = vec![
            GitHubEmail {
                email: "old@example.com".to_string(),
                primary: false,
                verified: true,
            },
            GitHubEmail {
                email: "main@example.com".to_string(),
                primary: true,
                verified: true,
            },
        ];
        assert_eq!(
            select_github_email(&emails),
            Some("main@example.com".to_string())
        );
    }

    #[test]
    fn select_github_email_falls_back_to_any_verified() {
        let emails = vec![
            GitHubEmail {
                email: "unverified@example.com".to_string(),
                primary: true,
                verified: false,
            },
            GitHubEmail {
                email: "verified@example.com".to_string(),
                primary: false,
                verified: true,
            },
        ];
        assert_eq!(
            select_github_email(&emails),
            Some("verified@example.com".to_string())
        );
    }

    #[test]
    fn select_github_email_rejects_all_unverified() {
        let emails = vec![GitHubEmail {
            email: "unverified@example.com".to_string(),
            primary: true,
            verified: false,
        }];
        assert_eq!(select_github_email(&emails), None);
        assert_eq!(select_github_email(&[]), None);
    }

    #[test]
    fn verified_github_emails_primary_first_unverified_excluded() {
        let emails = vec![
            GitHubEmail {
                email: "unverified@example.com".to_string(),
                primary: false,
                verified: false,
            },
            GitHubEmail {
                email: "other@example.com".to_string(),
                primary: false,
                verified: true,
            },
            GitHubEmail {
                email: "main@example.com".to_string(),
                primary: true,
                verified: true,
            },
        ];
        assert_eq!(
            verified_github_emails(&emails),
            vec![
                "main@example.com".to_string(),
                "other@example.com".to_string()
            ]
        );
        assert!(verified_github_emails(&[]).is_empty());
    }

    #[test]
    fn google_auth_url_includes_redirect_scope_and_invite_state() {
        let client = create_google_client(
            &GoogleConfig {
                client_id: "google-client".to_string(),
                client_secret: "google-secret".to_string(),
            },
            "https://app.example.test/auth/google/callback",
        )
        .expect("google client");

        let (url, csrf) =
            get_google_auth_url(&client, Some("invite-123")).expect("google auth url");
        let query: Vec<_> = url.query_pairs().collect();

        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://accounts.google.com/o/oauth2/v2/auth"
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "client_id" && value == "google-client")
        );
        assert!(query.iter().any(|(key, value)| key == "redirect_uri"
            && value == "https://app.example.test/auth/google/callback"));
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "state" && value == csrf.secret())
        );
        assert!(query.iter().any(|(key, value)| key == "scope"
            && value.contains("openid")
            && value.contains("email")
            && value.contains("profile")));
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "state_data" && value == "invite-123")
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "prompt" && value == "select_account")
        );
    }

    #[test]
    fn github_auth_url_includes_redirect_scope_and_invite_state() {
        let client = create_github_client(
            &GitHubConfig {
                client_id: "github-client".to_string(),
                client_secret: "github-secret".to_string(),
            },
            "https://app.example.test/auth/github/callback",
        )
        .expect("github client");

        let (url, csrf) =
            get_github_auth_url(&client, Some("invite-456")).expect("github auth url");
        let query: Vec<_> = url.query_pairs().collect();

        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            "https://github.com/login/oauth/authorize"
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "client_id" && value == "github-client")
        );
        assert!(query.iter().any(|(key, value)| key == "redirect_uri"
            && value == "https://app.example.test/auth/github/callback"));
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "state" && value == csrf.secret())
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "scope" && value == "user:email")
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "state_data" && value == "invite-456")
        );
    }
}
