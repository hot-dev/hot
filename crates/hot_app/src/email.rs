//! Email sending module for the Hot App
//!
//! This module provides application-specific email templates built on top of
//! the core email infrastructure from the `hot` crate.
//!
//! For core email infrastructure (Email, EmailSender, EmailConfig, etc.),
//! see `hot::email`.

// Re-export core email types for convenience
pub use hot::email::{Email, EmailConfig, EmailError, EmailProvider, EmailSender, current_year};

/// Extension trait for app-specific email sending methods
#[allow(async_fn_in_trait)]
pub trait AppEmailSender {
    /// Get the email configuration
    fn config(&self) -> &EmailConfig;

    /// Send a generic email
    async fn send_email(&self, email: &Email) -> Result<(), EmailError>;

    /// Send a verification email
    async fn send_verification_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        verification_token: &str,
    ) -> Result<(), EmailError> {
        let web_url = self.config().web_base_url.as_str();
        let logo_url = self.config().logo_url.as_str();
        let verification_url = format!(
            "{}/verify-email?token={}",
            self.config().app_base_url,
            verification_token
        );

        let greeting = match to_name {
            Some(name) => format!("Hi {},", name),
            None => "Hi,".to_string(),
        };

        let html_content = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Verify your email</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 30px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #2d3748;">Verify your email address</h2>

        <p>{greeting}</p>

        <p>Thanks for signing up for Hot Dev! Please click the button below to verify your email address and complete your registration.</p>

        <div style="text-align: center; margin: 30px 0;">
            <a href="{verification_url}" style="background-color: #e53e3e; color: white; padding: 12px 30px; text-decoration: none; border-radius: 6px; font-weight: 600; display: inline-block;">
                Verify Email Address
            </a>
        </div>

        <p style="color: #718096; font-size: 14px;">
            If the button doesn't work, copy and paste this link into your browser:
            <br>
            <a href="{verification_url}" style="color: #e53e3e; word-break: break-all;">{verification_url}</a>
        </p>

        <p style="color: #718096; font-size: 14px;">
            This link will expire in 24 hours.
        </p>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px;">
        <p>If you didn't create an account with Hot Dev, you can safely ignore this email.</p>
        <p>&copy; {current_year} Hot Dev, LLC. All rights reserved.</p>
    </div>
</body>
</html>"#,
            greeting = greeting,
            verification_url = verification_url,
            web_url = web_url,
            logo_url = logo_url,
            current_year = current_year()
        );

        let text_content = format!(
            r#"{greeting}

Thanks for signing up for Hot Dev! Please click the link below to verify your email address and complete your registration:

{verification_url}

This link will expire in 24 hours.

If you didn't create an account with Hot Dev, you can safely ignore this email.

- The Hot Dev Team"#,
            greeting = greeting,
            verification_url = verification_url
        );

        let email = Email::new(to_email, "Verify your Hot Dev email address")
            .with_html(html_content)
            .with_text(text_content);

        self.send_email(&email).await
    }

    /// Send a verification email for an alert destination (specific email address)
    async fn send_destination_verification_email(
        &self,
        to_email: &str,
        org_name: &str,
        destination_name: &str,
        verification_token: &str,
    ) -> Result<(), EmailError> {
        let web_url = self.config().web_base_url.as_str();
        let logo_url = self.config().logo_url.as_str();
        let verification_url = format!(
            "{}/verify-alert-destination?token={}",
            self.config().app_base_url,
            verification_token
        );

        let html_content = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Verify your alert destination</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 30px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #2d3748;">Verify your alert destination</h2>

        <p>Hi,</p>

        <p>Someone in the <strong>{org_name}</strong> organization on Hot Dev added this email address as an alert destination (<strong>{destination_name}</strong>). To start receiving alerts at this address, please verify by clicking the button below.</p>

        <div style="text-align: center; margin: 30px 0;">
            <a href="{verification_url}" style="background-color: #e53e3e; color: white; padding: 12px 30px; text-decoration: none; border-radius: 6px; font-weight: 600; display: inline-block;">
                Verify Email Address
            </a>
        </div>

        <p style="color: #718096; font-size: 14px;">
            If the button doesn't work, copy and paste this link into your browser:
            <br>
            <a href="{verification_url}" style="color: #e53e3e; word-break: break-all;">{verification_url}</a>
        </p>

        <p style="color: #718096; font-size: 14px;">
            This link will expire in 24 hours.
        </p>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px;">
        <p>If you didn't expect this email, you can safely ignore it. No alerts will be sent to this address unless you verify.</p>
        <p>&copy; {current_year} Hot Dev, LLC. All rights reserved.</p>
    </div>
</body>
</html>"#,
            org_name = org_name,
            destination_name = destination_name,
            verification_url = verification_url,
            web_url = web_url,
            logo_url = logo_url,
            current_year = current_year()
        );

        let text_content = format!(
            r#"Hi,

Someone in the "{org_name}" organization on Hot Dev added this email address as an alert destination ("{destination_name}"). To start receiving alerts at this address, please verify by visiting the link below:

{verification_url}

This link will expire in 24 hours.

If you didn't expect this email, you can safely ignore it. No alerts will be sent to this address unless you verify.

- The Hot Dev Team"#,
            org_name = org_name,
            destination_name = destination_name,
            verification_url = verification_url
        );

        let email = Email::new(to_email, "Verify your alert destination - Hot Dev")
            .with_html(html_content)
            .with_text(text_content);

        self.send_email(&email).await
    }

    /// Send an organization invitation email
    async fn send_invitation_email(
        &self,
        to_email: &str,
        org_name: &str,
        inviter_name: &str,
        invite_code: &str,
    ) -> Result<(), EmailError> {
        let web_url = self.config().web_base_url.as_str();
        let logo_url = self.config().logo_url.as_str();
        let invite_url = format!("{}/invite?code={}", self.config().app_base_url, invite_code);

        let html_content = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>You're invited to join {org_name}</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 30px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #2d3748;">You're invited to join {org_name}</h2>

        <p>Hi,</p>

        <p><strong>{inviter_name}</strong> has invited you to join <strong>{org_name}</strong> on Hot Dev.</p>

        <p>Hot Dev is a platform for building, deploying, and running backend applications with ease.</p>

        <div style="text-align: center; margin: 30px 0;">
            <a href="{invite_url}" style="background-color: #e53e3e; color: white; padding: 12px 30px; text-decoration: none; border-radius: 6px; font-weight: 600; display: inline-block;">
                Accept Invitation
            </a>
        </div>

        <p style="color: #718096; font-size: 14px;">
            If the button doesn't work, copy and paste this link into your browser:
            <br>
            <a href="{invite_url}" style="color: #e53e3e; word-break: break-all;">{invite_url}</a>
        </p>

        <p style="color: #718096; font-size: 14px;">
            This invitation will expire in 7 days.
        </p>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px;">
        <p>If you weren't expecting this invitation, you can safely ignore this email.</p>
        <p>&copy; {current_year} Hot Dev, LLC. All rights reserved.</p>
    </div>
</body>
</html>"#,
            org_name = org_name,
            inviter_name = inviter_name,
            invite_url = invite_url,
            web_url = web_url,
            logo_url = logo_url,
            current_year = current_year()
        );

        let text_content = format!(
            r#"Hi,

{inviter_name} has invited you to join {org_name} on Hot Dev.

Hot Dev is a platform for building, deploying, and running backend applications with ease.

Accept your invitation by visiting:
{invite_url}

This invitation will expire in 7 days.

If you weren't expecting this invitation, you can safely ignore this email.

- The Hot Dev Team"#,
            inviter_name = inviter_name,
            org_name = org_name,
            invite_url = invite_url
        );

        let email = Email::new(
            to_email,
            format!("You're invited to join {} on Hot Dev", org_name),
        )
        .with_html(html_content)
        .with_text(text_content);

        self.send_email(&email).await
    }

    /// Send a welcome email after successful subscription checkout
    async fn send_welcome_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        org_name: &str,
        plan_name: &str,
    ) -> Result<(), EmailError> {
        let greeting = match to_name {
            Some(name) => format!("Hi {},", name),
            None => "Hi,".to_string(),
        };

        let web_url = self.config().web_base_url.as_str();
        let docs_url = self.config().docs_url.as_str();
        let logo_url = self.config().logo_url.as_str();

        let html_content = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Welcome to Hot Cloud!</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 30px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #2d3748;">Welcome to Hot Cloud! 🎉</h2>

        <p>{greeting}</p>

        <p>Thank you for subscribing to <strong>{plan_name}</strong> for <strong>{org_name}</strong>. Your subscription is now active and you're ready to start building.</p>

        <p>The Hot Docs will walk you through installing Hot locally, writing your first Hot project, creating an API key, and deploying to Hot Cloud.</p>

        <div style="text-align: center; margin: 30px 0;">
            <a href="{docs_url}" style="background-color: #e53e3e; color: white; padding: 12px 30px; text-decoration: none; border-radius: 6px; font-weight: 600; display: inline-block;">
                Read the Hot Docs
            </a>
        </div>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px;">
        <p>
            <a href="{docs_url}" style="color: #a0aec0;">Documentation</a> ·
            <a href="{web_url}" style="color: #a0aec0;">hot.dev</a>
        </p>
        <p>&copy; {current_year} Hot Dev, LLC. All rights reserved.</p>
    </div>
</body>
</html>"#,
            greeting = greeting,
            plan_name = plan_name,
            org_name = org_name,
            docs_url = docs_url,
            web_url = web_url,
            logo_url = logo_url,
            current_year = current_year()
        );

        let text_content = format!(
            r#"{greeting}

Welcome to Hot Cloud! 🎉

Thank you for subscribing to {plan_name} for {org_name}. Your subscription is now active and you're ready to start building.

The Hot Docs will walk you through installing Hot locally, writing your first Hot project, creating an API key, and deploying to Hot Cloud.

Read the Hot Docs: {docs_url}

- The Hot Dev Team"#,
            greeting = greeting,
            plan_name = plan_name,
            org_name = org_name,
            docs_url = docs_url
        );

        let email = Email::new(
            to_email,
            format!("Welcome to Hot Cloud - {} is ready!", org_name),
        )
        .with_html(html_content)
        .with_text(text_content);

        self.send_email(&email).await
    }

    /// Send a subscription cancellation email
    async fn send_cancellation_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        org_name: &str,
        plan_name: &str,
        period_end: &str,
    ) -> Result<(), EmailError> {
        let greeting = match to_name {
            Some(name) => format!("Hi {},", name),
            None => "Hi,".to_string(),
        };

        let web_url = self.config().web_base_url.as_str();
        let docs_url = self.config().docs_url.as_str();
        let logo_url = self.config().logo_url.as_str();
        let billing_url = format!("{}/orgs", self.config().app_base_url);

        let html_content = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Subscription Cancelled</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 30px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #2d3748;">Subscription Cancelled</h2>

        <p>{greeting}</p>

        <p>Your <strong>{plan_name}</strong> subscription for <strong>{org_name}</strong> has been cancelled.</p>

        <div style="background: #fefce8; border: 1px solid #fef08a; border-radius: 6px; padding: 16px; margin: 20px 0;">
            <p style="margin: 0; color: #854d0e;">
                <strong>Your access will continue until {period_end}.</strong>
            </p>
        </div>

        <p>If you cancelled by mistake, you can reactivate your subscription at any time before the end of your billing period.</p>

        <div style="text-align: center; margin: 30px 0;">
            <a href="{billing_url}" style="background-color: #e53e3e; color: white; padding: 12px 30px; text-decoration: none; border-radius: 6px; font-weight: 600; display: inline-block;">
                Manage Subscription
            </a>
        </div>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px;">
        <p>
            <a href="{docs_url}" style="color: #a0aec0;">Documentation</a> ·
            <a href="{web_url}" style="color: #a0aec0;">hot.dev</a>
        </p>
        <p>&copy; {current_year} Hot Dev, LLC. All rights reserved.</p>
    </div>
</body>
</html>"#,
            greeting = greeting,
            plan_name = plan_name,
            org_name = org_name,
            period_end = period_end,
            billing_url = billing_url,
            docs_url = docs_url,
            web_url = web_url,
            logo_url = logo_url,
            current_year = current_year()
        );

        let text_content = format!(
            r#"{greeting}

Your {plan_name} subscription for {org_name} has been cancelled.

Your access will continue until {period_end}.

If you cancelled by mistake, you can reactivate your subscription at any time before the end of your billing period.

Manage your subscription: {billing_url}

- The Hot Dev Team"#,
            greeting = greeting,
            plan_name = plan_name,
            org_name = org_name,
            period_end = period_end,
            billing_url = billing_url
        );

        let email = Email::new(to_email, format!("Subscription cancelled for {}", org_name))
            .with_html(html_content)
            .with_text(text_content);

        self.send_email(&email).await
    }

    /// Send a subscription reactivation email
    async fn send_reactivation_email(
        &self,
        to_email: &str,
        to_name: Option<&str>,
        org_name: &str,
        plan_name: &str,
    ) -> Result<(), EmailError> {
        let greeting = match to_name {
            Some(name) => format!("Hi {},", name),
            None => "Hi,".to_string(),
        };

        let web_url = self.config().web_base_url.as_str();
        let docs_url = self.config().docs_url.as_str();
        let logo_url = self.config().logo_url.as_str();
        let billing_url = format!("{}/orgs", self.config().app_base_url);

        let html_content = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Subscription Reactivated</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 30px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #2d3748;">Subscription Reactivated! 🎉</h2>

        <p>{greeting}</p>

        <p>Great news! Your <strong>{plan_name}</strong> subscription for <strong>{org_name}</strong> has been reactivated.</p>

        <div style="background: #dcfce7; border: 1px solid #86efac; border-radius: 6px; padding: 16px; margin: 20px 0;">
            <p style="margin: 0; color: #166534;">
                <strong>Your subscription is now active and will continue to renew automatically.</strong>
            </p>
        </div>

        <div style="text-align: center; margin: 30px 0;">
            <a href="{billing_url}" style="background-color: #e53e3e; color: white; padding: 12px 30px; text-decoration: none; border-radius: 6px; font-weight: 600; display: inline-block;">
                View Billing
            </a>
        </div>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px;">
        <p>
            <a href="{docs_url}" style="color: #a0aec0;">Documentation</a> ·
            <a href="{web_url}" style="color: #a0aec0;">hot.dev</a>
        </p>
        <p>&copy; {current_year} Hot Dev, LLC. All rights reserved.</p>
    </div>
</body>
</html>"#,
            greeting = greeting,
            plan_name = plan_name,
            org_name = org_name,
            billing_url = billing_url,
            docs_url = docs_url,
            web_url = web_url,
            logo_url = logo_url,
            current_year = current_year()
        );

        let text_content = format!(
            r#"{greeting}

Great news! Your {plan_name} subscription for {org_name} has been reactivated.

Your subscription is now active and will continue to renew automatically.

View your billing: {billing_url}

- The Hot Dev Team"#,
            greeting = greeting,
            plan_name = plan_name,
            org_name = org_name,
            billing_url = billing_url
        );

        let email = Email::new(
            to_email,
            format!("Subscription reactivated for {}", org_name),
        )
        .with_html(html_content)
        .with_text(text_content);

        self.send_email(&email).await
    }
}

/// Implement AppEmailSender for the core EmailSender (direct sending, used in tests)
impl AppEmailSender for EmailSender {
    fn config(&self) -> &EmailConfig {
        EmailSender::config(self)
    }

    async fn send_email(&self, email: &Email) -> Result<(), EmailError> {
        EmailSender::send_email(self, email).await
    }
}

/// Email enqueuer that writes pre-rendered emails to the database and
/// enqueues them to the hot:email queue for processing by the worker.
///
/// This replaces direct EmailSender usage in app handlers, moving actual
/// sending to the worker process.
pub struct AppEmailEnqueuer {
    config: EmailConfig,
    db: std::sync::Arc<hot::db::DatabasePool>,
}

impl AppEmailEnqueuer {
    /// Create a new email enqueuer
    pub fn new(db: std::sync::Arc<hot::db::DatabasePool>, config: EmailConfig) -> Self {
        Self { config, db }
    }

    /// Create from environment and existing database pool
    pub fn from_env(db: std::sync::Arc<hot::db::DatabasePool>) -> Self {
        Self::new(db, EmailConfig::from_env())
    }

    /// Create from resolved Hot config and existing database pool.
    pub fn from_conf(db: std::sync::Arc<hot::db::DatabasePool>, conf: &hot::val::Val) -> Self {
        Self::new(db, EmailConfig::from_conf(conf))
    }
}

impl AppEmailSender for AppEmailEnqueuer {
    fn config(&self) -> &EmailConfig {
        &self.config
    }

    async fn send_email(&self, email: &Email) -> Result<(), EmailError> {
        let from_address = self.config.from_address();

        // 1. Write audit record to email_queue table
        let email_queue_id = hot::db::email_queue::EmailQueueEntry::enqueue(
            &self.db,
            &email.to,
            &email.subject,
            email.html.as_deref(),
            email.text.as_deref(),
            &from_address,
        )
        .await
        .map_err(|e| EmailError::SendFailed(format!("Failed to enqueue email: {}", e)))?;

        // 2. Enqueue to hot:email queue for processing by worker
        if let Some(queue) = hot::notification_queue::email_queue() {
            let msg = hot::lang::event::queue::EmailMessage {
                id: uuid::Uuid::now_v7(),
                head: ahash::AHashMap::new(),
                body: hot::lang::event::queue::EmailMessageBody {
                    email_queue_id,
                    to_address: email.to.clone(),
                    subject: email.subject.clone(),
                    html_body: email.html.clone(),
                    text_body: email.text.clone(),
                    from_address,
                },
            };
            let message: hot::data::msg::Message = msg.into();
            if let Err(e) = hot::queue::Queue::enqueue(queue.as_ref(), message).await {
                tracing::error!(
                    "Failed to enqueue email to hot:email queue (id: {}): {}",
                    email_queue_id,
                    e
                );
                // Email is still in DB as pending - worker recovery will pick it up
            } else {
                tracing::debug!(
                    "Enqueued app email to {} (queue_id: {})",
                    email.to,
                    email_queue_id
                );
            }
        } else {
            tracing::warn!(
                "No email queue configured, email to {} saved to DB only (queue_id: {})",
                email.to,
                email_queue_id
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[async_trait::async_trait]
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

    async fn setup_test_db() -> std::sync::Arc<hot::db::DatabasePool> {
        std::sync::Arc::new(hot::db::test_db().await)
    }

    #[tokio::test]
    async fn test_app_email_enqueuer_writes_to_db() {
        let db = setup_test_db().await;
        let config = EmailConfig {
            app_base_url: "https://app.hot.dev".to_string(),
            web_base_url: "https://hot.dev".to_string(),
            docs_url: "https://hot.dev/docs".to_string(),
            logo_url: "https://assets.hot.dev/email/hot_logo_email.png".to_string(),
            from_email: "noreply@hot.dev".to_string(),
            from_name: "Hot Dev".to_string(),
        };

        let enqueuer = AppEmailEnqueuer::new(db.clone(), config);

        // Send an email through the enqueuer (no queue configured, just DB)
        let email = Email::new("user@example.com", "Test Subject")
            .with_html("<p>Hello</p>")
            .with_text("Hello");

        let result = enqueuer.send_email(&email).await;
        assert!(
            result.is_ok(),
            "send_email should succeed: {:?}",
            result.err()
        );

        // The email should be in the email_queue table with pending status
        // We can't easily get the ID, but we can verify by querying
        // Use a simple count check
        if let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() {
            let count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM email_queue WHERE to_address = 'user@example.com'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(count.0, 1, "Should have 1 email in queue");

            let entry: hot::db::email_queue::EmailQueueEntry =
                sqlx::query_as("SELECT * FROM email_queue WHERE to_address = 'user@example.com'")
                    .fetch_one(pool)
                    .await
                    .unwrap();

            assert_eq!(entry.subject, "Test Subject");
            assert_eq!(entry.html_body, Some("<p>Hello</p>".to_string()));
            assert_eq!(entry.text_body, Some("Hello".to_string()));
            assert_eq!(entry.from_address, "Hot Dev <noreply@hot.dev>");
            assert_eq!(
                entry.status_id,
                hot::db::email_queue::EmailQueueStatus::Pending as i16
            );
        }
    }

    #[tokio::test]
    async fn test_app_email_enqueuer_verification_email() {
        let db = setup_test_db().await;
        let config = EmailConfig {
            app_base_url: "https://app.hot.dev".to_string(),
            web_base_url: "https://hot.dev".to_string(),
            docs_url: "https://hot.dev/docs".to_string(),
            logo_url: "https://assets.hot.dev/email/hot_logo_email.png".to_string(),
            from_email: "noreply@hot.dev".to_string(),
            from_name: "Hot Dev".to_string(),
        };

        let enqueuer = AppEmailEnqueuer::new(db.clone(), config);

        // Use the trait method to send a verification email
        let result = enqueuer
            .send_verification_email("new-user@example.com", Some("Alice"), "test-token-123")
            .await;
        assert!(result.is_ok());

        // Verify the email was queued
        if let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() {
            let entry: hot::db::email_queue::EmailQueueEntry = sqlx::query_as(
                "SELECT * FROM email_queue WHERE to_address = 'new-user@example.com'",
            )
            .fetch_one(pool)
            .await
            .unwrap();

            assert_eq!(entry.subject, "Verify your Hot Dev email address");
            assert!(entry.html_body.as_ref().unwrap().contains("test-token-123"));
            assert!(entry.html_body.as_ref().unwrap().contains("Hi Alice,"));
            assert!(entry.text_body.is_some());
        }
    }

    #[tokio::test]
    async fn test_app_email_enqueuer_invitation_email() {
        let db = setup_test_db().await;
        let config = EmailConfig {
            app_base_url: "https://app.hot.dev".to_string(),
            web_base_url: "https://hot.dev".to_string(),
            docs_url: "https://hot.dev/docs".to_string(),
            logo_url: "https://assets.hot.dev/email/hot_logo_email.png".to_string(),
            from_email: "noreply@hot.dev".to_string(),
            from_name: "Hot Dev".to_string(),
        };

        let enqueuer = AppEmailEnqueuer::new(db.clone(), config);

        let result = enqueuer
            .send_invitation_email("invitee@example.com", "Acme Corp", "Bob", "invite-abc")
            .await;
        assert!(result.is_ok());

        if let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() {
            let entry: hot::db::email_queue::EmailQueueEntry = sqlx::query_as(
                "SELECT * FROM email_queue WHERE to_address = 'invitee@example.com'",
            )
            .fetch_one(pool)
            .await
            .unwrap();

            assert!(entry.subject.contains("Acme Corp"));
            assert!(entry.html_body.as_ref().unwrap().contains("Bob"));
            assert!(entry.html_body.as_ref().unwrap().contains("invite-abc"));
        }
    }

    #[tokio::test]
    async fn test_app_email_enqueuer_destination_verification_email() {
        let db = setup_test_db().await;
        let config = EmailConfig {
            app_base_url: "https://app.hot.dev".to_string(),
            web_base_url: "https://hot.dev".to_string(),
            docs_url: "https://hot.dev/docs".to_string(),
            logo_url: "https://assets.hot.dev/email/hot_logo_email.png".to_string(),
            from_email: "noreply@hot.dev".to_string(),
            from_name: "Hot Dev".to_string(),
        };

        let enqueuer = AppEmailEnqueuer::new(db.clone(), config);

        let result = enqueuer
            .send_destination_verification_email(
                "alerts@partner.com",
                "Acme Corp",
                "Partner Alerts",
                "dest-verify-token-xyz",
            )
            .await;
        assert!(result.is_ok());

        if let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() {
            let entry: hot::db::email_queue::EmailQueueEntry =
                sqlx::query_as("SELECT * FROM email_queue WHERE to_address = 'alerts@partner.com'")
                    .fetch_one(pool)
                    .await
                    .unwrap();

            assert_eq!(entry.subject, "Verify your alert destination - Hot Dev");
            let html = entry.html_body.as_ref().unwrap();
            assert!(
                html.contains("dest-verify-token-xyz"),
                "HTML should contain the token"
            );
            assert!(
                html.contains("Acme Corp"),
                "HTML should contain the org name"
            );
            assert!(
                html.contains("Partner Alerts"),
                "HTML should contain the destination name"
            );
            assert!(
                html.contains("verify-alert-destination?token=dest-verify-token-xyz"),
                "HTML should contain the full verification URL"
            );
            let text = entry.text_body.as_ref().unwrap();
            assert!(
                text.contains("dest-verify-token-xyz"),
                "Text should contain the token"
            );
            assert!(
                text.contains("Acme Corp"),
                "Text should contain the org name"
            );
        }
    }
}
