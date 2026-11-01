//! SMTP delivery, selected with `email.provider = "smtp"` (or
//! `email.alerts.provider` for alerts). Settings live under `email.smtp`; the
//! password is normally supplied as `HOT_EMAIL_SMTP_PASSWORD_FILE`.

use crate::email::{Email, EmailError, EmailProvider};
use crate::val::Val;
use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;

/// How the connection to the SMTP server is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// Plaintext connection that must upgrade with STARTTLS (default port 587).
    StartTls,
    /// TLS from the first byte, also called SMTPS (default port 465).
    Tls,
    /// No encryption, for a relay on a trusted local network. Never allowed
    /// with credentials.
    None,
}

#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: Option<String>,
    pub password: Option<String>,
    pub timeout: Duration,
}

impl SmtpSettings {
    pub fn from_conf(conf: &Val) -> Result<Self, String> {
        let host = setting(conf, "email.smtp.host")
            .ok_or("email.smtp.host is required when the email provider is smtp")?;
        let security = match setting(conf, "email.smtp.security")
            .unwrap_or_else(|| "starttls".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "starttls" => SmtpSecurity::StartTls,
            "tls" => SmtpSecurity::Tls,
            "none" => SmtpSecurity::None,
            other => {
                return Err(format!(
                    "Unsupported email.smtp.security {other:?}; expected starttls, tls, or none"
                ));
            }
        };
        let port = match setting(conf, "email.smtp.port") {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("email.smtp.port {port:?} is not a valid port"))?,
            None => match security {
                SmtpSecurity::StartTls => 587,
                SmtpSecurity::Tls => 465,
                SmtpSecurity::None => 25,
            },
        };
        let username = setting(conf, "email.smtp.username");
        let password = setting(conf, "email.smtp.password");
        match (&username, &password) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    "Set both email.smtp.username and email.smtp.password, or neither".into(),
                );
            }
            (Some(_), Some(_)) if security == SmtpSecurity::None => {
                return Err("email.smtp.security none would send the SMTP password in \
                            plaintext; use starttls or tls"
                    .into());
            }
            _ => {}
        }
        let timeout = match setting(conf, "email.smtp.timeout-seconds") {
            Some(seconds) => seconds
                .parse::<u64>()
                .ok()
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs)
                .ok_or_else(|| {
                    format!("email.smtp.timeout-seconds {seconds:?} must be a positive integer")
                })?,
            None => Duration::from_secs(30),
        };
        Ok(Self {
            host,
            port,
            security,
            username,
            password,
            timeout,
        })
    }
}

/// A trimmed, non-empty setting, whether it was configured as text or a number.
fn setting(conf: &Val, key: &str) -> Option<String> {
    conf.get(key)
        .map(|_| conf.get_str(key).trim().to_string())
        .filter(|value| !value.is_empty())
}

pub struct SmtpEmailProvider {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpEmailProvider {
    pub fn new(settings: &SmtpSettings) -> Result<Self, String> {
        // Certificates are verified against the platform trust store.
        let tls_parameters = || {
            TlsParameters::new(settings.host.clone())
                .map_err(|e| format!("Invalid SMTP TLS settings for {}: {e}", settings.host))
        };
        let tls = match settings.security {
            SmtpSecurity::StartTls => Tls::Required(tls_parameters()?),
            SmtpSecurity::Tls => Tls::Wrapper(tls_parameters()?),
            SmtpSecurity::None => Tls::None,
        };
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
            .port(settings.port)
            .tls(tls)
            .timeout(Some(settings.timeout));
        if let (Some(username), Some(password)) = (&settings.username, &settings.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }
        Ok(Self {
            transport: builder.build(),
        })
    }
}

#[async_trait]
impl EmailProvider for SmtpEmailProvider {
    async fn send(&self, email: &Email, from: &str) -> Result<(), EmailError> {
        let message = build_message(email, from)?;
        self.transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|e| EmailError::SendFailed(format!("SMTP delivery failed: {e}")))
    }

    fn is_configured(&self) -> bool {
        true
    }
}

fn build_message(email: &Email, from: &str) -> Result<Message, EmailError> {
    let from: Mailbox = from
        .parse()
        .map_err(|e| EmailError::SendFailed(format!("Invalid sender address {from:?}: {e}")))?;
    let to: Mailbox = email.to.parse().map_err(|e| {
        EmailError::SendFailed(format!("Invalid recipient address {:?}: {e}", email.to))
    })?;
    let builder = Message::builder()
        .from(from)
        .to(to)
        .subject(email.subject.clone());
    match (&email.html, &email.text) {
        (Some(html), Some(text)) => builder.multipart(MultiPart::alternative_plain_html(
            text.clone(),
            html.clone(),
        )),
        (Some(html), None) => builder.singlepart(SinglePart::html(html.clone())),
        (None, text) => builder.singlepart(SinglePart::plain(text.clone().unwrap_or_default())),
    }
    .map_err(|e| EmailError::SendFailed(format!("Cannot build email: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::val;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn settings_default_to_required_starttls() {
        let settings = SmtpSettings::from_conf(&val!({"email": {"smtp": {
            "host": "smtp.example.com", "username": "hot", "password": "pw"
        }}}))
        .unwrap();
        assert_eq!(settings.security, SmtpSecurity::StartTls);
        assert_eq!(settings.port, 587);
        assert_eq!(settings.timeout, Duration::from_secs(30));

        let settings = SmtpSettings::from_conf(&val!({"email": {"smtp": {
            "host": "smtp.example.com", "security": "TLS", "port": "2465"
        }}}))
        .unwrap();
        assert_eq!(settings.security, SmtpSecurity::Tls);
        assert_eq!(settings.port, 2465);
    }

    #[test]
    fn settings_reject_unsafe_or_incomplete_configuration() {
        let error = |smtp| SmtpSettings::from_conf(&val!({"email": {"smtp": smtp}})).unwrap_err();
        assert!(error(val!({})).contains("email.smtp.host is required"));
        assert!(error(val!({"host": "h", "security": "ssl"})).contains("Unsupported"));
        assert!(error(val!({"host": "h", "port": "0"})).contains("not a valid port"));
        assert!(error(val!({"host": "h", "port": "smtp"})).contains("not a valid port"));
        assert!(error(val!({"host": "h", "username": "hot"})).contains("Set both"));
        assert!(
            error(val!({"host": "h", "security": "none", "username": "u", "password": "p"}))
                .contains("plaintext")
        );
        assert!(error(val!({"host": "h", "timeout-seconds": "0"})).contains("positive"));
    }

    #[test]
    fn messages_carry_both_bodies() {
        let email = Email::new("Ops <ops@example.com>", "Alert")
            .with_text("plain body")
            .with_html("<p>html body</p>");
        let raw = String::from_utf8(
            build_message(&email, "Hot Alerts <alerts@example.com>")
                .unwrap()
                .formatted(),
        )
        .unwrap();
        assert!(raw.contains("From: \"Hot Alerts\" <alerts@example.com>"));
        assert!(raw.contains("To: Ops <ops@example.com>"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("plain body") && raw.contains("<p>html body</p>"));
        assert!(matches!(
            build_message(&Email::new("not an address", "x"), "a@example.com"),
            Err(EmailError::SendFailed(_))
        ));
    }

    /// Accepts one message over plaintext SMTP and returns the DATA section.
    async fn receive_one_message(listener: tokio::net::TcpListener) -> String {
        let (socket, _) = listener.accept().await.unwrap();
        let (read, mut write) = socket.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"220 test ESMTP\r\n").await.unwrap();
        let mut data = String::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let command = line.to_ascii_uppercase();
            let reply: &[u8] = if command.starts_with("EHLO") {
                b"250 test\r\n"
            } else if command.starts_with("DATA") {
                write.write_all(b"354 send it\r\n").await.unwrap();
                while let Some(line) = lines.next_line().await.unwrap() {
                    if line == "." {
                        break;
                    }
                    data.push_str(&line);
                    data.push('\n');
                }
                b"250 queued\r\n"
            } else if command.starts_with("QUIT") {
                write.write_all(b"221 bye\r\n").await.unwrap();
                break;
            } else {
                b"250 ok\r\n"
            };
            write.write_all(reply).await.unwrap();
        }
        data
    }

    #[tokio::test]
    async fn delivers_through_an_smtp_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(receive_one_message(listener));
        let settings = SmtpSettings::from_conf(&val!({"email": {"smtp": {
            "host": "127.0.0.1", "port": i64::from(port), "security": "none"
        }}}))
        .unwrap();
        let provider = SmtpEmailProvider::new(&settings).unwrap();
        provider
            .send(
                &Email::new("ops@example.com", "Hello").with_text("delivered"),
                "Hot <hot@example.com>",
            )
            .await
            .unwrap();
        drop(provider);
        let data = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the transport closes its connection")
            .unwrap();
        assert!(data.contains("Subject: Hello"), "{data}");
        assert!(data.contains("delivered"), "{data}");
    }
}
