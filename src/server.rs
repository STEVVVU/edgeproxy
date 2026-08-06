use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant as StdInstant;

use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at, Duration, Instant};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::http::{
    parse_response_head, read_request, reason_phrase, write_error, Request, ResponseHead,
};
use crate::load_balancer::{LoadBalancer, SelectedUpstream, Upstream};
use crate::metrics::{Metrics, SharedMetrics};

const COPY_BUFFER_SIZE: usize = 16 * 1024;
const OVERLOADED_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    content-type: text/plain\r\ncontent-length: 18\r\nconnection: close\r\n\r\nproxy at capacity\n";

struct State {
    config: Arc<Config>,
    balancer: Arc<LoadBalancer>,
    metrics: SharedMetrics,
}

pub struct BoundProxy {
    listener: TcpListener,
    metrics_listener: TcpListener,
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
        let balancer = Arc::new(LoadBalancer::new(config.upstreams.clone(), config.policy));
        Ok(Self {
            listener,
            metrics_listener,
            state: Arc::new(State {
                config: Arc::new(config),
                balancer,
                metrics: Arc::new(Metrics::default()),
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

    pub async fn run<F>(self, shutdown_signal: F) -> Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let proxy_addr = self.listen_addr()?;
        let metrics_addr = self.metrics_addr()?;
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

        info!(
            event = "startup",
            listen = %proxy_addr,
            metrics_listen = %metrics_addr,
            policy = ?self.state.config.policy,
            upstreams = self.state.config.upstreams.len(),
            max_in_flight = self.state.config.max_in_flight,
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

async fn handle_connection(
    downstream: &mut TcpStream,
    peer: SocketAddr,
    state: Arc<State>,
) -> Result<()> {
    let started = StdInstant::now();
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

async fn proxy_request(
    downstream: &mut TcpStream,
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
    stream: TcpStream,
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
) -> Result<(TcpStream, ResponseHead, BytesMut), AttemptError> {
    let upstream = selected.upstream();
    let connect_timeout = Duration::from_millis(state.config.timeouts.connect_ms);
    let mut stream = timeout(connect_timeout, TcpStream::connect(upstream.config.address))
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
    let _ = stream.set_nodelay(true);

    let request_bytes = request.upstream_bytes(
        &upstream.config.address.to_string(),
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

async fn forward_response(
    downstream: &mut TcpStream,
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
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
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
                            let success = health_probe(&upstream, &state.config).await;
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

async fn health_probe(upstream: &Upstream, config: &Config) -> bool {
    let whole_timeout = Duration::from_millis(config.health_check.timeout_ms);
    timeout(whole_timeout, async {
        let mut stream = TcpStream::connect(upstream.config.address).await?;
        let request = format!(
            "GET {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n",
            upstream.config.health_path, upstream.config.address
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
