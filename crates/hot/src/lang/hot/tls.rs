//! TLS upgrade module for ::hot::tls functions
//!
//! Upgrades an existing ::hot::tcp connection to TLS in place (STARTTLS
//! style). This is how Postgres, SMTP, and friends negotiate TLS: the
//! client sends a protocol-level request on the plain socket, then both
//! sides start the handshake on the same connection.
//!
//! Verification defaults to full certificate + hostname verification
//! against the system webpki roots. `mode: "insecure"` disables
//! verification entirely and must be an explicit opt-in.

use crate::lang::hot::tcp::{TcpStreamState, build_conn_map, extract_handle};
use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

// ----------------------------------------------------------------------------
// Insecure verifier (mode: "insecure") — accepts any certificate
// ----------------------------------------------------------------------------

#[derive(Debug)]
struct NoCertVerification(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ----------------------------------------------------------------------------
// Option parsing
// ----------------------------------------------------------------------------

struct UpgradeOpts {
    server_name: Option<String>,
    insecure: bool,
    ca_pem: Option<String>,
    client_cert_pem: Option<String>,
    client_key_pem: Option<String>,
}

fn get_str(m: &indexmap::IndexMap<Val, Val>, key: &str) -> Option<String> {
    match m.get(&Val::from(key)) {
        Some(Val::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn parse_opts(fn_name: &str, opts: Option<&Val>) -> Result<UpgradeOpts, Val> {
    let m = match opts {
        Some(Val::Map(m)) => m,
        Some(Val::Null) | None => {
            return Ok(UpgradeOpts {
                server_name: None,
                insecure: false,
                ca_pem: None,
                client_cert_pem: None,
                client_key_pem: None,
            });
        }
        Some(_) => return Err(err_val(format!("{}: options must be a map", fn_name))),
    };

    let insecure = match m.get(&Val::from("mode")) {
        Some(Val::Str(s)) => match &**s {
            "verify-full" => false,
            "insecure" => true,
            other => {
                return Err(err_val(format!(
                    "{}: invalid mode '{}'. Use 'verify-full' or 'insecure'",
                    fn_name, other
                )));
            }
        },
        None => false,
        Some(_) => {
            return Err(err_val(format!("{}: mode must be a string", fn_name)));
        }
    };

    let client_cert_pem = get_str(m, "client-cert-pem");
    let client_key_pem = get_str(m, "client-key-pem");
    if client_cert_pem.is_some() != client_key_pem.is_some() {
        return Err(err_val(format!(
            "{}: client-cert-pem and client-key-pem must be provided together",
            fn_name
        )));
    }

    Ok(UpgradeOpts {
        server_name: get_str(m, "server-name"),
        insecure,
        ca_pem: get_str(m, "ca-pem"),
        client_cert_pem,
        client_key_pem,
    })
}

fn build_client_config(fn_name: &str, opts: &UpgradeOpts) -> Result<rustls::ClientConfig, Val> {
    let provider = crypto_provider();

    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| err_val(format!("{}: TLS config error: {}", fn_name, e)))?;

    let builder = if opts.insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerification(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        if let Some(ca_pem) = &opts.ca_pem {
            let mut added = 0usize;
            for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                let cert =
                    cert.map_err(|e| err_val(format!("{}: invalid ca-pem: {:?}", fn_name, e)))?;
                roots
                    .add(cert)
                    .map_err(|e| err_val(format!("{}: invalid ca-pem cert: {}", fn_name, e)))?;
                added += 1;
            }
            if added == 0 {
                return Err(err_val(format!(
                    "{}: ca-pem contained no certificates",
                    fn_name
                )));
            }
        }

        builder.with_root_certificates(roots)
    };

    let config = match (&opts.client_cert_pem, &opts.client_key_pem) {
        (Some(cert_pem), Some(key_pem)) => {
            let certs: Vec<CertificateDer<'static>> =
                CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                    .collect::<Result<_, _>>()
                    .map_err(|e| {
                        err_val(format!("{}: invalid client-cert-pem: {:?}", fn_name, e))
                    })?;
            let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                .map_err(|e| err_val(format!("{}: invalid client-key-pem: {:?}", fn_name, e)))?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| err_val(format!("{}: client cert error: {}", fn_name, e)))?
        }
        _ => builder.with_no_client_auth(),
    };

    Ok(config)
}

// ----------------------------------------------------------------------------
// Public API functions
// ----------------------------------------------------------------------------

/// Upgrade an open ::hot::tcp connection to TLS in place.
///
/// # Arguments
/// * 1 arg:  connection
/// * 2 args: connection, options (Map: `server-name`, `mode`, `ca-pem`,
///   `client-cert-pem`, `client-key-pem`)
///
/// `server-name` defaults to the host the connection was opened with.
/// `mode` is `"verify-full"` (default) or `"insecure"`.
///
/// # Returns
/// The connection map with `tls: true`. The underlying handle is shared:
/// after a successful upgrade, reads/writes through the original value
/// also use TLS. On handshake failure the connection is closed.
pub fn upgrade(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tls/upgrade";

    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(err_val(format!(
            "{}: expected 1-2 args (connection [, options])",
            FN
        )));
    }

    let handle = match extract_handle(FN, &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    if handle.inner.closed.load(Ordering::Relaxed) {
        return HotResult::Err(err_val(format!("{}: connection is closed", FN)));
    }

    let opts = match parse_opts(FN, args.get(1)) {
        Ok(o) => o,
        Err(e) => return HotResult::Err(e),
    };

    let server_name_str = opts
        .server_name
        .clone()
        .unwrap_or_else(|| handle.inner.host.clone());

    let server_name = match ServerName::try_from(server_name_str.as_str()) {
        Ok(sn) => sn.to_owned(),
        Err(e) => {
            return HotResult::Err(err_val(format!(
                "{}: invalid server-name '{}': {}",
                FN, server_name_str, e
            )));
        }
    };

    let config = match build_client_config(FN, &opts) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(e),
    };

    let port = match &args[0] {
        Val::Map(m) => match m.get(&Val::from("port")) {
            Some(Val::Int(p)) => *p,
            _ => 0,
        },
        _ => 0,
    };

    let inner = Arc::clone(&handle.inner);
    let result = tokio::runtime::Handle::current().block_on(async {
        let mut guard = inner.stream.lock().await;

        let plain = match std::mem::replace(&mut *guard, TcpStreamState::Closed) {
            TcpStreamState::Plain(s) => s,
            TcpStreamState::Tls(s) => {
                *guard = TcpStreamState::Tls(s);
                return Err(err_val(format!("{}: connection is already TLS", FN)));
            }
            TcpStreamState::Closed => {
                return Err(err_val(format!("{}: connection is closed", FN)));
            }
        };

        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        match connector.connect(server_name, plain).await {
            Ok(tls_stream) => {
                // Record the server's leaf certificate for peer-cert-hash
                if let Some(leaf) = tls_stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|certs| certs.first())
                    && let Ok(mut cert_guard) = inner.peer_cert.lock()
                {
                    *cert_guard = Some(leaf.as_ref().to_vec());
                }
                *guard = TcpStreamState::Tls(Box::new(tls_stream));
                Ok(())
            }
            Err(e) => {
                // The plain stream was consumed by the failed handshake;
                // the connection is unusable.
                inner.closed.store(true, Ordering::Relaxed);
                Err(err_val(format!("{}: TLS handshake failed: {}", FN, e)))
            }
        }
    });

    match result {
        Ok(()) => HotResult::Ok(build_conn_map(&inner, port, true)),
        Err(e) => HotResult::Err(e),
    }
}

/// Hash of the server's leaf certificate (DER), for SCRAM channel binding
/// (tls-server-end-point) and certificate pinning.
///
/// # Arguments
/// * 1 arg:  connection (must have been upgraded to TLS)
/// * 2 args: connection, algorithm ("sha256" (default), "sha384", "sha512")
pub fn peer_cert_hash(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::tls/peer-cert-hash";

    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(err_val(format!(
            "{}: expected 1-2 args (connection [, algorithm])",
            FN
        )));
    }

    let handle = match extract_handle(FN, &args[0]) {
        Ok(h) => h,
        Err(e) => return HotResult::Err(e),
    };

    let algorithm = match args.get(1) {
        Some(Val::Str(s)) => match &**s {
            "sha256" => &aws_lc_rs::digest::SHA256,
            "sha384" => &aws_lc_rs::digest::SHA384,
            "sha512" => &aws_lc_rs::digest::SHA512,
            other => {
                return HotResult::Err(err_val(format!(
                    "{}: unsupported algorithm '{}'. Use sha256, sha384, or sha512",
                    FN, other
                )));
            }
        },
        None => &aws_lc_rs::digest::SHA256,
        Some(_) => {
            return HotResult::Err(err_val(format!("{}: algorithm must be a string", FN)));
        }
    };

    let cert = match handle.inner.peer_cert.lock() {
        Ok(guard) => guard.clone(),
        Err(e) => return HotResult::Err(err_val(format!("{}: lock error: {}", FN, e))),
    };

    match cert {
        Some(der) => {
            let digest = aws_lc_rs::digest::digest(algorithm, &der);
            HotResult::Ok(Val::Bytes(digest.as_ref().to_vec()))
        }
        None => HotResult::Err(err_val(format!(
            "{}: no peer certificate (connection has not been upgraded to TLS)",
            FN
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::hot::tcp;
    use indexmap::IndexMap;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    // Test PKI fixture, valid for 100 years: a "Hot Test CA" root and a
    // CN=localhost leaf (SAN DNS:localhost + IP:127.0.0.1) signed by it.
    // Test fixture only — the private key is not a secret.
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

    fn unwrap_ok(result: HotResult<Val>) -> Val {
        match result {
            HotResult::Ok(v) => v,
            HotResult::Err(e) => panic!("Expected Ok, got Err: {:?}", e),
        }
    }

    fn expect_err(result: HotResult<Val>) -> Val {
        match result {
            HotResult::Err(e) => e,
            HotResult::Ok(v) => panic!("Expected Err, got Ok: {:?}", v),
        }
    }

    /// Start a TLS echo server with the test certificate; returns its port.
    async fn start_tls_echo_server() -> u16 {
        let certs: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(TEST_CERT_PEM.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(TEST_KEY_PEM.as_bytes()).unwrap();

        let config = rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut buf = [0u8; 4096];
                    loop {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if tls.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    fn connect_plain(port: u16) -> Val {
        unwrap_ok(tcp::connect(&[
            Val::from("localhost"),
            Val::Int(port as i64),
        ]))
    }

    fn opts_map(pairs: &[(&str, Val)]) -> Val {
        let mut m: IndexMap<Val, Val> = IndexMap::new();
        for (k, v) in pairs {
            m.insert(Val::from(*k), v.clone());
        }
        Val::Map(Box::new(m))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_upgrade_verify_full_with_custom_ca() {
        let port = start_tls_echo_server().await;

        tokio::task::spawn_blocking(move || {
            let conn = connect_plain(port);
            let opts = opts_map(&[
                ("server-name", Val::from("localhost")),
                ("ca-pem", Val::from(TEST_CA_PEM)),
            ]);
            let tls_conn = unwrap_ok(upgrade(&[conn, opts]));

            // Map is flagged as TLS
            if let Val::Map(m) = &tls_conn {
                assert_eq!(m.get(&Val::from("tls")), Some(&Val::Bool(true)));
            } else {
                panic!("Expected connection map");
            }

            // Echo through the TLS stream
            unwrap_ok(tcp::write(&[tls_conn.clone(), Val::Bytes(vec![9, 8, 7])]));
            let echoed = unwrap_ok(tcp::read_exact(&[tls_conn.clone(), Val::Int(3)]));
            assert_eq!(echoed, Val::Bytes(vec![9, 8, 7]));

            // peer-cert-hash matches sha256 of the certificate DER
            let expected: Vec<CertificateDer<'static>> =
                CertificateDer::pem_slice_iter(TEST_CERT_PEM.as_bytes())
                    .collect::<Result<_, _>>()
                    .unwrap();
            let expected_hash =
                aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, expected[0].as_ref());
            let hash = unwrap_ok(peer_cert_hash(std::slice::from_ref(&tls_conn)));
            assert_eq!(hash, Val::Bytes(expected_hash.as_ref().to_vec()));

            // Double upgrade errors
            let err = expect_err(upgrade(std::slice::from_ref(&tls_conn)));
            assert!(err.to_string().contains("already TLS"), "got: {:?}", err);

            unwrap_ok(tcp::close(&[tls_conn]));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_upgrade_verify_full_rejects_untrusted_cert() {
        let port = start_tls_echo_server().await;

        tokio::task::spawn_blocking(move || {
            let conn = connect_plain(port);
            // Default mode with system roots only: self-signed cert must fail
            let err = expect_err(upgrade(std::slice::from_ref(&conn)));
            assert!(
                err.to_string().contains("handshake failed"),
                "got: {:?}",
                err
            );
            // Failed handshake closes the connection
            let open = unwrap_ok(tcp::is_open(&[conn]));
            assert_eq!(open, Val::Bool(false));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_upgrade_insecure_mode() {
        let port = start_tls_echo_server().await;

        tokio::task::spawn_blocking(move || {
            let conn = connect_plain(port);
            let opts = opts_map(&[("mode", Val::from("insecure"))]);
            let tls_conn = unwrap_ok(upgrade(&[conn, opts]));

            unwrap_ok(tcp::write(&[tls_conn.clone(), Val::from("ping")]));
            let echoed = unwrap_ok(tcp::read_exact(&[tls_conn.clone(), Val::Int(4)]));
            assert_eq!(echoed, Val::Bytes(b"ping".to_vec()));

            unwrap_ok(tcp::close(&[tls_conn]));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_peer_cert_hash_before_upgrade_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        tokio::task::spawn_blocking(move || {
            let conn = connect_plain(port);
            let err = expect_err(peer_cert_hash(std::slice::from_ref(&conn)));
            assert!(
                err.to_string().contains("no peer certificate"),
                "got: {:?}",
                err
            );
            unwrap_ok(tcp::close(&[conn]));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_upgrade_opt_validation() {
        tokio::task::spawn_blocking(move || {
            expect_err(upgrade(&[Val::from("not-a-conn")]));
            // client cert without key is rejected before any IO
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let conn = connect_plain(port);
            let opts = opts_map(&[("client-cert-pem", Val::from(TEST_CERT_PEM))]);
            let err = expect_err(upgrade(&[conn.clone(), opts]));
            assert!(
                err.to_string().contains("must be provided together"),
                "got: {:?}",
                err
            );
            // bad mode
            let opts = opts_map(&[("mode", Val::from("yolo"))]);
            expect_err(upgrade(&[conn.clone(), opts]));
            unwrap_ok(tcp::close(&[conn]));
        })
        .await
        .unwrap();
    }
}
