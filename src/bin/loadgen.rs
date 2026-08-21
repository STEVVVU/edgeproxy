use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::ClientConfig;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

#[derive(Debug, Parser)]
#[command(about = "Connection-per-request HTTP/1.1 benchmark client for edgeproxy")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:18080")]
    address: SocketAddr,
    #[arg(long, default_value_t = 20_000)]
    requests: usize,
    #[arg(long, default_value_t = 128)]
    concurrency: usize,
    #[arg(long, default_value = "/benchmark")]
    path: String,
    #[arg(long, default_value_t = 5_000)]
    timeout_ms: u64,
    /// Measure the HTTPS listener instead of the plaintext one.
    ///
    /// Certificate verification is skipped because the benchmark serves a
    /// throwaway self-signed certificate; the handshake still performs full key
    /// exchange and signing, which is what the measurement is about.
    #[arg(long, default_value_t = false)]
    tls: bool,
    /// SNI hostname sent when `--tls` is set.
    #[arg(long, default_value = "localhost")]
    tls_server_name: String,
}

#[derive(Serialize)]
struct Report {
    address: SocketAddr,
    /// "http" or "https", so a report file is never ambiguous about which
    /// listener produced it.
    scheme: &'static str,
    requests: usize,
    succeeded: usize,
    failed: usize,
    concurrency: usize,
    elapsed_seconds: f64,
    requests_per_second: f64,
    latency_ms_avg: f64,
    latency_ms_p50: f64,
    latency_ms_p95: f64,
    latency_ms_p99: f64,
    methodology: &'static str,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.requests > 0, "requests must be positive");
    anyhow::ensure!(args.concurrency > 0, "concurrency must be positive");
    anyhow::ensure!(args.path.starts_with('/'), "path must start with '/'");

    let next = Arc::new(AtomicUsize::new(0));
    // `host` matches the SNI name under TLS so the request is well-formed for a
    // name-based origin; the proxy rewrites it before forwarding either way.
    let host = if args.tls {
        args.tls_server_name.clone()
    } else {
        "benchmark".to_owned()
    };
    let request = Arc::new(format!(
        "GET {} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n",
        args.path
    ));
    let tls = if args.tls {
        Some(TlsClient {
            connector: TlsConnector::from(insecure_client_config()?),
            server_name: ServerName::try_from(args.tls_server_name.clone())
                .context("invalid --tls-server-name")?,
        })
    } else {
        None
    };
    let tls = Arc::new(tls);
    let started = Instant::now();
    let mut tasks = JoinSet::new();

    for _ in 0..args.concurrency.min(args.requests) {
        let next = Arc::clone(&next);
        let request = Arc::clone(&request);
        let tls = Arc::clone(&tls);
        let address = args.address;
        let request_count = args.requests;
        let request_timeout = Duration::from_millis(args.timeout_ms);
        tasks.spawn(async move {
            let mut latencies = Vec::new();
            let mut succeeded = 0;
            let mut failed = 0;
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= request_count {
                    break;
                }
                let request_started = Instant::now();
                let attempt = timeout(
                    request_timeout,
                    exchange(address, request.as_bytes(), tls.as_ref().as_ref()),
                )
                .await;
                match attempt {
                    Ok(Ok(true)) => {
                        succeeded += 1;
                        latencies.push(request_started.elapsed().as_secs_f64() * 1000.0);
                    }
                    Ok(Ok(false) | Err(_)) | Err(_) => failed += 1,
                }
            }
            (succeeded, failed, latencies)
        });
    }

    let mut succeeded = 0;
    let mut failed = 0;
    let mut latencies = Vec::with_capacity(args.requests);
    while let Some(result) = tasks.join_next().await {
        let (worker_succeeded, worker_failed, worker_latencies) =
            result.context("load worker panicked")?;
        succeeded += worker_succeeded;
        failed += worker_failed;
        latencies.extend(worker_latencies);
    }

    let elapsed = started.elapsed().as_secs_f64();
    latencies.sort_by(f64::total_cmp);
    let average = if latencies.is_empty() {
        0.0
    } else {
        latencies.iter().sum::<f64>() / latencies.len() as f64
    };
    let report = Report {
        address: args.address,
        scheme: if args.tls { "https" } else { "http" },
        requests: args.requests,
        succeeded,
        failed,
        concurrency: args.concurrency,
        elapsed_seconds: elapsed,
        requests_per_second: succeeded as f64 / elapsed,
        latency_ms_avg: average,
        latency_ms_p50: percentile(&latencies, 0.50),
        latency_ms_p95: percentile(&latencies, 0.95),
        latency_ms_p99: percentile(&latencies, 0.99),
        methodology: if args.tls {
            "HTTPS connection-per-request; full TLS handshake per request; fixed local upstream response"
        } else {
            "HTTP/1.1 connection-per-request; fixed local upstream response"
        },
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    if failed > 0 {
        anyhow::bail!("{failed} requests failed");
    }
    Ok(())
}

/// TLS parameters shared by every worker task.
struct TlsClient {
    connector: TlsConnector,
    server_name: ServerName<'static>,
}

/// Performs one request/response exchange, over TLS when `tls` is supplied.
///
/// Every request opens a fresh connection, so under `--tls` each measurement
/// includes a full handshake. That is the intended comparison: it isolates the
/// cost TLS adds to the connection-per-request model the proxy uses.
async fn exchange(address: SocketAddr, request: &[u8], tls: Option<&TlsClient>) -> Result<bool> {
    let stream = TcpStream::connect(address).await?;
    stream.set_nodelay(true)?;

    let mut response = Vec::with_capacity(512);
    match tls {
        Some(tls) => {
            let mut stream = tls
                .connector
                .connect(tls.server_name.clone(), stream)
                .await?;
            stream.write_all(request).await?;
            stream.flush().await?;
            // No write shutdown here: closing the TLS write half sends
            // close_notify, and the proxy replies `connection: close`, so
            // read_to_end still terminates.
            stream.read_to_end(&mut response).await?;
        }
        None => {
            let mut stream = stream;
            stream.write_all(request).await?;
            stream.shutdown().await?;
            stream.read_to_end(&mut response).await?;
        }
    }
    Ok(response.starts_with(b"HTTP/1.1 200 "))
}

/// Client config that accepts the benchmark's throwaway self-signed certificate.
///
/// The handshake still performs real key exchange and signature verification of
/// the transcript; only the trust-chain and hostname checks are skipped. This is
/// a benchmark client, never the proxy's own upstream path.
fn insecure_client_config() -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCertificate { provider }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct AcceptAnyServerCertificate {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCertificate {
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

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted[index]
}
