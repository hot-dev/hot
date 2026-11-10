//! SMTP delivery, selected with `email.provider = "smtp"` (or
//! `email.alerts.provider` for alerts). Settings live under `email.smtp`; the
//! password is normally supplied as `HOT_EMAIL_SMTP_PASSWORD_FILE`.

use crate::email::{Email, EmailError, EmailProvider};
use crate::val::Val;
use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Certificate, CertificateStore, Tls, TlsParameters};
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
    /// `email.smtp.ca-cert`: PEM roots trusted instead of the system's, for
    /// a relay whose certificate a private certificate authority issued.
    pub ca_cert: Option<String>,
}

impl SmtpSettings {
    pub fn from_conf(conf: &Val) -> Result<Self, String> {
        let settings = Self::connection_from_conf(conf)?;
        let username = setting(conf, "email.smtp.username");
        // Used exactly as given: like the `_FILE` form, which keeps spaces.
        let password = conf
            .get("email.smtp.password")
            .map(|_| conf.get_str("email.smtp.password"))
            .filter(|password| !password.is_empty());
        match (&username, &password) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    "Set both email.smtp.username and email.smtp.password, or neither".into(),
                );
            }
            (Some(_), Some(_)) if settings.security == SmtpSecurity::None => {
                return Err("email.smtp.security none would send the SMTP password in \
                            plaintext; use starttls or tls"
                    .into());
            }
            _ => {}
        }
        Ok(Self {
            username,
            password,
            ..settings
        })
    }

    /// The relay and how to reach it, without the credentials. Every Hot
    /// process reads these settings; only the worker, which sends, holds the
    /// credentials, so the app can check these alone.
    pub fn connection_from_conf(conf: &Val) -> Result<Self, String> {
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
        let ca_cert = setting(conf, "email.smtp.ca-cert");
        if let Some(pem) = &ca_cert {
            if !pem.contains("-----BEGIN CERTIFICATE-----") {
                return Err("email.smtp.ca-cert must hold PEM certificates".into());
            }
            if security == SmtpSecurity::None {
                return Err("email.smtp.ca-cert needs email.smtp.security starttls or tls".into());
            }
        }
        Ok(Self {
            host,
            port,
            security,
            username: None,
            password: None,
            timeout,
            ca_cert,
        })
    }
}

/// Check that `from` is a sender address SMTP messages can carry.
pub fn check_sender(from: &str) -> Result<(), String> {
    from.parse::<Mailbox>()
        .map(|_| ())
        .map_err(|e| format!("Invalid sender address {from:?}: {e}"))
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
        // Certificates are verified against the platform trust store, or
        // only against email.smtp.ca-cert when it is set.
        let tls_parameters = || {
            let mut builder = TlsParameters::builder(settings.host.clone());
            if let Some(pem) = &settings.ca_cert {
                let roots = Certificate::from_pem(pem.as_bytes())
                    .map_err(|e| format!("email.smtp.ca-cert is not valid PEM: {e}"))?;
                builder = builder
                    .certificate_store(CertificateStore::None)
                    .add_root_certificate(roots);
            }
            builder
                .build()
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
        self.transport.send(message).await.map(|_| ()).map_err(|e| {
            let reason = format!("SMTP delivery failed: {e}");
            if refuses_the_message(&e) {
                EmailError::Rejected(reason)
            } else {
                EmailError::SendFailed(reason)
            }
        })
    }

    fn is_configured(&self) -> bool {
        true
    }
}

/// Whether a failed send is the relay's final answer about this message or
/// its recipient, which no retry changes. Every other failure may pass and is
/// retried: no connection, TLS, a timeout, a 4yz reply, and a 5yz reply about
/// the session or the sender (see `reply_refuses_the_message`).
fn refuses_the_message(error: &lettre::transport::smtp::Error) -> bool {
    use std::error::Error as _;

    // lettre gives the reply code, and the reply's text as the error's source.
    // It does not say which command was refused.
    let Some(code) = error.status().filter(|_| error.is_permanent()) else {
        return false;
    };
    let text = error.source().map(ToString::to_string).unwrap_or_default();
    reply_refuses_the_message(u16::from(code), &text)
}

/// Whether the SMTP reply `code` with `text` refuses the message or its
/// recipient: a mailbox that cannot be mailed, or content or a size the relay
/// will not take.
///
/// A 5yz reply alone does not say so. A relay also answers 5yz to credentials
/// it does not accept, a session without TLS, a client or sender it does not
/// serve, and a sender over quota. Those refuse every message until the
/// configuration is fixed, and failing a queued email for one would lose mail
/// a retry sends. The command that was refused is not known here, so the reply
/// has to name the message or recipient itself:
///
/// - with an enhanced status code (RFC 3463), by its subject: the recipient's
///   address (5.1.1 to 5.1.4, 5.1.6, 5.1.10), the recipient's mailbox (5.2.x),
///   a message too big for the system (5.3.4), or the content (5.6.x). The
///   sender's address (5.1.7, 5.1.8), the mail system, the network, the
///   protocol and security or policy (5.7.x, which is also authentication,
///   relaying and sender authorization) are not;
/// - without one, only 551 (user not local) and 552 (storage or size
///   exceeded). Relays answer a bare 550, 553 or 554 to an unknown mailbox and
///   to an unverified sender or an unwelcome client alike.
fn reply_refuses_the_message(code: u16, text: &str) -> bool {
    // Replies that are only ever about the service or the authentication.
    if !(500..600).contains(&code) || matches!(code, 521 | 530 | 534 | 535 | 538) {
        return false;
    }
    match enhanced_status(text) {
        Some((1, detail)) => matches!(detail, 1..=4 | 6 | 10),
        Some((3, detail)) => detail == 4,
        Some((subject, _)) => matches!(subject, 2 | 6),
        None => matches!(code, 551 | 552),
    }
}

/// The subject and detail of the permanent (class 5) enhanced status code
/// that `text` starts with, if it has one.
fn enhanced_status(text: &str) -> Option<(u16, u16)> {
    let mut parts = text.split_whitespace().next()?.split('.');
    let mut number = || {
        let part = parts.next()?;
        let digits = (1..=3).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| part.parse::<u16>().ok()).flatten()
    };
    let (class, subject, detail) = (number()?, number()?, number()?);
    (class == 5 && parts.next().is_none()).then_some((subject, detail))
}

fn build_message(email: &Email, from: &str) -> Result<Message, EmailError> {
    let from: Mailbox = from
        .parse()
        .map_err(|e| EmailError::SendFailed(format!("Invalid sender address {from:?}: {e}")))?;
    // A queued message keeps the sender it was written with, which this process
    // did not choose.
    if crate::email::is_hot_cloud_sender(from.email.as_ref()) {
        return Err(EmailError::SendFailed(format!(
            "SMTP mail can't come from {}: set email.from or HOT_EMAIL_FROM where the mail \
             is written to this installation's own sender",
            from.email
        )));
    }
    let to: Mailbox = email.to.parse().map_err(|e| {
        EmailError::Rejected(format!("Invalid recipient address {:?}: {e}", email.to))
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
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

    // The test PKI from `lang::hot::tls`: a "Hot Test CA" root and the
    // localhost / 127.0.0.1 leaf it signed. Test fixture only; the private
    // key is not a secret.
    const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBgzCCASmgAwIBAgIUKm/xiZfEndHRCb0MofIVj356hw0wCgYIKoZIzj0EAwIw\n\
FjEUMBIGA1UEAwwLSG90IFRlc3QgQ0EwIBcNMjYwNzA3MDM1MDQ1WhgPMjEyNjA2\n\
MTMwMzUwNDVaMBYxFDASBgNVBAMMC0hvdCBUZXN0IENBMFkwEwYHKoZIzj0CAQYI\n\
KoZIzj0DAQcDQgAEokW275HjubOEbC/OND5MpxOV0kcDS1rNLCtCISBdVrO+JTpK\n\
85d0XeQmoYXeYuD/dD4ys9K292oXWZ4aTWuDdqNTMFEwHQYDVR0OBBYEFDaY9NS/\n\
cz7ihrSW7Cl4qLBLAsx6MB8GA1UdIwQYMBaAFDaY9NS/cz7ihrSW7Cl4qLBLAsx6\n\
MA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIgDUFZKJdUSarOOcjU\n\
KR6YTMP7FbxQnQPbmCJJWzRv26QCIQDlIFow5t5MfOA2bCI4qlQpzzqU3UfI8A0x\n\
rpQ+gA9K6A==\n\
-----END CERTIFICATE-----\n";

    const TEST_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBlzCCAT2gAwIBAgIUXxk8536GdbnDdepwNhhX4cfqVN8wCgYIKoZIzj0EAwIw\n\
FjEUMBIGA1UEAwwLSG90IFRlc3QgQ0EwIBcNMjYwNzA3MDM1MDQ1WhgPMjEyNjA2\n\
MTMwMzUwNDVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqG\n\
SM49AwEHA0IABOgIT9Zw2p2tferzeILkKhAN0GfmEzBsWcLe8glvq8XOZgGh+4Jo\n\
ZdZshD8edmxlGAQ5MFo3deUnkndZQwRH3BOjaTBnMBoGA1UdEQQTMBGCCWxvY2Fs\n\
aG9zdIcEfwAAATAJBgNVHRMEAjAAMB0GA1UdDgQWBBQ5nT/E/CTNMSEQ5UKwFYAg\n\
Ina3TjAfBgNVHSMEGDAWgBQ2mPTUv3M+4oa0luwpeKiwSwLMejAKBggqhkjOPQQD\n\
AgNIADBFAiAC2V/3eTlMXqXcS35Eb84ngDCZDelHiN5KoVhDK0h61AIhAJSxHINC\n\
6yDbW1bjPhYQyaUD5eVcUsj5qJtNXsubLY3K\n\
-----END CERTIFICATE-----\n";

    const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg/EfkR8G7u2a5vwkh\n\
zadHetFsN0BfxyFYNGX4RQYMf9ihRANCAAToCE/WcNqdrX3q83iC5CoQDdBn5hMw\n\
bFnC3vIJb6vFzmYBofuCaGXWbIQ/HnZsZRgEOTBaN3XlJ5J3WUMER9wT\n\
-----END PRIVATE KEY-----\n";

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
        assert!(error(val!({"host": "h", "ca-cert": "not a certificate"})).contains("PEM"));
        assert!(
            error(val!({"host": "h", "security": "none", "ca-cert": TEST_CA_PEM}))
                .contains("starttls or tls")
        );
    }

    #[test]
    fn passwords_keep_surrounding_spaces() {
        let settings = SmtpSettings::from_conf(&val!({"email": {"smtp": {
            "host": "h", "username": " hot ", "password": " s3cret "
        }}}))
        .unwrap();
        assert_eq!(settings.username.as_deref(), Some("hot"));
        assert_eq!(settings.password.as_deref(), Some(" s3cret "));
    }

    #[test]
    fn connection_settings_leave_out_the_credentials() {
        // The app checks these without the credentials only the worker holds.
        let settings = SmtpSettings::connection_from_conf(&val!({"email": {"smtp": {
            "host": "h", "username": "hot"
        }}}))
        .unwrap();
        assert_eq!(settings.host, "h");
        assert!(settings.username.is_none() && settings.password.is_none());
        assert!(SmtpSettings::connection_from_conf(&val!({})).is_err());
    }

    #[test]
    fn senders_must_parse_as_one_mailbox() {
        assert!(check_sender("Hot <hot@example.com>").is_ok());
        assert!(check_sender("\"Acme, Inc.\" <hot@example.com>").is_ok());
        assert!(check_sender("Acme, Inc. <hot@example.com>").is_err());
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
            Err(EmailError::Rejected(_))
        ));
    }

    #[test]
    fn messages_do_not_come_from_hot_clouds_domain() {
        let email = Email::new("ops@example.com", "Invitation").with_text("body");
        for from in [
            "Hot Dev <hi@notifications.hot.dev>",
            "alerts@NOTIFICATIONS.hot.dev",
        ] {
            let Err(EmailError::SendFailed(reason)) = build_message(&email, from) else {
                panic!("{from} must be refused");
            };
            assert!(reason.contains("HOT_EMAIL_FROM"), "{reason}");
        }
        assert!(build_message(&email, "Hot <hot@notifications.acme.com>").is_ok());
    }

    /// Accepts one message over plaintext SMTP and returns the DATA section.
    async fn receive_one_message(listener: tokio::net::TcpListener) -> String {
        let (socket, _) = listener.accept().await.unwrap();
        serve_one_message(socket).await
    }

    /// Answers one SMTP session on `socket` and returns the DATA section. A
    /// client that hangs up (as the pooled transport does when dropped) ends it.
    async fn serve_one_message(socket: impl AsyncRead + AsyncWrite) -> String {
        let (read, mut write) = tokio::io::split(socket);
        let mut lines = BufReader::new(read).lines();
        let mut data = String::new();
        let mut reply: &[u8] = b"220 test ESMTP\r\n";
        loop {
            if write.write_all(reply).await.is_err() || write.flush().await.is_err() {
                break;
            }
            let Ok(Some(line)) = lines.next_line().await else {
                break;
            };
            let command = line.to_ascii_uppercase();
            reply = if command.starts_with("EHLO") {
                b"250 test\r\n"
            } else if command.starts_with("DATA") {
                if write.write_all(b"354 send it\r\n").await.is_err()
                    || write.flush().await.is_err()
                {
                    break;
                }
                while let Ok(Some(line)) = lines.next_line().await {
                    if line == "." {
                        break;
                    }
                    data.push_str(&line);
                    data.push('\n');
                }
                b"250 queued\r\n"
            } else if command.starts_with("QUIT") {
                let _ = write.write_all(b"221 bye\r\n").await;
                let _ = write.flush().await;
                break;
            } else {
                b"250 ok\r\n"
            };
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

    // `HOT_EMAIL_FROM` on the app alone: a queued message keeps the sender the app
    // wrote, whatever this process's own settings say.
    #[tokio::test]
    async fn a_worker_without_a_sender_sends_the_one_the_message_carries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(receive_one_message(listener));
        let sender = crate::email::EmailSender::from_conf(&val!({"email": {
            "provider": "smtp",
            "smtp": {"host": "127.0.0.1", "port": i64::from(port), "security": "none"}
        }}));
        assert!(sender.is_available());
        sender
            .send_email_with_from(
                &Email::new("ops@example.com", "Invitation").with_text("from the app"),
                "Hot Dev <hot@acme.com>",
            )
            .await
            .unwrap();
        drop(sender);
        let data = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the transport closes its connection")
            .unwrap();
        assert!(data.contains("From: \"Hot Dev\" <hot@acme.com>"), "{data}");
        assert!(data.contains("from the app"), "{data}");
    }

    #[test]
    fn only_replies_about_the_recipient_or_the_message_are_rejections() {
        let rejections = [
            (
                550,
                "5.1.1 The email account that you tried to reach does not exist",
            ),
            (550, "5.1.2 Bad destination system address"),
            (501, "5.1.3 Bad recipient address syntax"),
            (
                553,
                "5.1.3 The recipient address is not a valid RFC 5321 address",
            ),
            (551, "5.1.6 Recipient has moved"),
            (556, "5.1.10 Recipient address has null MX"),
            (
                550,
                "5.2.1 The email account that you tried to reach is disabled",
            ),
            (552, "5.2.2 The recipient's inbox is out of storage space"),
            (552, "5.2.3 Your message exceeded the message size limits"),
            (552, "5.3.4 Message size exceeds fixed limit"),
            (554, "5.6.0 Mail message is malformed"),
            (550, "5.6.1 Media not supported"),
            (551, "User not local; please try <other@example.com>"),
            (
                552,
                "Requested mail action aborted: exceeded storage allocation",
            ),
        ];
        for (code, text) in rejections {
            assert!(reply_refuses_the_message(code, text), "{code} {text}");
        }

        let retried = [
            // Authentication and the session's security.
            (530, "5.7.0 Must issue a STARTTLS command first"),
            (530, "5.7.0 Authentication required"),
            (530, "Authentication required"),
            (534, "5.7.9 Application-specific password required"),
            (535, "5.7.8 Username and Password not accepted"),
            (535, "Authentication Credentials Invalid"),
            (
                538,
                "5.7.11 Encryption required for requested authentication mechanism",
            ),
            (
                535,
                "5.2.1 an authentication reply, whatever status it carries",
            ),
            // A relay that does not serve this client.
            (521, "5.3.2 This host does not accept mail"),
            (554, "No SMTP service here"),
            (554, "5.7.1 Service unavailable; client host blocked"),
            (550, "5.7.1 Relaying denied"),
            (550, "5.7.1 Client host rejected: Access denied"),
            (550, "Access denied"),
            // The sender, its authorization and its quota.
            (501, "5.1.7 Bad sender address syntax"),
            (550, "5.1.8 Sender address rejected: Domain not found"),
            (550, "5.7.1 Sender address rejected: not owned by user"),
            (
                550,
                "5.7.26 Unauthenticated email from example.com is not accepted",
            ),
            (550, "5.4.5 Daily user sending limit exceeded"),
            (
                550,
                "The from address does not match a verified Sender Identity",
            ),
            (554, "Message rejected: Email address is not verified"),
            // The mail system and the protocol.
            (550, "5.3.0 Other mail system problem"),
            (554, "5.3.5 System incorrectly configured"),
            (500, "5.5.2 Syntax error, command unrecognized"),
            (503, "5.5.1 Bad sequence of commands"),
            (554, "Transaction failed"),
            // Without an enhanced status code a 550 or 553 does not say whom
            // it refuses, and other address statuses do not either.
            (550, "No such user here"),
            (553, "Sender address rejected"),
            (550, "5.1.0 Address rejected"),
            // Not an enhanced status code of a permanent reply.
            (550, "5.1 no detail"),
            (550, "4.1.1 class of another reply"),
            (550, "5.1.1.2 too many parts"),
            (550, "5.1.x no number"),
            (550, ""),
            // Not a permanent reply.
            (450, "4.2.1 Mailbox busy"),
            (451, "4.7.1 Greylisted, try again later"),
            (452, "4.2.2 Mailbox full"),
            (250, "2.1.5 Ok"),
        ];
        for (code, text) in retried {
            assert!(!reply_refuses_the_message(code, text), "{code} {text}");
        }

        assert_eq!(enhanced_status("5.1.10 null MX"), Some((1, 10)));
        assert_eq!(enhanced_status("5.7.8"), Some((7, 8)));
        assert_eq!(enhanced_status("no code"), None);
    }

    /// A plaintext relay that greets every session with `greeting`, offers
    /// AUTH, and answers a command that starts with one of the `replies`'
    /// prefixes with that reply instead of accepting it.
    async fn scripted_relay(
        greeting: &'static str,
        replies: &'static [(&'static str, &'static str)],
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (read, mut write) = tokio::io::split(socket);
                    let mut lines = BufReader::new(read).lines();
                    let mut reply = greeting;
                    while write.write_all(reply.as_bytes()).await.is_ok()
                        && write.write_all(b"\r\n").await.is_ok()
                    {
                        let Ok(Some(line)) = lines.next_line().await else {
                            break;
                        };
                        let command = line.to_ascii_uppercase();
                        let scripted = replies.iter().find(|(on, _)| command.starts_with(on));
                        reply = match scripted {
                            Some((_, reply)) => reply,
                            None if command.starts_with("EHLO") => "250-test\r\n250 AUTH PLAIN",
                            None => "250 ok",
                        };
                    }
                });
            }
        });
        port
    }

    /// Sends one message through the relay on `port`, with credentials.
    async fn send_through(port: u16) -> EmailError {
        let provider = SmtpEmailProvider::new(&SmtpSettings {
            host: "127.0.0.1".into(),
            port,
            security: SmtpSecurity::None,
            username: Some("hot".into()),
            password: Some("pw".into()),
            timeout: Duration::from_secs(5),
            ca_cert: None,
        })
        .unwrap();
        provider
            .send(
                &Email::new("ops@example.com", "Hello").with_text("body"),
                "Hot <hot@example.com>",
            )
            .await
            .expect_err("the relay refuses the send")
    }

    #[tokio::test]
    async fn a_relay_refusing_the_session_or_the_sender_is_retried_not_rejected() {
        const GREETING: &str = "220 test ESMTP";
        let retried: [(&str, &[(&str, &str)]); 7] = [
            // Rotated or wrong credentials: every queued email meets this.
            (
                GREETING,
                &[("AUTH", "535 5.7.8 Username and Password not accepted")],
            ),
            (
                GREETING,
                &[("AUTH", "535 Authentication Credentials Invalid")],
            ),
            (
                GREETING,
                &[("MAIL", "530 5.7.0 Must issue a STARTTLS command first")],
            ),
            (
                GREETING,
                &[(
                    "MAIL",
                    "550 The from address does not match a verified Sender Identity",
                )],
            ),
            (GREETING, &[("RCPT", "550 5.7.1 Relaying denied")]),
            (
                GREETING,
                &[("RCPT", "451 4.7.1 Greylisted, try again later")],
            ),
            ("554 5.7.1 Service unavailable; client host blocked", &[]),
        ];
        for (greeting, replies) in retried {
            let error = send_through(scripted_relay(greeting, replies).await).await;
            assert!(matches!(error, EmailError::SendFailed(_)), "{error}");
        }

        let rejected: [&[(&str, &str)]; 3] = [
            &[("RCPT", "550 5.1.1 User unknown")],
            // A reply of several lines, as Gmail sends.
            &[(
                "RCPT",
                "550-5.1.1 The email account that you tried to reach\r\n550 5.1.1 does not exist",
            )],
            &[("DATA", "552 5.3.4 Message size exceeds fixed limit")],
        ];
        for replies in rejected {
            let error = send_through(scripted_relay(GREETING, replies).await).await;
            assert!(matches!(error, EmailError::Rejected(_)), "{error}");
            assert!(error.to_string().contains("5."), "{error}");
        }
    }

    /// Sends one message over SMTPS to a relay whose certificate the test CA
    /// issued, trusting `ca_cert` (or the system roots without it).
    async fn send_over_tls(ca_cert: Option<&str>) -> Result<String, EmailError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let certs = CertificateDer::pem_slice_iter(TEST_CERT_PEM.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(TEST_KEY_PEM.as_bytes()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            match acceptor.accept(socket).await {
                Ok(tls) => Some(serve_one_message(tls).await),
                Err(_) => None,
            }
        });
        let mut smtp = val!({"host": "127.0.0.1", "port": i64::from(port), "security": "tls"});
        if let Some(pem) = ca_cert {
            smtp = smtp.merge(&val!({"ca-cert": pem}));
        }
        let settings = SmtpSettings::from_conf(&val!({"email": {"smtp": smtp}})).unwrap();
        let provider = SmtpEmailProvider::new(&settings).unwrap();
        let sent = provider
            .send(
                &Email::new("ops@example.com", "Hello").with_text("over tls"),
                "Hot <hot@example.com>",
            )
            .await;
        drop(provider);
        let data = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the transport closes its connection")
            .unwrap();
        sent.map(|()| data.unwrap_or_default())
    }

    #[tokio::test]
    async fn a_private_certificate_authority_is_trusted_when_configured() {
        let data = send_over_tls(Some(TEST_CA_PEM)).await.unwrap();
        assert!(data.contains("over tls"), "{data}");
        assert!(
            send_over_tls(None).await.is_err(),
            "the test CA is not in the system trust store"
        );
    }
}
