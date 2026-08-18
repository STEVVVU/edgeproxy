use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant as StdInstant;

use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use rustls::{ClientConfig, ServerConfig};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at, Duration, Instant};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::http::{
    parse_response_head, read_request, reason_phrase, write_error, Request, ResponseHead,
};
use crate::load_balancer::{LoadBalancer, SelectedUpstream, Upstream};
use crate::metrics::{Metrics, SharedMetrics};
use crate::tls::{self, MaybeTlsStream};

const COPY_BUFFER_SIZE: usize = 16 * 1024;
const OVERLOADED_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    content-type: text/plain\r\ncontent-length: 18\r\nconnection: close\r\n\r\nproxy at capacity\n";

/// Any downstream or upstream transport edgeproxy speaks HTTP/1.1 over.
///
/// Implemented by `TcpStream` and by [`MaybeTlsStream`], which lets the request
/// path stay generic instead of duplicating itself per transport.
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Transport for S {}

struct State {
    config: Arc<Config>,
    balancer: Arc<LoadBalancer>,
    metrics: SharedMetrics,
    /// Present only when at least one upstream sets `tls = true`.
    upstream_tls: Option<UpstreamTls>,
}

/// Client-side TLS configurations for backend re-encryption.
///
/// Two configs because verification policy is per-upstream: the strict one is
/// shared by all verifying upstreams, and `insecure` is built only if some
/// upstream opts out of verification.
struct UpstreamTls {
    verifying: Arc<ClientConfig>,
    insecure: Option<Arc<ClientConfig>>,
}

impl UpstreamTls {
    fn connector_for(&self, upstream: &Upstream) -> TlsConnector {
        let config = if upstream.config.tls_insecure_skip_verify {
            self.insecure.as_ref().unwrap_or(&self.verifying)
        } else {
            &self.verifying
        };
        TlsConnector::from(Arc::clone(config))
    }
}

pub struct BoundProxy {
    listener: TcpListener,
    metrics_listener: TcpListener,
    /// The HTTPS listener and its rustls config, when `[tls]` is configured.
    tls_listener: Option<(TcpListener, Arc<ServerConfig>)>,
    state: Arc<State>,
}

impl BoundProxy {
    pub async fn bind(config: Config) -> Result<Self> {
        config.validate()?;
        let listener = TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("failed to bind proxy listener {}", config.listen))?;
        let metrics_listener = TcpListener::bind(config.metrics_listen)
            .await
            .with_context(|| {
                format!("failed to bind metrics listener {}", config.metrics_listen)
            })?;

        // Certificates are read here so a bad path or unparseable PEM fails
        // startup rather than the first HTTPS connection.
        let tls_listener = match &config.tls {
            Some(settings) => {
                let server_config = tls::server_config(settings)?;
                let bound = TcpListener::bind(settings.listen)
                    .await
                    .with_context(|| format!("failed to bind TLS listener {}", settings.listen))?;
                Some((bound, server_config))
            }
            None => None,
        };

        let upstream_tls = if config.has_tls_upstreams() {
            let insecure_upstreams: Vec<&str> = config
                .upstreams
                .iter()
                .filter(|upstream| upstream.tls && upstream.tls_insecure_skip_verify)
                .map(|upstream| upstream.name.as_str())
                .collect();
            if !insecure_upstreams.is_empty() {
                warn!(
                    event = "upstream_tls_verification_disabled",
                    upstreams = ?insecure_upstreams,
                    "upstream certificate verification is disabled; do not use in production"
                );
            }
            let insecure = if insecure_upstreams.is_empty() {
                None
            } else {
                Some(tls::upstream_client_config(true)?)
            };
            // Validate every configured SNI name up front for the same reason
            // certificates are loaded eagerly.
            for upstream in config.upstreams.iter().filter(|upstream| upstream.tls) {
                tls::upstream_server_name(upstream)?;
            }
            Some(UpstreamTls {
                verifying: tls::upstream_client_config(false)?,
                insecure,
            })
        } else {
            None
        };

        let balancer = Arc::new(LoadBalancer::new(config.upstreams.clone(), config.policy));
        Ok(Self {
            listener,
            metrics_listener,
            tls_listener,
            state: Arc::new(State {
                config: Arc::new(config),
                balancer,
                metrics: Arc::new(Metrics::new()),
                upstream_tls,
            }),
        })
    }

    pub fn listen_addr(&self) -> Result<SocketAddr> {
        self.listener.local_addr().context("proxy local address")
    }

    pub fn metrics_addr(&self) -> Result<SocketAddr> {
        self.metrics_listener
            .local_addr()
            .context("metrics local address")
    }

    /// The bound HTTPS address, or `None` when TLS is not configured.
    ///
    /// Useful for tests that bind port 0.
    pub fn tls_addr(&self) -> Result<Option<SocketAddr>> {
        self.tls_listener
            .as_ref()
            .map(|(listener, _)| listener.local_addr().context("TLS local address"))
            .transpose()
    }

    pub async fn run<F>(self, shutdown_signal: F) -> Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let proxy_addr = self.listen_addr()?;
        let metrics_addr = self.metrics_addr()?;
        let tls_addr = self.tls_addr()?;
        let admission = Arc::new(Semaphore::new(self.state.config.max_in_flight));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let metrics_task = spawn_metrics_server(
            self.metrics_listener,
            Arc::clone(&self.state),
            shutdown_rx.clone(),
        );
        let health_task = spawn_health_checks(Arc::clone(&self.state), shutdown_rx);
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown_signal);

        // `select!` needs a future to poll even when TLS is disabled. Splitting
        // the option lets the TLS arm be disabled with a guard instead.
        let (tls_listener, tls_config) = match self.tls_listener {
            Some((listener, config)) => (Some(listener), Some(config)),
            None => (None, None),
        };

        info!(
            event = "startup",
            listen = %proxy_addr,
            metrics_listen = %metrics_addr,
            tls_listen = tls_addr.map(|address| address.to_string()).as_deref().unwrap_or("-"),
            policy = ?self.state.config.policy,
            upstreams = self.state.config.upstreams.len(),
            max_in_flight = self.state.config.max_in_flight,
            tls_upstreams = self.state.config.has_tls_upstreams(),
            "edgeproxy started"
        );

        loop {
            tokio::select! {
                () = &mut shutdown_signal => {
                    info!(event = "shutdown_started", "stopping admission");
                    break;
                }
                accepted = self.listener.accept() => {
                    let (mut stream, peer) = match accepted {
                        Ok(connection) => connection,
                        Err(error) => {
                            warn!(event = "accept_error", %error);
                            continue;
                        }
                    };
                    let Ok(permit) = Arc::clone(&admission).try_acquire_owned() else {
                        self.state.metrics.reject_backpressure();
                        reject_overloaded(&mut stream).await;
                        continue;
                    };
                    let state = Arc::clone(&self.state);
                    connections.spawn(async move {
                        let _permit = permit;
                        let mut stream = MaybeTlsStream::Plain(stream);
                        if let Err(error) = handle_connection(&mut stream, peer, state).await {
                            debug!(event = "connection_error", client = %peer, %error);
                        }
                    });
                }
                // Only polled when a TLS listener exists.
                accepted = accept_optional(tls_listener.as_ref()), if tls_listener.is_some() => {
                    let (mut stream, peer) = match accepted {
                        Ok(connection) => connection,
                        Err(error) => {
                            warn!(event = "tls_accept_error", %error);
                            continue;
                        }
                    };
                    let Ok(permit) = Arc::clone(&admission).try_acquire_owned() else {
                        self.state.metrics.reject_backpressure();
                        // Rejected before the handshake: a TLS client cannot read
                        // this plaintext response, but the bounded read-and-close
                        // still avoids an unbounded rejection queue.
                        reject_overloaded(&mut stream).await;
                        continue;
                    };
                    let state = Arc::clone(&self.state);
                    let acceptor = TlsAcceptor::from(Arc::clone(
                        tls_config.as_ref().expect("TLS config exists with a TLS listener"),
                    ));
                    connections.spawn(async move {
                        let _permit = permit;
                        // The handshake is bounded by the request-header timeout:
                        // it is the first phase of reading a request, so a stalled
                        // handshake must not hold a permit indefinitely.
                        let handshake_timeout =
                            Duration::from_millis(state.config.timeouts.request_header_ms);
                        let accepted = match timeout(handshake_timeout, acceptor.accept(stream)).await {
                            Ok(Ok(stream)) => stream,
                            Ok(Err(error)) => {
                                debug!(event = "tls_handshake_failed", client = %peer, %error);
                                return;
                            }
                            Err(_) => {
                                debug!(event = "tls_handshake_timeout", client = %peer);
                                return;
                            }
                        };
                        let mut stream = MaybeTlsStream::Tls(Box::new(accepted.into()));
                        if let Err(error) = handle_connection(&mut stream, peer, state).await {
                            debug!(event = "connection_error", client = %peer, %error);
                        }
                    });
                }
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = joined {
                        warn!(event = "connection_task_failed", %error);
                    }
                }
            }
        }

        let _ = shutdown_tx.send(true);
        let grace = self.state.config.shutdown_grace();
        let drain = async {
            while let Some(joined) = connections.join_next().await {
                if let Err(error) = joined {
                    warn!(event = "connection_task_failed", %error);
                }
            }
        };
        if timeout(grace, drain).await.is_err() {
            warn!(
                event = "shutdown_forced",
                grace_ms = grace.as_millis(),
                remaining = connections.len(),
                "aborting connections after grace period"
            );
            connections.abort_all();
        }
        finish_background_task(metrics_task).await;
        finish_background_task(health_task).await;
        info!(event = "shutdown_complete", "edgeproxy stopped");
        Ok(())
    }
}

/// Accepts from an optional listener.
///
/// When the listener is absent this future never completes; the `select!` arm
/// that calls it is guarded by `tls_listener.is_some()`, so it is never polled
/// in that case.
async fn accept_optional(
    listener: Option<&TcpListener>,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

async fn reject_overloaded(stream: &mut TcpStream) {
    let mut discard = [0_u8; 1024];
    let _ = timeout(Duration::from_millis(25), stream.read(&mut discard)).await;
    let _ = timeout(
        Duration::from_millis(100),
        stream.write_all(OVERLOADED_RESPONSE),
    )
    .await;
    let _ = stream.shutdown().await;
}

async fn finish_background_task(mut task: JoinHandle<()>) {
    if timeout(Duration::from_secs(2), &mut task).await.is_err() {
        task.abort();
    }
}

async fn handle_connection<S: Transport>(
    downstream: &mut MaybeTlsStream<S>,
    peer: SocketAddr,
    state: Arc<State>,
) -> Result<()> {
    let started = StdInstant::now();
    let scheme = if downstream.is_tls() { "https" } else { "http" };
    let _active = state.metrics.request_started();
    let request = match read_request(downstream, &state.config.limits, &state.config.timeouts).await
    {
        Ok(request) => request,
        Err(error) => {
            let status = error.status();
            state.metrics.response(status);
            write_error(downstream, status, &error.to_string()).await;
            state.metrics.observe_latency(started.elapsed());
            info!(
                event = "access",
                client = %peer,
                scheme,
                method = "-",
                target = "-",
                status,
                latency_ms = started.elapsed().as_millis(),
                error = %error
            );
            return Ok(());
        }
    };

    state.metrics.add_client_bytes(request.body.len(), 0);
    let method = request.method.clone();
    let target = request.target.clone();
    let outcome = proxy_request(downstream, peer, &request, &state).await;
    let (status, upstream, retries, bytes_sent, failure) = match outcome {
        Ok(outcome) => (
            outcome.status,
            outcome.upstream,
            outcome.retries,
            outcome.bytes_sent,
            None,
        ),
        Err(error) => {
            let status = error.status;
            if !error.response_started {
                write_error(downstream, status, &error.message).await;
            }
            (
                status,
                error.upstream,
                error.retries,
                0,
                Some(error.message),
            )
        }
    };

    state.metrics.response(status);
    state.metrics.add_client_bytes(0, bytes_sent);
    state.metrics.observe_latency(started.elapsed());
    info!(
        event = "access",
        client = %peer,
        scheme,
        method,
        target,
        status,
        upstream = upstream.as_deref().unwrap_or("-"),
        retries,
        bytes_sent,
        latency_ms = started.elapsed().as_millis(),
        error = failure.as_deref().unwrap_or("-")
    );
    Ok(())
}

struct ProxyOutcome {
    status: u16,
    upstream: Option<String>,
    retries: usize,
    bytes_sent: usize,
}

struct ProxyFailure {
    status: u16,
    message: String,
    upstream: Option<String>,
    retries: usize,
    response_started: bool,
}

async fn proxy_request<S: Transport>(
    downstream: &mut MaybeTlsStream<S>,
    peer: SocketAddr,
    request: &Request,
    state: &State,
) -> Result<ProxyOutcome, ProxyFailure> {
    let mut retries = 0;
    let mut last_upstream = None;
    let attempts = state.config.retries.max_attempts;

    for attempt_index in 0..attempts {
        let Some(selected) = state.balancer.select() else {
            return Err(ProxyFailure {
                status: 503,
                message: "no healthy upstream is available".to_owned(),
                upstream: last_upstream,
                retries,
                response_started: false,
            });
        };
        let upstream_name = selected.upstream().config.name.clone();
        last_upstream = Some(upstream_name.clone());

        match perform_attempt(&selected, request, peer, state).await {
            Ok((stream, head, body_prefix)) => {
                let mut response = AttemptResponse {
                    stream,
                    head,
                    body_prefix,
                    selected,
                };
                let retryable_status = request.is_idempotent()
                    && state
                        .config
                        .retries
                        .retry_statuses
                        .contains(&response.head.status)
                    && attempt_index + 1 < attempts;
                if retryable_status {
                    record_upstream_failure(response.selected.upstream(), state);
                    retries += 1;
                    state.metrics.retry();
                    continue;
                }

                if response.head.status >= 500 {
                    record_upstream_failure(response.selected.upstream(), state);
                } else {
                    response.selected.upstream().record_success();
                }
                match forward_response(downstream, &mut response, state).await {
                    Ok(bytes_sent) => {
                        return Ok(ProxyOutcome {
                            status: response.head.status,
                            upstream: Some(upstream_name),
                            retries,
                            bytes_sent,
                        });
                    }
                    Err(error) => {
                        if error.upstream_failure {
                            record_upstream_failure(response.selected.upstream(), state);
                        }
                        return Err(ProxyFailure {
                            status: response.head.status,
                            message: error.message,
                            upstream: Some(upstream_name),
                            retries,
                            response_started: true,
                        });
                    }
                }
            }
            Err(error) => {
                record_upstream_failure(selected.upstream(), state);
                let can_retry = attempt_index + 1 < attempts
                    && match error.phase {
                        FailurePhase::BeforeSend => true,
                        FailurePhase::AfterSend => request.is_idempotent(),
                    };
                if can_retry {
                    retries += 1;
                    state.metrics.retry();
                    continue;
                }
                return Err(ProxyFailure {
                    status: error.status(),
                    message: error.to_string(),
                    upstream: Some(upstream_name),
                    retries,
                    response_started: false,
                });
            }
        }
    }

    Err(ProxyFailure {
        status: 502,
        message: "upstream attempts exhausted".to_owned(),
        upstream: last_upstream,
        retries,
        response_started: false,
    })
}

fn record_upstream_failure(upstream: &Upstream, state: &State) {
    state.metrics.upstream_failure();
    if upstream.record_failure(&state.config.circuit_breaker) {
        state.metrics.circuit_opened();
        warn!(
            event = "circuit_opened",
            upstream = upstream.config.name,
            open_ms = state.config.circuit_breaker.open_ms
        );
    }
}

struct AttemptResponse {
    /// Plaintext or TLS, depending on the upstream's `tls` setting.
    stream: MaybeTlsStream<TcpStream>,
    head: ResponseHead,
    body_prefix: BytesMut,
    selected: SelectedUpstream,
}

#[derive(Clone, Copy, Debug)]
enum FailurePhase {
    BeforeSend,
    AfterSend,
}

#[derive(Debug, Error)]
#[error("{message}")]
struct AttemptError {
    phase: FailurePhase,
    kind: AttemptErrorKind,
    message: String,
}

#[derive(Clone, Copy, Debug)]
enum AttemptErrorKind {
    Connect,
    Timeout,
    Protocol,
    Io,
}

impl AttemptError {
    const fn status(&self) -> u16 {
        match self.kind {
            AttemptErrorKind::Timeout => 504,
            AttemptErrorKind::Connect | AttemptErrorKind::Protocol | AttemptErrorKind::Io => 502,
        }
    }
}

async fn perform_attempt(
    selected: &SelectedUpstream,
    request: &Request,
    peer: SocketAddr,
    state: &State,
) -> Result<(MaybeTlsStream<TcpStream>, ResponseHead, BytesMut), AttemptError> {
    let upstream = selected.upstream();
    let connect_timeout = Duration::from_millis(state.config.timeouts.connect_ms);
    let tcp_stream = timeout(connect_timeout, TcpStream::connect(upstream.config.address))
        .await
        .map_err(|_| AttemptError {
            phase: FailurePhase::BeforeSend,
            kind: AttemptErrorKind::Timeout,
            message: format!("connect timeout to {}", upstream.config.name),
        })?
        .map_err(|error| AttemptError {
            phase: FailurePhase::BeforeSend,
            kind: AttemptErrorKind::Connect,
            message: format!("connect to {} failed: {error}", upstream.config.name),
        })?;
    let _ = tcp_stream.set_nodelay(true);

    // The upstream handshake happens before any request bytes are written, so a
    // handshake failure stays in the `BeforeSend` phase and is safe to retry
    // for any method, idempotent or not.
    let mut stream = if upstream.config.tls {
        let tls = state.upstream_tls.as_ref().ok_or_else(|| AttemptError {
            phase: FailurePhase::BeforeSend,
            kind: AttemptErrorKind::Connect,
            message: format!(
                "upstream {} requires TLS but no client config was built",
                upstream.config.name
            ),
        })?;
        let server_name =
            tls::upstream_server_name(&upstream.config).map_err(|error| AttemptError {
                phase: FailurePhase::BeforeSend,
                kind: AttemptErrorKind::Connect,
                message: format!("invalid TLS name for {}: {error}", upstream.config.name),
            })?;
        let connector = tls.connector_for(upstream);
        let handshake = timeout(connect_timeout, connector.connect(server_name, tcp_stream))
            .await
            .map_err(|_| AttemptError {
                phase: FailurePhase::BeforeSend,
                kind: AttemptErrorKind::Timeout,
                message: format!("TLS handshake timeout to {}", upstream.config.name),
            })?
            .map_err(|error| AttemptError {
                phase: FailurePhase::BeforeSend,
                kind: AttemptErrorKind::Connect,
                message: format!(
                    "TLS handshake with {} failed: {error}",
                    upstream.config.name
                ),
            })?;
        MaybeTlsStream::Tls(Box::new(handshake.into()))
    } else {
        MaybeTlsStream::Plain(tcp_stream)
    };

    // For a TLS upstream the authority must match the name presented in SNI and
    // verified against the certificate, otherwise the origin sees a `Host` that
    // disagrees with the TLS identity. Plaintext upstreams keep the previous
    // address-literal behavior.
    let authority = match (upstream.config.tls, &upstream.config.tls_server_name) {
        (true, Some(server_name)) => {
            format!("{}:{}", server_name, upstream.config.address.port())
        }
        _ => upstream.config.address.to_string(),
    };
    let request_bytes = request.upstream_bytes(
        &authority,
        &peer.ip().to_string(),
        state.config.preserve_host,
    );
    timeout(
        Duration::from_millis(state.config.timeouts.upstream_write_ms),
        stream.write_all(&request_bytes),
    )
    .await
    .map_err(|_| AttemptError {
        phase: FailurePhase::AfterSend,
        kind: AttemptErrorKind::Timeout,
        message: format!("write timeout to {}", upstream.config.name),
    })?
    .map_err(|error| AttemptError {
        phase: FailurePhase::AfterSend,
        kind: AttemptErrorKind::Io,
        message: format!("write to {} failed: {error}", upstream.config.name),
    })?;

    let deadline =
        Instant::now() + Duration::from_millis(state.config.timeouts.upstream_response_header_ms);
    let mut buffer = BytesMut::with_capacity(4096);
    let head = loop {
        if let Some(end) = find_header_end(&buffer) {
            let head_length = end + 4;
            let head = parse_response_head(
                &buffer[..head_length],
                state.config.limits.max_headers,
                &request.method,
            )
            .map_err(|message| AttemptError {
                phase: FailurePhase::AfterSend,
                kind: AttemptErrorKind::Protocol,
                message: format!("{} returned {message}", upstream.config.name),
            })?;
            if head.status < 200 {
                return Err(AttemptError {
                    phase: FailurePhase::AfterSend,
                    kind: AttemptErrorKind::Protocol,
                    message: format!(
                        "{} returned unsupported informational response",
                        upstream.config.name
                    ),
                });
            }
            break head;
        }
        if buffer.len() >= state.config.limits.max_response_header_bytes {
            return Err(AttemptError {
                phase: FailurePhase::AfterSend,
                kind: AttemptErrorKind::Protocol,
                message: format!("{} response headers exceeded limit", upstream.config.name),
            });
        }
        let read = timeout_at(deadline, stream.read_buf(&mut buffer))
            .await
            .map_err(|_| AttemptError {
                phase: FailurePhase::AfterSend,
                kind: AttemptErrorKind::Timeout,
                message: format!("response header timeout from {}", upstream.config.name),
            })?
            .map_err(|error| AttemptError {
                phase: FailurePhase::AfterSend,
                kind: AttemptErrorKind::Io,
                message: format!("read from {} failed: {error}", upstream.config.name),
            })?;
        if read == 0 {
            return Err(AttemptError {
                phase: FailurePhase::AfterSend,
                kind: AttemptErrorKind::Protocol,
                message: format!("{} closed before a response", upstream.config.name),
            });
        }
    };

    buffer.advance(head.consumed);
    Ok((stream, head, buffer))
}

async fn forward_response<S: Transport>(
    downstream: &mut MaybeTlsStream<S>,
    response: &mut AttemptResponse,
    state: &State,
) -> Result<usize, ForwardError> {
    let write_timeout = Duration::from_millis(state.config.timeouts.downstream_write_ms);
    timeout(write_timeout, downstream.write_all(&response.head.bytes))
        .await
        .map_err(|_| ForwardError::downstream("downstream response-header timeout"))?
        .map_err(|error| {
            ForwardError::downstream(format!("downstream response-header write failed: {error}"))
        })?;
    let mut sent = response.head.bytes.len();

    if response.head.body_forbidden {
        let _ = downstream.shutdown().await;
        return Ok(sent);
    }

    let expected_body_bytes = response.head.content_length;
    let prefix_length = expected_body_bytes.map_or(response.body_prefix.len(), |expected| {
        expected.min(response.body_prefix.len())
    });
    if prefix_length > 0 {
        timeout(
            write_timeout,
            downstream.write_all(&response.body_prefix[..prefix_length]),
        )
        .await
        .map_err(|_| ForwardError::downstream("downstream write timeout"))?
        .map_err(|error| ForwardError::downstream(format!("downstream write failed: {error}")))?;
        sent = sent.saturating_add(prefix_length);
    }

    let idle_timeout = Duration::from_millis(state.config.timeouts.upstream_idle_ms);
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut remaining = expected_body_bytes.map(|expected| expected.saturating_sub(prefix_length));
    loop {
        if remaining == Some(0) {
            break;
        }
        let read_limit = remaining.map_or(buffer.len(), |left| left.min(buffer.len()));
        let read = timeout(
            idle_timeout,
            response.stream.read(&mut buffer[..read_limit]),
        )
        .await
        .map_err(|_| ForwardError::upstream("upstream idle timeout while streaming response"))?
        .map_err(|error| {
            ForwardError::upstream(format!("upstream response read failed: {error}"))
        })?;
        if read == 0 {
            if remaining.is_some_and(|left| left > 0) {
                return Err(ForwardError::upstream(
                    "upstream closed before Content-Length bytes arrived",
                ));
            }
            break;
        }
        timeout(write_timeout, downstream.write_all(&buffer[..read]))
            .await
            .map_err(|_| ForwardError::downstream("downstream write timeout"))?
            .map_err(|error| {
                ForwardError::downstream(format!("downstream write failed: {error}"))
            })?;
        sent = sent.saturating_add(read);
        if let Some(left) = &mut remaining {
            *left -= read;
        }
    }
    let _ = downstream.shutdown().await;
    Ok(sent)
}

struct ForwardError {
    message: String,
    upstream_failure: bool,
}

impl ForwardError {
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            upstream_failure: true,
        }
    }

    fn downstream(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            upstream_failure: false,
        }
    }
}

fn spawn_health_checks(state: Arc<State>, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let period = Duration::from_millis(state.config.health_check.interval_ms);
        let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let mut checks = JoinSet::new();
                    for upstream in state.balancer.upstreams() {
                        let upstream = Arc::clone(upstream);
                        let state = Arc::clone(&state);
                        checks.spawn(async move {
                            let success = health_probe(&upstream, &state).await;
                            state.metrics.health_check(success);
                            upstream.record_health_result(
                                success,
                                state.config.health_check.healthy_threshold,
                                state.config.health_check.unhealthy_threshold,
                            );
                            if success {
                                upstream.record_success();
                            }
                            debug!(
                                event = "health_check",
                                upstream = upstream.config.name,
                                success
                            );
                        });
                    }
                    while checks.join_next().await.is_some() {}
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

/// Probes one upstream's health endpoint.
///
/// TLS upstreams are probed over TLS with the same client config the request
/// path uses, so a certificate problem shows up as an unhealthy upstream rather
/// than as per-request 502s.
async fn health_probe(upstream: &Upstream, state: &State) -> bool {
    let config = &state.config;
    let whole_timeout = Duration::from_millis(config.health_check.timeout_ms);
    timeout(whole_timeout, async {
        let tcp_stream = TcpStream::connect(upstream.config.address).await?;
        let (mut stream, authority) = if upstream.config.tls {
            let Some(tls) = state.upstream_tls.as_ref() else {
                return Ok::<bool, std::io::Error>(false);
            };
            let Ok(server_name) = tls::upstream_server_name(&upstream.config) else {
                return Ok(false);
            };
            let authority = match &upstream.config.tls_server_name {
                Some(name) => format!("{}:{}", name, upstream.config.address.port()),
                None => upstream.config.address.to_string(),
            };
            let handshake = tls
                .connector_for(upstream)
                .connect(server_name, tcp_stream)
                .await?;
            (
                MaybeTlsStream::Tls(Box::new(handshake.into())),
                authority,
            )
        } else {
            let authority = upstream.config.address.to_string();
            (MaybeTlsStream::Plain(tcp_stream), authority)
        };
        let request = format!(
            "GET {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n",
            upstream.config.health_path, authority
        );
        stream.write_all(request.as_bytes()).await?;
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok::<bool, std::io::Error>(false);
        }
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut response = httparse::Response::new(&mut headers);
        let parsed = response.parse(&buffer[..read]);
        Ok(matches!(
            parsed,
            Ok(httparse::Status::Complete(_)) if response.code.is_some_and(|code| (200..400).contains(&code))
        ))
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

fn spawn_metrics_server(
    listener: TcpListener,
    state: Arc<State>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut stream, _peer)) = accepted else {
                        continue;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let mut request = [0_u8; 1024];
                        let read = timeout(Duration::from_secs(2), stream.read(&mut request)).await;
                        let is_metrics = matches!(read, Ok(Ok(size)) if request[..size].starts_with(b"GET /metrics "));
                        let (status, body) = if is_metrics {
                            (200, state.metrics.render(&state.balancer))
                        } else {
                            (404, "not found\n".to_owned())
                        };
                        let response = format!(
                            "HTTP/1.1 {} {}\r\ncontent-type: text/plain; version=0.0.4\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{}",
                            status,
                            reason_phrase(status),
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}
