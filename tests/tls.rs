//! End-to-end TLS tests: real rustls handshakes over real sockets.
//!
//! Certificates are generated at test time (see `support/certs.rs`); no key
//! material is committed. Each test builds a client that trusts only the
//! certificate it just generated, so a broken SNI or trust path fails loudly
//! rather than silently falling back.

mod support {
    pub mod certs;
}

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use edgeproxy::config::{
    CircuitBreaker, HealthCheck, Limits, Policy, Retries, SniCertificate, Timeouts, TlsListener,
    UpstreamConfig,
};
use edgeproxy::{BoundProxy, Config};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use support::certs::{generate, temp_dir, TestCertificate};

/// A plaintext HTTP/1.1 upstream that always answers with `body`.
struct Fixture {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(body: &'static str) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let address = listener.local_addr().expect("fixture address");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let _ = timeout(Duration::from_secs(1), stream.read(&mut request)).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\
                     content-type: text/plain\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    Fixture { address, task }
}

struct TestProxy {
    tls_address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestProxy {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        timeout(Duration::from_secs(5), self.task)
            .await
            .expect("proxy shutdown timeout")
            .expect("proxy task panicked")
            .expect("proxy shutdown failed");
    }
}

fn base_config(upstreams: Vec<SocketAddr>, tls: Option<TlsListener>) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().expect("address"),
        metrics_listen: "127.0.0.1:0".parse().expect("address"),
        tls,
        policy: Policy::RoundRobin,
        max_in_flight: 32,
        shutdown_grace_ms: 1000,
        preserve_host: false,
        limits: Limits::default(),
        timeouts: Timeouts {
            request_header_ms: 2000,
            request_body_ms: 2000,
            connect_ms: 1000,
            upstream_write_ms: 1000,
            upstream_response_header_ms: 1000,
            upstream_idle_ms: 1000,
            downstream_write_ms: 1000,
        },
        retries: Retries {
            max_attempts: 1,
            retry_statuses: vec![502, 503, 504],
        },
        circuit_breaker: CircuitBreaker {
            failure_threshold: 10,
            open_ms: 1000,
        },
        health_check: HealthCheck {
            // Long interval: these tests assert on request behavior, not probes.
            interval_ms: 60_000,
            timeout_ms: 500,
            unhealthy_threshold: 2,
            healthy_threshold: 1,
        },
        upstreams: upstreams
            .into_iter()
            .enumerate()
            .map(|(index, address)| UpstreamConfig {
                name: format!("upstream-{index}"),
                address,
                health_path: "/health".to_owned(),
                tls: false,
                tls_server_name: None,
                tls_insecure_skip_verify: false,
            })
            .collect(),
    }
}

async fn spawn_proxy(config: Config) -> TestProxy {
    let proxy = BoundProxy::bind(config).await.expect("proxy bind");
    let tls_address = proxy
        .tls_addr()
        .expect("tls address")
        .expect("TLS listener configured");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(proxy.run(async move {
        let _ = shutdown_rx.await;
    }));
    TestProxy {
        tls_address,
        shutdown: Some(shutdown_tx),
        task,
    }
}

/// Builds a client that trusts exactly the supplied certificates.
fn client_config(trusted: &[&TestCertificate]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for certificate in trusted {
        roots
            .add(CertificateDer::from(certificate.certificate_der.clone()))
            .expect("add test root");
    }
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Performs one HTTPS request and returns the raw response bytes.
async fn https_exchange(
    address: SocketAddr,
    server_name: &str,
    config: Arc<ClientConfig>,
    request: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let stream = TcpStream::connect(address).await?;
    let name = ServerName::try_from(server_name.to_owned())?;
    let mut tls = TlsConnector::from(config).connect(name, stream).await?;
    tls.write_all(request).await?;
    tls.flush().await?;
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), tls.read_to_end(&mut response)).await??;
    Ok(response)
}

fn get_request(path: &str, host: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nhost: {host}\r\n\r\n").into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminates_tls_and_proxies_to_a_plaintext_upstream() {
    let dir = temp_dir("terminate");
    let certificate = generate(&dir, "default", &["localhost"]);
    let upstream = fixture("tls-ok").await;

    let mut config = base_config(vec![upstream.address], None);
    config.tls = Some(TlsListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        certificate: certificate.certificate_path.clone(),
        private_key: certificate.private_key_path.clone(),
        sni: Vec::new(),
    });
    let proxy = spawn_proxy(config).await;

    let response = https_exchange(
        proxy.tls_address,
        "localhost",
        client_config(&[&certificate]),
        &get_request("/secure", "localhost"),
    )
    .await
    .expect("https exchange");

    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert!(response.ends_with(b"tls-ok"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sni_selects_the_matching_certificate() {
    let dir = temp_dir("sni");
    let default_certificate = generate(&dir, "default", &["default.test"]);
    let alt_certificate = generate(&dir, "alt", &["alt.test"]);
    let upstream = fixture("sni-ok").await;

    let mut config = base_config(vec![upstream.address], None);
    config.tls = Some(TlsListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        certificate: default_certificate.certificate_path.clone(),
        private_key: default_certificate.private_key_path.clone(),
        sni: vec![SniCertificate {
            server_name: "alt.test".to_owned(),
            certificate: alt_certificate.certificate_path.clone(),
            private_key: alt_certificate.private_key_path.clone(),
        }],
    });
    let proxy = spawn_proxy(config).await;

    // A client trusting only the SNI certificate reaches alt.test, which proves
    // the resolver served that certificate and not the default one.
    let response = https_exchange(
        proxy.tls_address,
        "alt.test",
        client_config(&[&alt_certificate]),
        &get_request("/sni", "alt.test"),
    )
    .await
    .expect("sni exchange");
    assert!(response.starts_with(b"HTTP/1.1 200"));

    // The default certificate still serves an unmatched name.
    let response = https_exchange(
        proxy.tls_address,
        "default.test",
        client_config(&[&default_certificate]),
        &get_request("/default", "default.test"),
    )
    .await
    .expect("default exchange");
    assert!(response.starts_with(b"HTTP/1.1 200"));

    // Trusting only the default certificate must fail for alt.test, since the
    // resolver hands back the alt certificate there.
    let mismatch = https_exchange(
        proxy.tls_address,
        "alt.test",
        client_config(&[&default_certificate]),
        &get_request("/sni", "alt.test"),
    )
    .await;
    assert!(mismatch.is_err(), "untrusted certificate must be rejected");

    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_untrusted_certificate_is_rejected_by_the_client() {
    let dir = temp_dir("untrusted");
    let served = generate(&dir, "served", &["localhost"]);
    let other = generate(&dir, "other", &["localhost"]);
    let upstream = fixture("nope").await;

    let mut config = base_config(vec![upstream.address], None);
    config.tls = Some(TlsListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        certificate: served.certificate_path.clone(),
        private_key: served.private_key_path.clone(),
        sni: Vec::new(),
    });
    let proxy = spawn_proxy(config).await;

    let result = https_exchange(
        proxy.tls_address,
        "localhost",
        // Trusts a different certificate than the one the proxy serves.
        client_config(&[&other]),
        &get_request("/", "localhost"),
    )
    .await;
    assert!(
        result.is_err(),
        "handshake must fail for an untrusted chain"
    );

    proxy.stop().await;
}

/// The framing and smuggling defenses must apply identically over TLS.
///
/// A CL+TE request arriving over HTTPS has to be rejected by the same parser
/// that rejects it in plaintext; TLS termination must not open a second,
/// less-strict path into the proxy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smuggling_defenses_apply_over_tls() {
    let dir = temp_dir("smuggling");
    let certificate = generate(&dir, "default", &["localhost"]);
    let upstream = fixture("should-not-arrive").await;

    let mut config = base_config(vec![upstream.address], None);
    config.tls = Some(TlsListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        certificate: certificate.certificate_path.clone(),
        private_key: certificate.private_key_path.clone(),
        sni: Vec::new(),
    });
    let proxy = spawn_proxy(config).await;
    let client = client_config(&[&certificate]);

    // Content-Length plus Transfer-Encoding.
    let response = https_exchange(
        proxy.tls_address,
        "localhost",
        Arc::clone(&client),
        b"POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\
          transfer-encoding: chunked\r\n\r\ndata",
    )
    .await
    .expect("cl+te exchange");
    assert!(
        response.starts_with(b"HTTP/1.1 400"),
        "CL+TE must be rejected over TLS"
    );

    // Conflicting duplicate Content-Length.
    let response = https_exchange(
        proxy.tls_address,
        "localhost",
        Arc::clone(&client),
        b"POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\
          content-length: 5\r\n\r\ndata",
    )
    .await
    .expect("duplicate cl exchange");
    assert!(
        response.starts_with(b"HTTP/1.1 400"),
        "duplicate Content-Length must be rejected over TLS"
    );

    // Missing Host.
    let response = https_exchange(
        proxy.tls_address,
        "localhost",
        Arc::clone(&client),
        b"GET / HTTP/1.1\r\naccept: */*\r\n\r\n",
    )
    .await
    .expect("missing host exchange");
    assert!(
        response.starts_with(b"HTTP/1.1 400"),
        "missing Host must be rejected over TLS"
    );

    // Two Host headers.
    let response = https_exchange(
        proxy.tls_address,
        "localhost",
        client,
        b"GET / HTTP/1.1\r\nhost: localhost\r\nhost: evil.test\r\n\r\n",
    )
    .await
    .expect("duplicate host exchange");
    assert!(
        response.starts_with(b"HTTP/1.1 400"),
        "duplicate Host must be rejected over TLS"
    );

    proxy.stop().await;
}

/// The plaintext listener keeps working when TLS is also configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_and_tls_listeners_serve_concurrently() {
    let dir = temp_dir("both");
    let certificate = generate(&dir, "default", &["localhost"]);
    let upstream = fixture("both-ok").await;

    let mut config = base_config(vec![upstream.address], None);
    config.tls = Some(TlsListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        certificate: certificate.certificate_path.clone(),
        private_key: certificate.private_key_path.clone(),
        sni: Vec::new(),
    });
    let proxy = BoundProxy::bind(config).await.expect("proxy bind");
    let plain_address = proxy.listen_addr().expect("plain address");
    let tls_address = proxy
        .tls_addr()
        .expect("tls address")
        .expect("tls listener");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(proxy.run(async move {
        let _ = shutdown_rx.await;
    }));

    // Plaintext path.
    let mut stream = TcpStream::connect(plain_address).await.expect("connect");
    stream
        .write_all(&get_request("/plain", "localhost"))
        .await
        .expect("write");
    stream.shutdown().await.expect("shutdown");
    let mut plain_response = Vec::new();
    timeout(
        Duration::from_secs(5),
        stream.read_to_end(&mut plain_response),
    )
    .await
    .expect("plain timeout")
    .expect("plain read");
    assert!(plain_response.starts_with(b"HTTP/1.1 200"));

    // TLS path.
    let tls_response = https_exchange(
        tls_address,
        "localhost",
        client_config(&[&certificate]),
        &get_request("/secure", "localhost"),
    )
    .await
    .expect("tls exchange");
    assert!(tls_response.starts_with(b"HTTP/1.1 200"));

    let _ = shutdown_tx.send(());
    timeout(Duration::from_secs(5), task)
        .await
        .expect("shutdown timeout")
        .expect("proxy task")
        .expect("proxy result");
}

/// A bad certificate path must fail at bind time, not at first request.
#[tokio::test]
async fn a_missing_certificate_fails_at_bind() {
    let upstream = fixture("never").await;
    let mut config = base_config(vec![upstream.address], None);
    config.tls = Some(TlsListener {
        listen: "127.0.0.1:0".parse().expect("address"),
        certificate: "does/not/exist.crt".into(),
        private_key: "does/not/exist.key".into(),
        sni: Vec::new(),
    });
    assert!(BoundProxy::bind(config).await.is_err());
}
