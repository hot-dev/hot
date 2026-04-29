//! Email sending module with pluggable provider support
//!
//! This module provides core email sending functionality that can be used
//! by both hot_app (for verification emails, etc.) and hot_worker (for alert delivery).

use async_trait::async_trait;
use chrono::Datelike;
use std::sync::{Arc, OnceLock};

/// Get the current year for copyright text
pub fn current_year() -> i32 {
    chrono::Utc::now().year()
}

/// An email message to be sent
#[derive(Debug, Clone)]
pub struct Email {
    /// Recipient email address
    pub to: String,
    /// Subject line
    pub subject: String,
    /// HTML content (optional)
    pub html: Option<String>,
    /// Plain text content (optional)
    pub text: Option<String>,
}

impl Email {
    /// Create a new email with required fields
    pub fn new(to: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            subject: subject.into(),
            html: None,
            text: None,
        }
    }

    /// Set the HTML content
    pub fn with_html(mut self, html: impl Into<String>) -> Self {
        self.html = Some(html.into());
        self
    }

    /// Set the plain text content
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

/// Email provider trait for pluggable email backends
#[async_trait]
pub trait EmailProvider: Send + Sync {
    /// Send an email through this provider
    async fn send(&self, email: &Email, from: &str) -> Result<(), EmailError>;

    /// Check if the provider is properly configured
    fn is_configured(&self) -> bool;
}

/// Which part of the system is requesting an email provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailPurpose {
    App,
    Alerts,
}

/// Factory installed by binary composition layers to create concrete email providers.
pub trait EmailProviderFactory: Send + Sync {
    fn provider_from_conf(
        &self,
        conf: &crate::val::Val,
        purpose: EmailPurpose,
    ) -> Box<dyn EmailProvider>;
}

/// No-op provider used when no concrete provider is configured.
pub struct NoopEmailProvider {
    reason: String,
}

impl NoopEmailProvider {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn unconfigured() -> Self {
        Self::new("email sending is not configured")
    }
}

#[async_trait]
impl EmailProvider for NoopEmailProvider {
    async fn send(&self, _email: &Email, _from: &str) -> Result<(), EmailError> {
        tracing::debug!("Email send skipped: {}", self.reason);
        Err(EmailError::NotConfigured)
    }

    fn is_configured(&self) -> bool {
        false
    }
}

struct NoopEmailProviderFactory;

impl EmailProviderFactory for NoopEmailProviderFactory {
    fn provider_from_conf(
        &self,
        conf: &crate::val::Val,
        purpose: EmailPurpose,
    ) -> Box<dyn EmailProvider> {
        let provider = configured_email_provider(conf, purpose);
        let reason = if provider.is_empty() || provider == "none" {
            "email.provider is not configured".to_string()
        } else {
            format!("email provider '{}' is not registered", provider)
        };
        Box::new(NoopEmailProvider::new(reason))
    }
}

static EMAIL_PROVIDER_FACTORY: OnceLock<Arc<dyn EmailProviderFactory>> = OnceLock::new();

pub fn set_email_provider_factory(
    factory: Arc<dyn EmailProviderFactory>,
) -> Result<(), Arc<dyn EmailProviderFactory>> {
    EMAIL_PROVIDER_FACTORY.set(factory)
}

pub fn email_provider_factory() -> Arc<dyn EmailProviderFactory> {
    EMAIL_PROVIDER_FACTORY
        .get_or_init(|| Arc::new(NoopEmailProviderFactory))
        .clone()
}

pub fn configured_email_provider(conf: &crate::val::Val, purpose: EmailPurpose) -> String {
    let default_provider = conf.get_str_or_default("email.provider", "none");
    let provider = match purpose {
        EmailPurpose::App => default_provider,
        EmailPurpose::Alerts => conf.get_str_or_default("email.alerts.provider", &default_provider),
    };
    provider.trim().to_ascii_lowercase()
}

/// Email configuration from environment
#[derive(Debug, Clone)]
pub struct EmailConfig {
    /// Base URL for the app (for building verification links)
    pub app_base_url: String,
    /// Base URL for product/marketing links.
    pub web_base_url: String,
    /// Documentation URL used in email footers and onboarding links.
    pub docs_url: String,
    /// Absolute logo URL for email clients.
    pub logo_url: String,
    /// From email address
    pub from_email: String,
    /// From name
    pub from_name: String,
}

impl EmailConfig {
    fn configured_value(conf: &crate::val::Val, key: &str, default: &str) -> String {
        let value = conf.get_str_or_default(key, default);
        if value.trim().is_empty() {
            default.to_string()
        } else {
            value.trim().to_string()
        }
    }

    fn email_web_url(conf: &crate::val::Val) -> String {
        let default = crate::product::web_url(conf);
        Self::configured_value(conf, "email.web-url", &default)
            .trim_end_matches('/')
            .to_string()
    }

    fn docs_url(conf: &crate::val::Val, web_base_url: &str) -> String {
        let default = format!("{}/docs", web_base_url);
        Self::configured_value(conf, "email.docs-url", &default)
    }

    fn logo_url(conf: &crate::val::Val) -> String {
        Self::configured_value(
            conf,
            "email.logo-url",
            "https://assets.hot.dev/email/hot_logo_email.png",
        )
    }

    /// Load email configuration from resolved Hot config.
    pub fn from_conf(conf: &crate::val::Val) -> Self {
        let app_base_url =
            Self::configured_value(conf, "email.app-url", &crate::env::get_app_url());
        let web_base_url = Self::email_web_url(conf);
        let docs_url = Self::docs_url(conf, &web_base_url);
        let logo_url = Self::logo_url(conf);
        let from_email = Self::configured_value(conf, "email.from", "hi@notifications.hot.dev");
        let from_name = Self::configured_value(conf, "email.from-name", "Hot Dev");

        Self {
            app_base_url,
            web_base_url,
            docs_url,
            logo_url,
            from_email,
            from_name,
        }
    }

    /// Load email configuration for alert emails from resolved Hot config.
    /// Falls back to regular email settings if alert-specific keys are not set.
    pub fn alerts_from_conf(conf: &crate::val::Val) -> Self {
        let mut config = Self::from_conf(conf);
        let default_from = if conf.get("email.from").is_some() {
            config.from_email.clone()
        } else {
            "alerts@notifications.hot.dev".to_string()
        };
        config.from_email = Self::configured_value(conf, "email.alerts.from", &default_from);
        config.from_name = Self::configured_value(conf, "email.alerts.from-name", "Hot Alerts");
        config
    }

    /// Load email configuration for regular app emails (verification, invites, etc.)
    pub fn from_env() -> Self {
        Self::from_conf(&crate::val!({}))
    }

    /// Load email configuration for alert emails
    /// Falls back to regular email settings if alert-specific vars are not set
    pub fn alerts_from_env() -> Self {
        Self::alerts_from_conf(&crate::val!({}))
    }

    /// Get the formatted "From" address
    pub fn from_address(&self) -> String {
        format!("{} <{}>", self.from_name, self.from_email)
    }
}

/// Email sender using pluggable providers
pub struct EmailSender {
    config: EmailConfig,
    provider: Box<dyn EmailProvider>,
}

impl EmailSender {
    /// Create a new email sender with a specific provider
    pub fn new(config: EmailConfig, provider: Box<dyn EmailProvider>) -> Self {
        Self { config, provider }
    }

    /// Create a new email sender from environment.
    pub fn from_env() -> Self {
        let config = EmailConfig::from_env();
        let provider =
            email_provider_factory().provider_from_conf(&crate::val!({}), EmailPurpose::App);
        Self::new(config, provider)
    }

    /// Create a new email sender from resolved Hot config.
    pub fn from_conf(conf: &crate::val::Val) -> Self {
        let config = EmailConfig::from_conf(conf);
        let provider = email_provider_factory().provider_from_conf(conf, EmailPurpose::App);
        Self::new(config, provider)
    }

    /// Create a new email sender for alerts from environment
    pub fn alerts_from_env() -> Self {
        let config = EmailConfig::alerts_from_env();
        let provider =
            email_provider_factory().provider_from_conf(&crate::val!({}), EmailPurpose::Alerts);
        Self::new(config, provider)
    }

    /// Create a new alert email sender from resolved Hot config.
    pub fn alerts_from_conf(conf: &crate::val::Val) -> Self {
        let config = EmailConfig::alerts_from_conf(conf);
        let provider = email_provider_factory().provider_from_conf(conf, EmailPurpose::Alerts);
        Self::new(config, provider)
    }

    /// Check if email sending is available
    pub fn is_available(&self) -> bool {
        self.provider.is_configured()
    }

    /// Get the email configuration
    pub fn config(&self) -> &EmailConfig {
        &self.config
    }

    /// Send a generic email
    pub async fn send_email(&self, email: &Email) -> Result<(), EmailError> {
        self.provider.send(email, &self.config.from_address()).await
    }

    /// Send an email with a custom "from" address (used by the email queue worker)
    pub async fn send_email_with_from(
        &self,
        email: &Email,
        from_address: &str,
    ) -> Result<(), EmailError> {
        self.provider.send(email, from_address).await
    }
}

/// Implementation of AlertEmailSender trait for integration with alert delivery system
#[async_trait]
impl crate::db::alert::AlertEmailSender for EmailSender {
    async fn send_alert_email(&self, to: &str, subject: &str, html: &str) -> Result<(), String> {
        let email = Email::new(to, subject).with_html(html);
        self.send_email(&email).await.map_err(|e| e.to_string())
    }
}

/// Email error types
#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("Email sending is not configured")]
    NotConfigured,

    #[error("Failed to send email: {0}")]
    SendFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_email_config_from_address() {
        let config = EmailConfig {
            app_base_url: "https://app.hot.dev".to_string(),
            web_base_url: "https://hot.dev".to_string(),
            docs_url: "https://hot.dev/docs".to_string(),
            logo_url: "https://assets.hot.dev/email/hot_logo_email.png".to_string(),
            from_email: "noreply@hot.dev".to_string(),
            from_name: "Hot Dev".to_string(),
        };

        assert_eq!(config.from_address(), "Hot Dev <noreply@hot.dev>");
    }

    #[test]
    fn test_email_builder() {
        let email = Email::new("test@example.com", "Test Subject")
            .with_html("<p>Hello</p>")
            .with_text("Hello");

        assert_eq!(email.to, "test@example.com");
        assert_eq!(email.subject, "Test Subject");
        assert_eq!(email.html, Some("<p>Hello</p>".to_string()));
        assert_eq!(email.text, Some("Hello".to_string()));
    }

    #[test]
    fn test_noop_provider_not_configured() {
        let provider = NoopEmailProvider::unconfigured();
        assert!(!provider.is_configured());
    }

    /// Mock provider for testing
    pub struct MockEmailProvider {
        pub should_fail: bool,
        pub sent_emails: std::sync::Mutex<Vec<Email>>,
    }

    impl MockEmailProvider {
        pub fn new() -> Self {
            Self {
                should_fail: false,
                sent_emails: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl EmailProvider for MockEmailProvider {
        async fn send(&self, email: &Email, _from: &str) -> Result<(), EmailError> {
            if self.should_fail {
                return Err(EmailError::SendFailed("Mock failure".to_string()));
            }
            self.sent_emails.lock().unwrap().push(email.clone());
            Ok(())
        }

        fn is_configured(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_email_sender_with_mock_provider() {
        let config = EmailConfig {
            app_base_url: "https://app.hot.dev".to_string(),
            web_base_url: "https://hot.dev".to_string(),
            docs_url: "https://hot.dev/docs".to_string(),
            logo_url: "https://assets.hot.dev/email/hot_logo_email.png".to_string(),
            from_email: "noreply@hot.dev".to_string(),
            from_name: "Hot Dev".to_string(),
        };

        let provider = MockEmailProvider::new();
        let sender = EmailSender::new(config, Box::new(provider));

        assert!(sender.is_available());
    }
}
