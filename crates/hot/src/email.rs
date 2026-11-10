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

    /// Why this provider cannot send, when it knows.
    fn unavailable_reason(&self) -> Option<&str> {
        None
    }
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

    /// Why `conf` cannot deliver email for `purpose`, judged from the
    /// settings every Hot process reads. Credentials, which only the sending
    /// worker holds, are checked when the provider is built. The default
    /// accepts any named provider.
    fn delivery_status(&self, conf: &crate::val::Val, purpose: EmailPurpose) -> Result<(), String> {
        match configured_email_provider(conf, purpose).as_str() {
            "" | "none" => Err("email.provider is not configured".to_string()),
            _ => Ok(()),
        }
    }
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

    fn unavailable_reason(&self) -> Option<&str> {
        Some(&self.reason)
    }
}

/// The public composition's factory: SMTP when selected, otherwise a provider
/// that reports why email is unavailable. Other compositions install their
/// own factory with [`set_email_provider_factory`].
struct DefaultEmailProviderFactory;

impl EmailProviderFactory for DefaultEmailProviderFactory {
    fn provider_from_conf(
        &self,
        conf: &crate::val::Val,
        purpose: EmailPurpose,
    ) -> Box<dyn EmailProvider> {
        let provider = configured_email_provider(conf, purpose);
        if provider == "smtp" {
            // An alert goes out as this process's sender. App mail arrives with
            // the sender the app wrote, and the app judged that: see
            // `email_render_status`. This process's own settings say nothing
            // about it. SMTP refuses Hot Cloud's domain on every send, so
            // `hot db redeploy`'s mail, sent as this process's sender, is refused
            // there, per admin, with a reason that names `HOT_EMAIL_FROM`.
            let sender = match purpose {
                EmailPurpose::App => Ok(()),
                EmailPurpose::Alerts => own_sender(conf, purpose),
            };
            return match sender
                .and_then(|()| crate::email_smtp::SmtpSettings::from_conf(conf))
                .and_then(|settings| crate::email_smtp::SmtpEmailProvider::new(&settings))
            {
                Ok(smtp) => {
                    // The sending worker writes an alert's links itself. App
                    // mail arrives with the links the app wrote, and the app
                    // judged those: see `email_render_status`.
                    if purpose == EmailPurpose::Alerts
                        && let Err(reason) = own_app_url(conf)
                    {
                        tracing::warn!(
                            "SMTP alert emails are sent with links that may not open: {reason}"
                        );
                    }
                    Box::new(smtp)
                }
                Err(error) => {
                    tracing::error!("SMTP email is misconfigured: {error}");
                    Box::new(NoopEmailProvider::new(format!(
                        "SMTP email is misconfigured: {error}"
                    )))
                }
            };
        }
        let reason = if provider.is_empty() || provider == "none" {
            "email.provider is not configured".to_string()
        } else {
            format!("email provider '{}' is not registered", provider)
        };
        Box::new(NoopEmailProvider::new(reason))
    }

    fn delivery_status(&self, conf: &crate::val::Val, purpose: EmailPurpose) -> Result<(), String> {
        match configured_email_provider(conf, purpose).as_str() {
            "" | "none" => Err("email.provider is not configured".to_string()),
            "smtp" => crate::email_smtp::SmtpSettings::connection_from_conf(conf)
                .and_then(|_| own_sender(conf, purpose))
                .and_then(|()| smtp_links(conf, purpose)),
            provider => Err(format!("email provider '{}' is not registered", provider)),
        }
    }
}

/// The domain of Hot Cloud's senders. The built-in defaults use it, and so do
/// the init templates when `HOT_EMAIL_FROM` or `HOT_EMAIL_ALERTS_FROM` is unset.
const HOT_CLOUD_SENDER_DOMAIN: &str = "notifications.hot.dev";

pub(crate) fn is_hot_cloud_sender(address: &str) -> bool {
    address
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(HOT_CLOUD_SENDER_DOMAIN))
}

/// Mail must come from the installation's own sender: an operator's relay may
/// not send for Hot Cloud's domain, and one that does would spoof it. This
/// checks the address mail would use, because a generated configuration sets
/// Hot Cloud's senders as well as leaving them out.
fn own_sender(conf: &crate::val::Val, purpose: EmailPurpose) -> Result<(), String> {
    let (config, settings) = match purpose {
        EmailPurpose::App => (EmailConfig::from_conf(conf), "email.from or HOT_EMAIL_FROM"),
        EmailPurpose::Alerts => (
            EmailConfig::alerts_from_conf(conf),
            "email.from or HOT_EMAIL_FROM, or email.alerts.from or HOT_EMAIL_ALERTS_FROM,",
        ),
    };
    if is_hot_cloud_sender(&config.from_email) {
        return Err(format!(
            "Set {settings} to this installation's own sender: \
             mail would come from {:?}",
            config.from_email
        ));
    }
    crate::email_smtp::check_sender(&config.from_address())
}

/// Whether the app can count on SMTP for `purpose`, given the links that mail
/// would carry. The app writes the links in its own mail, and they can carry
/// codes. An alert's links hold IDs and a project name, not a code, so a wrong
/// address costs a link that doesn't open. Refusing the alert would cost the
/// warning itself, so the provider logs [`own_app_url`]'s reason and sends it.
fn smtp_links(conf: &crate::val::Val, purpose: EmailPurpose) -> Result<(), String> {
    match purpose {
        EmailPurpose::App => own_app_url(conf),
        EmailPurpose::Alerts => Ok(()),
    }
}

/// Mail must link to this installation. Without `HOT_APP_URL`, the app and
/// worker init templates name Hot Cloud's app as `email.app-url`, and so does
/// the fallback when `HOT_ENV` is production or staging. Following an
/// invitation or verification link would then hand its code to Hot Cloud. The
/// all-in-one template names `localhost` instead, and outside development no
/// recipient can follow a link to that.
fn own_app_url(conf: &crate::val::Val) -> Result<(), String> {
    const HOT_CLOUD_APP_URLS: [&str; 2] = ["https://app.hot.dev", "https://app.hot-stg.dev"];
    let app_url = EmailConfig::from_conf(conf).app_base_url;
    let links = app_url.trim().trim_end_matches('/');
    if links.is_empty()
        || HOT_CLOUD_APP_URLS
            .iter()
            .any(|url| url.eq_ignore_ascii_case(links))
        || (!crate::env::is_local_dev() && links_to_own_machine(links))
    {
        return Err(format!(
            "Set email.app-url or HOT_APP_URL to this installation's address: \
             mail would link to {app_url:?}"
        ));
    }
    Ok(())
}

/// The address of this installation's app, to point a person at, when its
/// configuration names one [`own_app_url`] accepts. Otherwise the address is a
/// fallback, and someone sent to Hot Cloud's app could type this installation's
/// password into it, so the caller should not name one.
pub fn own_app_address(conf: &crate::val::Val) -> Option<String> {
    own_app_url(conf).ok()?;
    let app_url = EmailConfig::from_conf(conf).app_base_url;
    Some(app_url.trim().trim_end_matches('/').to_string())
}

/// Whether a link to `url` reaches only the machine it is opened on.
fn links_to_own_machine(url: &str) -> bool {
    let absolute = if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    };
    let Ok(parsed) = url::Url::parse(&absolute) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host == "localhost" || host.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
        None => false,
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
        .get_or_init(|| Arc::new(DefaultEmailProviderFactory))
        .clone()
}

/// Whether the installation can deliver email for `purpose`: it names a
/// provider this composition registers, with usable settings. Processes that
/// only queue email can rely on this without holding the provider's
/// credentials.
pub fn email_delivery_configured(conf: &crate::val::Val, purpose: EmailPurpose) -> bool {
    email_delivery_status(conf, purpose).is_ok()
}

/// Like [`email_delivery_configured`], with the reason delivery is unavailable.
pub fn email_delivery_status(conf: &crate::val::Val, purpose: EmailPurpose) -> Result<(), String> {
    email_provider_factory().delivery_status(conf, purpose)
}

/// Whether mail written from `conf` is fit to queue. A queued message keeps the
/// sender and links it was written with, whichever process sends it and
/// whatever that process's own settings say, so they are judged where they are
/// written. Hot Cloud sends as itself, and a local development run keeps the
/// built-in sender, which SMTP won't send from.
pub fn email_render_status(conf: &crate::val::Val) -> Result<(), String> {
    if crate::product::is_hot_cloud(conf) {
        return Ok(());
    }
    if !(crate::product::is_local_dev_experience(conf) && crate::env::is_local_dev()) {
        own_sender(conf, EmailPurpose::App)?;
    }
    own_app_url(conf)
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
    /// With SMTP, Hot Cloud's alert sender counts as not set: the init templates
    /// write it whenever `HOT_EMAIL_ALERTS_FROM` is unset, even if the
    /// installation has its own `email.from`.
    pub fn alerts_from_conf(conf: &crate::val::Val) -> Self {
        let mut config = Self::from_conf(conf);
        let default_from = if conf.get("email.from").is_some() {
            config.from_email.clone()
        } else {
            "alerts@notifications.hot.dev".to_string()
        };
        let alerts_from = Self::configured_value(conf, "email.alerts.from", &default_from);
        let keep_own_sender = is_hot_cloud_sender(&alerts_from)
            && !is_hot_cloud_sender(&config.from_email)
            && configured_email_provider(conf, EmailPurpose::Alerts) == "smtp";
        if !keep_own_sender {
            config.from_email = alerts_from;
        }
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

    /// Get the formatted "From" address. A name with characters RFC 5322
    /// reserves (a comma, `@`, `:`, a quote, ...) is quoted, so the address
    /// parses back as one mailbox.
    pub fn from_address(&self) -> String {
        // Control characters, such as a line break, can't appear in a header.
        let name: String = self
            .from_name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let name = name.trim();
        if name.is_empty() {
            return self.from_email.clone();
        }
        let atom = |c: char| {
            c.is_ascii_alphanumeric() || !c.is_ascii() || " !#$%&'*+-/=?^_`{|}~".contains(c)
        };
        if name.chars().all(atom) {
            format!("{} <{}>", name, self.from_email)
        } else {
            let quoted = name.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{}\" <{}>", quoted, self.from_email)
        }
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

    /// Why email sending is unavailable, to record against a message left unsent.
    pub fn unavailable_reason(&self) -> &str {
        self.provider
            .unavailable_reason()
            .unwrap_or("Email sender not configured")
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

    /// The send failed in a way a later attempt may not: the caller retries.
    #[error("Failed to send email: {0}")]
    SendFailed(String),

    /// This message or its recipient was refused for good (an address that
    /// can't be mailed, content or a size the relay won't take): sending it
    /// again gives the same answer. A refusal of the session or the sender
    /// (credentials, TLS, relaying, an unverified sender) is `SendFailed`,
    /// however final the provider's reply reads: it says nothing about this
    /// message, and a fixed configuration sends it.
    #[error("Email rejected: {0}")]
    Rejected(String),
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
    fn from_address_quotes_names_that_need_it() {
        let config = |name: &str| EmailConfig {
            from_name: name.to_string(),
            ..EmailConfig::from_conf(&crate::val!({"email": {"from": "hot@acme.com"}}))
        };
        assert_eq!(config("Hot Dev").from_address(), "Hot Dev <hot@acme.com>");
        assert_eq!(
            config("Acme, Inc.").from_address(),
            "\"Acme, Inc.\" <hot@acme.com>"
        );
        assert_eq!(
            config("Say \"hi\"\r\nBcc: x").from_address(),
            "\"Say \\\"hi\\\"  Bcc: x\" <hot@acme.com>"
        );
        assert_eq!(config(" ").from_address(), "hot@acme.com");
        for name in ["Acme, Inc.", "ops@acme", "Team: Ops", "Ünïcode", "a\\b"] {
            crate::email_smtp::check_sender(&config(name).from_address()).unwrap();
        }
    }

    #[test]
    fn smtp_delivery_needs_a_relay_and_a_sender() {
        let status = |conf: crate::val::Val| {
            DefaultEmailProviderFactory.delivery_status(&conf, EmailPurpose::App)
        };
        let alerts = |conf: crate::val::Val| {
            DefaultEmailProviderFactory.delivery_status(&conf, EmailPurpose::Alerts)
        };
        let smtp = |extra: crate::val::Val| {
            crate::val!({"email": {"provider": "smtp", "smtp": {"host": "smtp.example.com"}}})
                .merge(&extra)
        };
        assert!(status(crate::val!({})).is_err());
        assert!(status(crate::val!({"email": {"provider": "none"}})).is_err());
        assert!(
            status(crate::val!({"email": {"provider": "ses", "from": "hot@acme.com"}}))
                .unwrap_err()
                .contains("not registered")
        );
        // A relay without its host, or without the operator's own sender.
        assert!(
            status(crate::val!({"email": {"provider": "smtp", "from": "hot@acme.com"}}))
                .unwrap_err()
                .contains("email.smtp.host")
        );
        assert!(
            status(smtp(crate::val!({})))
                .unwrap_err()
                .contains("email.from")
        );
        assert!(alerts(smtp(crate::val!({}))).is_err());
        // What the init templates write when HOT_EMAIL_FROM and
        // HOT_EMAIL_ALERTS_FROM are unset is no sender of the operator's own.
        let generated = crate::val!({"email": {
            "from": "hi@notifications.hot.dev",
            "alerts": {"from": "alerts@notifications.hot.dev"}
        }});
        let reason = status(smtp(generated.clone())).unwrap_err();
        assert!(reason.contains("HOT_EMAIL_FROM"), "{reason}");
        assert!(reason.contains("hi@notifications.hot.dev"), "{reason}");
        let reason = alerts(smtp(generated)).unwrap_err();
        assert!(reason.contains("HOT_EMAIL_ALERTS_FROM"), "{reason}");
        assert!(reason.contains("alerts@notifications.hot.dev"), "{reason}");
        assert!(
            status(smtp(
                crate::val!({"email": {"from": "HI@NOTIFICATIONS.HOT.DEV"}})
            ))
            .is_err()
        );
        assert!(
            alerts(smtp(
                crate::val!({"email": {"alerts": {"from": "alerts@notifications.hot.dev"}}})
            ))
            .is_err()
        );
        // Each purpose is judged by the sender it uses.
        let own_alerts_sender = smtp(crate::val!({"email": {
            "from": "hi@notifications.hot.dev", "alerts": {"from": "alerts@acme.com"}
        }}));
        assert!(status(own_alerts_sender.clone()).is_err());
        assert_eq!(alerts(own_alerts_sender), Ok(()));
        assert!(
            status(smtp(crate::val!({"email": {"from": "not an address"}})))
                .unwrap_err()
                .contains("Invalid sender")
        );
        // The app holds no SMTP credentials; their absence does not count.
        let configured =
            smtp(crate::val!({"email": {"from": "hot@acme.com", "smtp": {"username": "hot"}}}));
        assert_eq!(status(configured.clone()), Ok(()));
        assert_eq!(alerts(configured), Ok(()));
        assert_eq!(
            alerts(smtp(
                crate::val!({"email": {"alerts": {"from": "alerts@acme.com"}}})
            )),
            Ok(())
        );
        assert!(email_delivery_configured(
            &smtp(crate::val!({"email": {"from": "hot@acme.com"}})),
            EmailPurpose::App
        ));
    }

    #[test]
    fn smtp_alerts_take_the_own_sender_over_the_generated_one() {
        // HOT_EMAIL_FROM set and HOT_EMAIL_ALERTS_FROM unset, on a generated
        // configuration.
        let generated = crate::val!({"email": {
            "provider": "smtp", "from": "hot@acme.com",
            "alerts": {"from": "alerts@notifications.hot.dev"},
            "smtp": {"host": "smtp.example.com"}
        }});
        assert_eq!(
            EmailConfig::alerts_from_conf(&generated).from_address(),
            "Hot Alerts <hot@acme.com>"
        );
        assert_eq!(
            DefaultEmailProviderFactory.delivery_status(&generated, EmailPurpose::Alerts),
            Ok(())
        );
        // SMTP chosen for alerts alone counts too.
        let alerts_only = generated.merge(&crate::val!({"email": {
            "provider": "none", "alerts": {"provider": "smtp"}
        }}));
        assert_eq!(
            EmailConfig::alerts_from_conf(&alerts_only).from_email,
            "hot@acme.com"
        );
        // A sender of the operator's own stays.
        let own = generated.merge(&crate::val!({"email": {"alerts": {"from": "alerts@acme.com"}}}));
        assert_eq!(
            EmailConfig::alerts_from_conf(&own).from_email,
            "alerts@acme.com"
        );
        // Another composition's provider may send as Hot Cloud.
        let other = generated.merge(&crate::val!({"email": {"provider": "cloud"}}));
        assert_eq!(
            EmailConfig::alerts_from_conf(&other).from_email,
            "alerts@notifications.hot.dev"
        );
        assert_eq!(
            EmailConfig::alerts_from_conf(&crate::val!({})).from_email,
            "alerts@notifications.hot.dev"
        );
    }

    #[test]
    fn links_to_own_machine_recognizes_loopback_hosts() {
        for url in [
            "http://localhost:4680",
            "http://LOCALHOST:4680",
            "http://localhost./",
            "http://hot.localhost:4680",
            "localhost:4680",
            "localhost",
            "https://user@localhost/app",
            "http://127.0.0.1:4680",
            "http://127.1.2.3",
            "http://0.0.0.0:4680",
            "http://[::1]:4680",
            "http://[::]:4680",
            "http://[::ffff:127.0.0.1]:4680",
        ] {
            assert!(links_to_own_machine(url), "{url}");
        }
        for url in [
            "https://hot.acme.com",
            "https://localhost.acme.com",
            "http://notlocalhost:4680",
            "http://10.0.0.5:4680",
            "http://[2001:db8::1]:4680",
            "hot.acme.com",
            "http://",
        ] {
            assert!(!links_to_own_machine(url), "{url}");
        }
    }

    // HOT_ENV production or staging, including their fallback address and
    // loopback addresses, is covered by
    // crates/hot_app/tests/smtp_links_outside_hot_cloud.rs, which owns its
    // process's environment.
    #[tokio::test]
    async fn smtp_mail_does_not_link_to_hot_cloud() {
        let conf = |app_url: &str| {
            crate::val!({"email": {
                "provider": "smtp", "from": "hot@acme.com", "app-url": app_url,
                "smtp": {"host": "smtp.example.com"}
            }})
        };
        for app_url in [
            "https://app.hot.dev",
            "https://app.hot.dev/",
            "HTTPS://APP.HOT.DEV",
            "https://app.hot-stg.dev",
        ] {
            let conf = conf(app_url);
            let reason = DefaultEmailProviderFactory
                .delivery_status(&conf, EmailPurpose::App)
                .unwrap_err();
            assert!(reason.contains("email.app-url or HOT_APP_URL"), "{reason}");
            // A worker sends the links the app wrote, whatever its own address.
            assert!(EmailSender::from_conf(&conf).is_available());
        }
        let own = conf("https://hot.acme.com");
        assert_eq!(
            DefaultEmailProviderFactory.delivery_status(&own, EmailPurpose::App),
            Ok(())
        );
        assert!(
            DefaultEmailProviderFactory
                .provider_from_conf(&own, EmailPurpose::App)
                .is_configured()
        );
    }

    // The fallback address and loopback addresses are likewise covered by
    // crates/hot_app/tests/smtp_links_outside_hot_cloud.rs.
    #[test]
    fn own_app_address_is_never_hot_clouds() {
        let at = |app_url: &str| crate::val!({"email": {"app-url": app_url}});
        for hot_cloud in [
            "https://app.hot.dev",
            "https://app.hot.dev/",
            "HTTPS://APP.HOT.DEV",
            "https://app.hot-stg.dev",
        ] {
            assert_eq!(own_app_address(&at(hot_cloud)), None);
        }
        for own in ["https://hot.acme.com", " https://hot.acme.com/ "] {
            assert_eq!(
                own_app_address(&at(own)).as_deref(),
                Some("https://hot.acme.com")
            );
        }
    }

    /// What `run` logs at INFO and above.
    fn logs_of<T>(run: impl FnOnce() -> T) -> (T, String) {
        #[derive(Clone, Default)]
        struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Logs {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, run);
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        (result, text)
    }

    #[tokio::test]
    async fn smtp_alerts_are_sent_with_a_warning_about_their_links() {
        let conf = |app_url: &str| {
            crate::val!({"email": {
                "provider": "smtp", "from": "hot@acme.com", "app-url": app_url,
                "smtp": {"host": "smtp.example.com"}
            }})
        };
        let build = |conf: &crate::val::Val| {
            logs_of(|| DefaultEmailProviderFactory.provider_from_conf(conf, EmailPurpose::Alerts))
        };

        let hot_cloud = conf("https://app.hot.dev");
        assert_eq!(
            DefaultEmailProviderFactory.delivery_status(&hot_cloud, EmailPurpose::Alerts),
            Ok(())
        );
        let (provider, logs) = build(&hot_cloud);
        assert!(provider.is_configured());
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("email.app-url or HOT_APP_URL"), "{logs}");
        assert!(logs.contains("https://app.hot.dev"), "{logs}");

        let (provider, logs) = build(&conf("https://hot.acme.com"));
        assert!(provider.is_configured());
        assert!(!logs.contains("WARN"), "{logs}");

        // Only the links are forgiven: a relay would reject Hot Cloud's sender
        // or spoof its domain, and a sender that can't send warns of nothing.
        let hot_cloud_sender = hot_cloud.merge(&crate::val!({"email": {
            "from": "hi@notifications.hot.dev",
            "alerts": {"from": "alerts@notifications.hot.dev"}
        }}));
        assert!(
            DefaultEmailProviderFactory
                .delivery_status(&hot_cloud_sender, EmailPurpose::Alerts)
                .unwrap_err()
                .contains("HOT_EMAIL_ALERTS_FROM")
        );
        let (provider, logs) = build(&hot_cloud_sender);
        assert!(!provider.is_configured());
        assert!(!logs.contains("WARN"), "{logs}");
    }

    #[tokio::test]
    async fn smtp_app_mail_is_sent_with_the_links_the_app_wrote() {
        let conf = crate::val!({"email": {
            "provider": "smtp", "from": "hot@acme.com", "app-url": "https://app.hot.dev",
            "smtp": {"host": "smtp.example.com"}
        }});
        let (provider, logs) =
            logs_of(|| DefaultEmailProviderFactory.provider_from_conf(&conf, EmailPurpose::App));
        assert!(provider.is_configured());
        assert!(!logs.contains("WARN"), "{logs}");
    }

    // HOT_ENV production or staging is covered by
    // crates/hot_app/tests/smtp_links_outside_hot_cloud.rs.
    #[test]
    fn mail_is_judged_where_it_is_written() {
        let at = |experience: &str, email: crate::val::Val| {
            crate::val!({"product": {"experience": experience}}).merge(&crate::val!({
                "email": email
            }))
        };
        let own = crate::val!({"from": "hot@acme.com", "app-url": "https://hot.acme.com"});
        // Local development queues with the built-in sender and its own address,
        // with or without an experience named.
        assert_eq!(email_render_status(&crate::val!({})), Ok(()));
        assert_eq!(
            email_render_status(&at("local-dev", crate::val!({}))),
            Ok(())
        );
        for experience in ["self-host", "local-dev"] {
            assert_eq!(email_render_status(&at(experience, own.clone())), Ok(()));
            let reason = email_render_status(&at(
                experience,
                crate::val!({"from": "hot@acme.com", "app-url": "https://app.hot.dev"}),
            ))
            .unwrap_err();
            assert!(reason.contains("email.app-url or HOT_APP_URL"), "{reason}");
        }
        // No delivery settings are needed: a worker may hold them all.
        let reason = email_render_status(&at("self-host", crate::val!({}))).unwrap_err();
        assert!(reason.contains("HOT_EMAIL_FROM"), "{reason}");
        assert!(reason.contains("hi@notifications.hot.dev"), "{reason}");
        assert!(
            email_render_status(&at("self-host", crate::val!({"from": "not an address"})))
                .unwrap_err()
                .contains("Invalid sender")
        );
        // Hot Cloud sends as itself.
        assert_eq!(
            email_render_status(&at(
                "hot-cloud",
                crate::val!({
                    "from": "hi@notifications.hot.dev", "app-url": "https://app.hot.dev"
                })
            )),
            Ok(())
        );
    }

    #[test]
    fn a_sender_says_why_it_cannot_send() {
        let sender = |provider: Box<dyn EmailProvider>| {
            EmailSender::new(EmailConfig::from_conf(&crate::val!({})), provider)
        };
        assert_eq!(
            sender(Box::new(NoopEmailProvider::new("no relay"))).unavailable_reason(),
            "no relay"
        );
        // A provider from another composition need not know.
        assert_eq!(
            sender(Box::new(MockEmailProvider::new())).unavailable_reason(),
            "Email sender not configured"
        );
        assert!(
            EmailSender::alerts_from_conf(&crate::val!({"email": {"provider": "ses"}}))
                .unavailable_reason()
                .contains("'ses' is not registered")
        );
    }

    // The SMTP transport's connection pool needs a runtime.
    #[tokio::test]
    async fn smtp_without_a_sender_does_not_send_from_hot_cloud() {
        let conf =
            crate::val!({"email": {"provider": "smtp", "smtp": {"host": "smtp.example.com"}}});
        // Nor does naming Hot Cloud's sender, as a generated configuration does.
        let generated = conf.merge(&crate::val!({"email": {"from": "hi@notifications.hot.dev"}}));
        let email = Email::new("ops@example.com", "Redeploy").with_text("body");
        for conf in [&conf, &generated] {
            // An alert would go out as this process's sender.
            assert!(!EmailSender::alerts_from_conf(conf).is_available());
            // A queued message brings its own, so this process can still send
            // those, but not as Hot Cloud. These are refused before the relay is
            // contacted.
            let sender = EmailSender::from_conf(conf);
            assert!(sender.is_available());
            for sent in [
                sender.send_email(&email).await,
                sender
                    .send_email_with_from(&email, "Hot Dev <hi@notifications.hot.dev>")
                    .await,
            ] {
                let Err(EmailError::SendFailed(reason)) = sent else {
                    panic!("Hot Cloud's sender must be refused");
                };
                assert!(reason.contains("HOT_EMAIL_FROM"), "{reason}");
            }
        }
        let conf = conf.merge(&crate::val!({"email": {"from": "hot@acme.com"}}));
        assert!(EmailSender::from_conf(&conf).is_available());
        assert!(EmailSender::alerts_from_conf(&conf).is_available());
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
