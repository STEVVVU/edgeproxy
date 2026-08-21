//! TLS termination and upstream re-encryption built on `rustls`.
//!
//! edgeproxy uses the pure-Rust `ring` cryptographic provider rather than
//! `aws-lc-rs`. Both are supported by rustls 0.23, but `ring` builds without
//! cmake or NASM, which keeps contributor setup simple on Windows in particular.
//! Neither provider requires `unsafe` in this crate, so `unsafe_code = "forbid"`
//! still holds.
//!
//! Certificates are loaded once at startup. There is no hot reload: like the
//! rest of the configuration, a certificate rotation requires a restart.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsStream;
use tracing::debug;

use crate::config::{TlsListener, UpstreamConfig};

/// A downstream or upstream byte stream that may or may not be wrapped in TLS.
///
/// Both variants are `Unpin`, so the projections below need no `unsafe`.
#[derive(Debug)]
pub enum MaybeTlsStream<S> {
    Plain(S),
    /// Boxed because `TlsStream` embeds rustls connection state and is large
    /// relative to a bare `TcpStream`; boxing keeps the enum small.
    Tls(Box<TlsStream<S>>),
}

impl<S> MaybeTlsStream<S> {
    /// Whether this stream is TLS-protected, for access logging.
    pub const fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for MaybeTlsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for MaybeTlsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Resolves a server certificate from the TLS ClientHello SNI value.
///
/// SNI routing here means *certificate* selection, not upstream selection: a
/// ClientHello naming `a.example` gets `a.example`'s certificate. Choosing a
/// different upstream pool per hostname is host-based routing, which edgeproxy
/// does not implement (see README scope limits).
#[derive(Debug)]
struct SniResolver {
    /// Exact, lowercased SNI hostname to certificate.
    by_name: HashMap<String, Arc<CertifiedKey>>,
    /// Used when SNI is absent or unmatched.
    default: Arc<CertifiedKey>,
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let Some(requested) = client_hello.server_name() else {
            debug!(event = "tls_sni_absent", "serving default certificate");
            return Some(Arc::clone(&self.default));
        };
        // rustls already validated this as a DNS name; lowercase for matching.
        let requested = requested.to_ascii_lowercase();
        match self.by_name.get(&requested) {
            Some(certified) => {
                debug!(event = "tls_sni_match", sni = %requested);
                Some(Arc::clone(certified))
            }
            None => {
                debug!(
                    event = "tls_sni_unmatched",
                    sni = %requested,
                    "serving default certificate"
                );
                Some(Arc::clone(&self.default))
            }
        }
    }
}

/// Builds the rustls server configuration for the HTTPS listener.
///
/// The default certificate is always loaded. Additional `[[tls.sni]]` entries
/// add hostname-specific certificates resolved from the ClientHello.
pub fn server_config(listener: &TlsListener) -> Result<Arc<ServerConfig>> {
    let default = Arc::new(load_certified_key(
        &listener.certificate,
        &listener.private_key,
    )?);

    let mut by_name = HashMap::with_capacity(listener.sni.len());
    for entry in &listener.sni {
        let certified = Arc::new(load_certified_key(&entry.certificate, &entry.private_key)?);
        // Config validation already rejected duplicates and non-lowercase names.
        by_name.insert(entry.server_name.clone(), certified);
    }

    let resolver = SniResolver { by_name, default };
    // `with_safe_default_protocol_versions` yields TLS 1.2 + 1.3; the `tls12`
    // feature is enabled so older clients still connect.
    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("rustls does not support the requested protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    // edgeproxy speaks HTTP/1.1 only, so advertise exactly that. A client that
    // offers only h2 fails the handshake instead of silently downgrading.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Builds the rustls client configuration used for upstream re-encryption.
///
/// Trust anchors come from the `webpki-roots` bundle rather than the platform
/// store, so verification behaves identically on every host regardless of the
/// OS CA configuration.
/// `insecure_skip_verify` exists for self-signed lab upstreams and is refused
/// unless the operator opts in per upstream; it is logged loudly at startup.
pub fn upstream_client_config(insecure_skip_verify: bool) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let builder = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("rustls does not support the requested protocol versions")?;

    let mut config = if insecure_skip_verify {
        let verifier = Arc::new(NoVerification {
            provider: Arc::new(ring::default_provider()),
        });
        builder
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth()
    } else {
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The `ServerName` presented to an upstream during the TLS handshake.
///
/// Defaults to the configured `tls_server_name`; without one, the upstream's IP
/// literal is used, which only validates if the certificate carries an IP SAN.
pub fn upstream_server_name(upstream: &UpstreamConfig) -> Result<ServerName<'static>> {
    match &upstream.tls_server_name {
        Some(name) => ServerName::try_from(name.clone())
            .with_context(|| format!("invalid tls_server_name for upstream {}", upstream.name)),
        None => Ok(ServerName::from(upstream.address.ip())),
    }
}

/// Loads a PEM certificate chain and its private key into a rustls signing key.
fn load_certified_key(certificate: &Path, private_key: &Path) -> Result<CertifiedKey> {
    let chain = load_certificate_chain(certificate)?;
    let key = load_private_key(private_key)?;
    let signing_key = ring::sign::any_supported_type(&key)
        .with_context(|| format!("unsupported private key type in {}", private_key.display()))?;
    Ok(CertifiedKey::new(chain, signing_key))
}

fn load_certificate_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open certificate {}", path.display()))?;
    let chain = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse certificates in {}", path.display()))?;
    anyhow::ensure!(
        !chain.is_empty(),
        "no PEM certificate found in {}",
        path.display()
    );
    Ok(chain)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open private key {}", path.display()))?;
    // Accepts PKCS#8, PKCS#1 (RSA), and SEC1 (EC) encodings.
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .with_context(|| format!("failed to parse private key in {}", path.display()))?
        .with_context(|| format!("no PEM private key found in {}", path.display()))
}

/// Certificate verifier that accepts any server certificate.
///
/// Reachable only through `upstreams[].tls_insecure_skip_verify`, which exists
/// so a self-signed lab upstream can be exercised end to end. Signature and
/// protocol-level checks still run; only the trust-chain and name checks are
/// skipped. This does not use `unsafe`; `dangerous()` is a rustls naming
/// convention that marks the security downgrade.
#[derive(Debug)]
struct NoVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Writes a throwaway self-signed certificate/key pair to a temp directory.
    fn write_test_pair(
        dir: &Path,
        stem: &str,
        names: &[&str],
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let generated = rcgen::generate_simple_self_signed(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .expect("generate test certificate");
        let certificate = dir.join(format!("{stem}.crt"));
        let private_key = dir.join(format!("{stem}.key"));
        fs::File::create(&certificate)
            .expect("create cert")
            .write_all(generated.cert.pem().as_bytes())
            .expect("write cert");
        fs::File::create(&private_key)
            .expect("create key")
            .write_all(generated.signing_key.serialize_pem().as_bytes())
            .expect("write key");
        (certificate, private_key)
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("edgeproxy-tls-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn loads_a_self_signed_pem_pair() {
        let dir = temp_dir("load");
        let (certificate, private_key) = write_test_pair(&dir, "default", &["localhost"]);
        let listener = TlsListener {
            listen: "127.0.0.1:0".parse().expect("address"),
            certificate,
            private_key,
            sni: Vec::new(),
        };
        assert!(server_config(&listener).is_ok());
    }

    #[test]
    fn rejects_a_certificate_file_that_is_not_pem() {
        let dir = temp_dir("garbage");
        let certificate = dir.join("garbage.crt");
        let private_key = dir.join("garbage.key");
        fs::write(&certificate, b"not a certificate").expect("write cert");
        fs::write(&private_key, b"not a key").expect("write key");
        let listener = TlsListener {
            listen: "127.0.0.1:0".parse().expect("address"),
            certificate,
            private_key,
            sni: Vec::new(),
        };
        assert!(server_config(&listener).is_err());
    }

    #[test]
    fn sni_resolver_falls_back_to_the_default_certificate() {
        let dir = temp_dir("sni");
        let (default_cert, default_key) = write_test_pair(&dir, "sni-default", &["default.test"]);
        let (alt_cert, alt_key) = write_test_pair(&dir, "sni-alt", &["alt.test"]);
        let listener = TlsListener {
            listen: "127.0.0.1:0".parse().expect("address"),
            certificate: default_cert,
            private_key: default_key,
            sni: vec![crate::config::SniCertificate {
                server_name: "alt.test".to_owned(),
                certificate: alt_cert,
                private_key: alt_key,
            }],
        };
        // Building succeeds and both certificates are registered; the resolver
        // itself is exercised end to end by tests/tls.rs over a real handshake.
        assert!(server_config(&listener).is_ok());
    }

    #[test]
    fn upstream_server_name_prefers_the_configured_name() {
        let upstream = UpstreamConfig {
            name: "one".to_owned(),
            address: "127.0.0.1:9001".parse().expect("address"),
            health_path: "/health".to_owned(),
            tls: true,
            tls_server_name: Some("api.internal".to_owned()),
            tls_insecure_skip_verify: false,
        };
        let name = upstream_server_name(&upstream).expect("server name");
        assert_eq!(name, ServerName::try_from("api.internal").expect("name"));
    }

    #[test]
    fn upstream_server_name_falls_back_to_the_address_literal() {
        let upstream = UpstreamConfig {
            name: "one".to_owned(),
            address: "127.0.0.1:9001".parse().expect("address"),
            health_path: "/health".to_owned(),
            tls: true,
            tls_server_name: None,
            tls_insecure_skip_verify: false,
        };
        let name = upstream_server_name(&upstream).expect("server name");
        assert!(matches!(name, ServerName::IpAddress(_)));
    }
}
