//! TLS for GitImage (design D5): a self-managed CA, serving certificates signed by it, and
//! axum listeners that re-read their mounted certificate every 60 s, so a renewal needs no
//! restart. The crypto provider is ring everywhere; call [`install_crypto_provider`] once at
//! process start.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use time::OffsetDateTime;

/// PEM-encoded certificate and private key.
#[derive(Clone, PartialEq, Eq)]
pub struct CertPair {
    pub cert_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for CertPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CertPair { cert_pem, key_pem: *** }")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("certificate parse: {0}")]
    Parse(String),
}

/// Installs ring as the process-wide rustls provider. Safe to call more than once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A 10-year CA named `gitimage-ca`.
pub fn generate_ca() -> Result<CertPair, TlsError> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "gitimage-ca");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(3650);
    let cert = params.self_signed(&key)?;
    Ok(CertPair {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// A 1-year serving certificate for `dns_names`, signed by `ca`.
pub fn issue_server_cert(ca: &CertPair, dns_names: &[String]) -> Result<CertPair, TlsError> {
    let ca_key = KeyPair::from_pem(&ca.key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&ca.cert_pem, ca_key)?;
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(dns_names.to_vec())?;
    let mut dn = DistinguishedName::new();
    dn.push(
        DnType::CommonName,
        dns_names.first().cloned().unwrap_or_default(),
    );
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(365);
    let cert = params.signed_by(&key, &issuer)?;
    Ok(CertPair {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Seconds until the first certificate in `pem` expires (negative when expired).
pub fn seconds_left(pem: &str) -> Result<i64, TlsError> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| TlsError::Parse(e.to_string()))?;
    let cert = block
        .parse_x509()
        .map_err(|e| TlsError::Parse(e.to_string()))?;
    let not_after = cert.validity().not_after.timestamp();
    Ok(not_after - OffsetDateTime::now_utc().unix_timestamp())
}

/// DNS names the certificate covers (SANs).
pub fn dns_names(pem: &str) -> Result<Vec<String>, TlsError> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| TlsError::Parse(e.to_string()))?;
    let cert = block
        .parse_x509()
        .map_err(|e| TlsError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(n) = name {
                out.push(n.to_string());
            }
        }
    }
    Ok(out)
}

/// `true` when `cert` is missing, signed for other names, or has less than 30 days left.
pub fn needs_renewal(cert_pem: Option<&str>, wanted: &[String]) -> bool {
    let Some(pem) = cert_pem else { return true };
    let Ok(left) = seconds_left(pem) else {
        return true;
    };
    let Ok(names) = dns_names(pem) else {
        return true;
    };
    left < 30 * 86400 || wanted.iter().any(|w| !names.contains(w))
}

/// Where a listener reads its certificate (a mounted `kubernetes.io/tls` Secret).
#[derive(Clone, Debug)]
pub struct CertFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl CertFiles {
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            cert: dir.join("tls.crt"),
            key: dir.join("tls.key"),
        }
    }
}

/// Serves `router` over TLS on `addr` until `shutdown` resolves. Waits for the certificate
/// files to appear (the controller may still be creating the Secret), then re-reads them
/// every 60 s.
pub async fn serve_tls(
    addr: SocketAddr,
    router: axum::Router,
    files: CertFiles,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    install_crypto_provider();
    loop {
        if files.cert.exists() && files.key.exists() {
            break;
        }
        tracing::info!(cert = %files.cert.display(), "waiting for TLS certificate");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&files.cert, &files.key).await?;
    let reload = config.clone();
    let reload_files = files.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = reload
                .reload_from_pem_file(&reload_files.cert, &reload_files.key)
                .await
            {
                tracing::warn!(error = %e, "TLS certificate reload failed; keeping the old one");
            }
        }
    });
    let handle = axum_server::Handle::new();
    let stopper = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        stopper.graceful_shutdown(Some(Duration::from_secs(10)));
    });
    axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(router.into_make_service())
        .await?;
    Ok(())
}

/// Serves `router` over plain HTTP on `addr` until `shutdown` resolves.
pub async fn serve_plain(
    addr: SocketAddr,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_issues_a_server_cert_that_does_not_need_renewal() {
        install_crypto_provider();
        let ca = generate_ca().unwrap();
        let names = vec!["gitimage-webhook.gitimage-system.svc".to_string()];
        let srv = issue_server_cert(&ca, &names).unwrap();
        assert!(!needs_renewal(Some(&srv.cert_pem), &names));
        assert!(needs_renewal(
            Some(&srv.cert_pem),
            &["other.svc".to_string()]
        ));
        assert!(needs_renewal(None, &names));
        let left = seconds_left(&srv.cert_pem).unwrap();
        assert!(left > 364 * 86400 && left <= 365 * 86400, "{left}");
        assert!(seconds_left(&ca.cert_pem).unwrap() > 3600 * 86400);
    }

    #[test]
    fn debug_hides_the_key() {
        let p = CertPair {
            cert_pem: "c".into(),
            key_pem: "SECRET".into(),
        };
        assert!(!format!("{p:?}").contains("SECRET"));
    }

    #[tokio::test]
    async fn serves_https_with_issued_cert() {
        install_crypto_provider();
        let ca = generate_ca().unwrap();
        let srv = issue_server_cert(&ca, &["localhost".into()]).unwrap();
        let dir = std::env::temp_dir().join(format!("gitimage-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tls.crt"), &srv.cert_pem).unwrap();
        std::fs::write(dir.join("tls.key"), &srv.key_pem).unwrap();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let router = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let server = tokio::spawn(serve_tls(addr, router, CertFiles::in_dir(&dir), async {
            let _ = rx.await;
        }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut roots = rustls::RootCertStore::empty();
        let (_, block) = x509_parser::pem::parse_x509_pem(ca.cert_pem.as_bytes()).unwrap();
        roots.add(block.contents.clone().into()).unwrap();
        let cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls_connector(cfg);
        let body = connector(port).await;
        assert!(body.ends_with("ok"), "{body}");
        let _ = tx.send(());
        let _ = server.await;
        std::fs::remove_dir_all(dir).ok();
    }

    /// Minimal HTTPS GET with raw rustls, to avoid a dev-dependency on an HTTP client.
    fn tokio_rustls_connector(
        cfg: rustls::ClientConfig,
    ) -> impl Fn(u16) -> std::pin::Pin<Box<dyn std::future::Future<Output = String>>> {
        let cfg = std::sync::Arc::new(cfg);
        move |port| {
            let cfg = cfg.clone();
            Box::pin(async move {
                use std::io::{Read, Write};
                tokio::task::spawn_blocking(move || {
                    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                    let mut conn = rustls::ClientConnection::new(cfg, name).unwrap();
                    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
                    tls.write_all(
                        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                    let mut out = Vec::new();
                    let _ = tls.read_to_end(&mut out);
                    String::from_utf8_lossy(&out).trim().to_string()
                })
                .await
                .unwrap()
            })
        }
    }
}
