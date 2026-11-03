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
            Err(EmailError::SendFailed(_))
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
