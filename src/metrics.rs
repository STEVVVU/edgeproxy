//! Prometheus metrics built on the `metrics` facade plus
//! `metrics-exporter-prometheus`.
//!
//! This replaces a hand-rolled set of atomics and a hand-written exposition
//! renderer. Every metric name and meaning from that implementation is
//! preserved exactly, so existing dashboards and alerts keep working:
//!
//! | Metric | Type |
//! | --- | --- |
//! | `edgeproxy_requests_total` | counter |
//! | `edgeproxy_responses_{2xx,4xx,5xx}_total` | counter |
//! | `edgeproxy_retries_total` | counter |
//! | `edgeproxy_backpressure_rejections_total` | counter |
//! | `edgeproxy_upstream_failures_total` | counter |
//! | `edgeproxy_circuit_opened_total` | counter |
//! | `edgeproxy_health_checks_total` | counter |
//! | `edgeproxy_health_check_failures_total` | counter |
//! | `edgeproxy_client_bytes_{received,sent}_total` | counter |
//! | `edgeproxy_active_downstream_connections` | gauge |
//! | `edgeproxy_request_duration_seconds` | histogram |
//! | `edgeproxy_upstream_healthy` | gauge, `upstream` label |
//! | `edgeproxy_upstream_circuit_open` | gauge, `upstream` label |
//! | `edgeproxy_upstream_active_connections` | gauge, `upstream` label |
//!
//! ## Why a local recorder instead of the global one
//!
//! The `metrics` crate's ergonomic macros (`counter!`, `gauge!`) route to one
//! process-global recorder. edgeproxy instead holds a [`PrometheusRecorder`]
//! per proxy and calls the [`Recorder`] trait directly. That keeps the previous
//! behavior — each `BoundProxy` owns its own counters — so several proxies can
//! run in one process (as the integration tests do) without their numbers
//! bleeding together. Metric handles are registered once in [`Metrics::new`]
//! and cached, so the hot path is an atomic increment, as before.
//!
//! ## Deliberate differences from the old renderer
//!
//! * `# HELP`/`# TYPE` lines are now emitted. The hand-rolled renderer omitted
//!   them; emitting them is strictly more correct exposition format.
//! * Histogram `_sum`/`_count`/`_bucket` series are produced by the exporter.
//!   Bucket boundaries are the same 1 ms – 1 s ladder, now expressed in seconds
//!   (see [`LATENCY_BUCKETS_SECONDS`]), and the exporter appends the `+Inf`
//!   bucket that the old renderer wrote by hand.
//! * Label escaping is handled by the exporter rather than a local helper.
//!
//! Per-upstream gauges are still *pulled*: [`Metrics::render`] samples the load
//! balancer's live atomics at scrape time rather than tracking them eagerly,
//! matching the previous behavior and keeping label cardinality bounded to the
//! configured upstream names.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Label, Metadata, Recorder, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle, PrometheusRecorder};

use crate::load_balancer::LoadBalancer;

/// Latency histogram buckets in seconds.
///
/// Identical boundaries to the previous millisecond ladder
/// (`1, 5, 10, 25, 50, 100, 500, 1000` ms).
pub const LATENCY_BUCKETS_SECONDS: [f64; 8] =
    [0.001, 0.005, 0.010, 0.025, 0.050, 0.100, 0.500, 1.000];

/// Metadata attached to every metric registration. `metrics` uses this for
/// recorder-side filtering; edgeproxy does not filter, so one constant suffices.
const METADATA: Metadata<'static> = Metadata::new(module_path!(), metrics::Level::INFO, None);

const UPSTREAM_HEALTHY: &str = "edgeproxy_upstream_healthy";
const UPSTREAM_CIRCUIT_OPEN: &str = "edgeproxy_upstream_circuit_open";
const UPSTREAM_ACTIVE_CONNECTIONS: &str = "edgeproxy_upstream_active_connections";

/// Owns this proxy's Prometheus recorder and its cached metric handles.
pub struct Metrics {
    recorder: PrometheusRecorder,
    handle: PrometheusHandle,
    requests: Counter,
    responses_2xx: Counter,
    responses_4xx: Counter,
    responses_5xx: Counter,
    retries: Counter,
    rejected_backpressure: Counter,
    upstream_failures: Counter,
    circuit_opened: Counter,
    health_checks: Counter,
    health_check_failures: Counter,
    bytes_from_clients: Counter,
    bytes_to_clients: Counter,
    active_downstream_gauge: Gauge,
    latency: Histogram,
    /// Authoritative value behind `edgeproxy_active_downstream_connections`.
    ///
    /// The gauge is set from this counter so the exposed value stays exact
    /// under concurrent connection churn, rather than relying on paired
    /// increment/decrement calls.
    active_downstream: AtomicU64,
}

impl std::fmt::Debug for Metrics {
    /// Hand-written because metric handles are opaque and not `Debug`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Metrics")
            .field(
                "active_downstream",
                &self.active_downstream.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Builds a recorder with the edgeproxy bucket ladder and registers every
    /// fixed metric up front.
    pub fn new() -> Self {
        let recorder = PrometheusBuilder::new()
            .set_buckets(&LATENCY_BUCKETS_SECONDS)
            .expect("latency buckets are a nonempty ascending list")
            .build_recorder();
        let handle = recorder.handle();

        describe(&recorder);

        let counter = |name: &'static str| recorder.register_counter(&key(name), &METADATA);
        Self {
            requests: counter("edgeproxy_requests_total"),
            responses_2xx: counter("edgeproxy_responses_2xx_total"),
            responses_4xx: counter("edgeproxy_responses_4xx_total"),
            responses_5xx: counter("edgeproxy_responses_5xx_total"),
            retries: counter("edgeproxy_retries_total"),
            rejected_backpressure: counter("edgeproxy_backpressure_rejections_total"),
            upstream_failures: counter("edgeproxy_upstream_failures_total"),
            circuit_opened: counter("edgeproxy_circuit_opened_total"),
            health_checks: counter("edgeproxy_health_checks_total"),
            health_check_failures: counter("edgeproxy_health_check_failures_total"),
            bytes_from_clients: counter("edgeproxy_client_bytes_received_total"),
            bytes_to_clients: counter("edgeproxy_client_bytes_sent_total"),
            active_downstream_gauge: recorder
                .register_gauge(&key("edgeproxy_active_downstream_connections"), &METADATA),
            latency: recorder
                .register_histogram(&key("edgeproxy_request_duration_seconds"), &METADATA),
            active_downstream: AtomicU64::new(0),
            handle,
            recorder,
        }
    }

    /// Records a started request and returns a guard that decrements the
    /// active-connection gauge on drop.
    pub fn request_started(&self) -> ActiveRequest<'_> {
        self.requests.increment(1);
        let active = self.active_downstream.fetch_add(1, Ordering::Relaxed) + 1;
        self.active_downstream_gauge.set(active as f64);
        ActiveRequest { metrics: self }
    }

    /// Increments the counter for the response's status class.
    ///
    /// 1xx and 3xx are intentionally uncounted, matching the previous
    /// implementation, which tracked only 2xx/4xx/5xx.
    pub fn response(&self, status: u16) {
        match status {
            200..=299 => &self.responses_2xx,
            400..=499 => &self.responses_4xx,
            500..=599 => &self.responses_5xx,
            _ => return,
        }
        .increment(1);
    }

    pub fn retry(&self) {
        self.retries.increment(1);
    }

    pub fn reject_backpressure(&self) {
        self.rejected_backpressure.increment(1);
    }

    pub fn upstream_failure(&self) {
        self.upstream_failures.increment(1);
    }

    pub fn circuit_opened(&self) {
        self.circuit_opened.increment(1);
    }

    pub fn health_check(&self, success: bool) {
        self.health_checks.increment(1);
        if !success {
            self.health_check_failures.increment(1);
        }
    }

    pub fn add_client_bytes(&self, incoming: usize, outgoing: usize) {
        self.bytes_from_clients
            .increment(u64::try_from(incoming).unwrap_or(u64::MAX));
        self.bytes_to_clients
            .increment(u64::try_from(outgoing).unwrap_or(u64::MAX));
    }

    /// Records end-to-end request latency.
    ///
    /// The histogram is in seconds, which is the Prometheus convention and what
    /// the metric name has always claimed.
    pub fn observe_latency(&self, latency: Duration) {
        self.latency.record(latency.as_secs_f64());
    }

    /// Renders the Prometheus exposition text for a `/metrics` scrape.
    ///
    /// Per-upstream gauges are sampled here so they always reflect live
    /// balancer state instead of the last mutation.
    pub fn render(&self, balancer: &LoadBalancer) -> String {
        for upstream in balancer.upstreams() {
            let label = Label::new("upstream", upstream.config.name.clone());
            self.upstream_gauge(UPSTREAM_HEALTHY, label.clone())
                .set(f64::from(u8::from(upstream.is_healthy())));
            self.upstream_gauge(UPSTREAM_CIRCUIT_OPEN, label.clone())
                .set(f64::from(u8::from(upstream.circuit_open())));
            self.upstream_gauge(UPSTREAM_ACTIVE_CONNECTIONS, label)
                .set(upstream.active_connections() as f64);
        }
        self.handle.render()
    }

    fn upstream_gauge(&self, name: &'static str, label: Label) -> Gauge {
        self.recorder
            .register_gauge(&Key::from_parts(name, vec![label]), &METADATA)
    }
}

fn key(name: &'static str) -> Key {
    Key::from_static_name(name)
}

/// Registers `# HELP`/`# TYPE` metadata and units for every metric.
fn describe(recorder: &PrometheusRecorder) {
    let counters = [
        (
            "edgeproxy_requests_total",
            Unit::Count,
            "Downstream requests admitted for processing",
        ),
        (
            "edgeproxy_responses_2xx_total",
            Unit::Count,
            "Responses returned downstream with a 2xx status",
        ),
        (
            "edgeproxy_responses_4xx_total",
            Unit::Count,
            "Responses returned downstream with a 4xx status",
        ),
        (
            "edgeproxy_responses_5xx_total",
            Unit::Count,
            "Responses returned downstream with a 5xx status",
        ),
        (
            "edgeproxy_retries_total",
            Unit::Count,
            "Upstream attempts retried after a safe failure",
        ),
        (
            "edgeproxy_backpressure_rejections_total",
            Unit::Count,
            "Connections rejected with 503 because max_in_flight was exhausted",
        ),
        (
            "edgeproxy_upstream_failures_total",
            Unit::Count,
            "Upstream failures counted against a circuit breaker",
        ),
        (
            "edgeproxy_circuit_opened_total",
            Unit::Count,
            "Circuit-breaker open transitions",
        ),
        (
            "edgeproxy_health_checks_total",
            Unit::Count,
            "Active health probes performed",
        ),
        (
            "edgeproxy_health_check_failures_total",
            Unit::Count,
            "Active health probes that failed",
        ),
        (
            "edgeproxy_client_bytes_received_total",
            Unit::Bytes,
            "Request body bytes read from downstream clients",
        ),
        (
            "edgeproxy_client_bytes_sent_total",
            Unit::Bytes,
            "Response bytes written to downstream clients",
        ),
    ];
    for (name, unit, description) in counters {
        recorder.describe_counter(KeyName::from(name), Some(unit), description.into());
    }

    recorder.describe_gauge(
        KeyName::from("edgeproxy_active_downstream_connections"),
        Some(Unit::Count),
        "Downstream requests currently in flight".into(),
    );
    recorder.describe_histogram(
        KeyName::from("edgeproxy_request_duration_seconds"),
        Some(Unit::Seconds),
        "End-to-end downstream request latency".into(),
    );
    recorder.describe_gauge(
        KeyName::from(UPSTREAM_HEALTHY),
        Some(Unit::Count),
        "1 when the upstream passes active health checks".into(),
    );
    recorder.describe_gauge(
        KeyName::from(UPSTREAM_CIRCUIT_OPEN),
        Some(Unit::Count),
        "1 when the upstream circuit breaker is open".into(),
    );
    recorder.describe_gauge(
        KeyName::from(UPSTREAM_ACTIVE_CONNECTIONS),
        Some(Unit::Count),
        "In-flight upstream connections for this upstream".into(),
    );
}

/// Guard that keeps `edgeproxy_active_downstream_connections` accurate.
pub struct ActiveRequest<'a> {
    metrics: &'a Metrics,
}

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        let active = self
            .metrics
            .active_downstream
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        self.metrics.active_downstream_gauge.set(active as f64);
    }
}

pub type SharedMetrics = Arc<Metrics>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Policy, UpstreamConfig};

    fn balancer() -> LoadBalancer {
        LoadBalancer::new(
            vec![UpstreamConfig {
                name: "alpha".to_owned(),
                address: "127.0.0.1:9001".parse().expect("address"),
                health_path: "/health".to_owned(),
                tls: false,
                tls_server_name: None,
                tls_insecure_skip_verify: false,
            }],
            Policy::RoundRobin,
        )
    }

    #[test]
    fn buckets_match_the_documented_millisecond_ladder() {
        let expected_ms = [1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 500.0, 1000.0];
        for (seconds, millis) in LATENCY_BUCKETS_SECONDS.iter().zip(expected_ms) {
            assert!((seconds * 1000.0 - millis).abs() < f64::EPSILON);
        }
        assert!(LATENCY_BUCKETS_SECONDS
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn renders_every_documented_metric_name() {
        let metrics = Metrics::new();
        let balancer = balancer();
        let _active = metrics.request_started();
        metrics.response(200);
        metrics.response(404);
        metrics.response(503);
        metrics.retry();
        metrics.reject_backpressure();
        metrics.upstream_failure();
        metrics.circuit_opened();
        metrics.health_check(true);
        metrics.health_check(false);
        metrics.add_client_bytes(12, 34);
        metrics.observe_latency(Duration::from_millis(7));

        let rendered = metrics.render(&balancer);
        for name in [
            "edgeproxy_requests_total",
            "edgeproxy_responses_2xx_total",
            "edgeproxy_responses_4xx_total",
            "edgeproxy_responses_5xx_total",
            "edgeproxy_retries_total",
            "edgeproxy_backpressure_rejections_total",
            "edgeproxy_upstream_failures_total",
            "edgeproxy_circuit_opened_total",
            "edgeproxy_health_checks_total",
            "edgeproxy_health_check_failures_total",
            "edgeproxy_client_bytes_received_total",
            "edgeproxy_client_bytes_sent_total",
            "edgeproxy_active_downstream_connections",
            "edgeproxy_request_duration_seconds_bucket",
            "edgeproxy_request_duration_seconds_sum",
            "edgeproxy_request_duration_seconds_count",
        ] {
            assert!(rendered.contains(name), "missing metric {name}");
        }
        for name in [
            UPSTREAM_HEALTHY,
            UPSTREAM_CIRCUIT_OPEN,
            UPSTREAM_ACTIVE_CONNECTIONS,
        ] {
            assert!(
                rendered.contains(&format!("{name}{{upstream=\"alpha\"}}")),
                "missing labeled metric {name}"
            );
        }
    }

    #[test]
    fn status_classes_map_to_the_same_counters_as_before() {
        let metrics = Metrics::new();
        let balancer = balancer();
        metrics.response(201);
        metrics.response(499);
        metrics.response(500);
        // 1xx and 3xx were never counted and still are not.
        metrics.response(100);
        metrics.response(302);
        let rendered = metrics.render(&balancer);
        assert!(rendered.contains("edgeproxy_responses_2xx_total 1"));
        assert!(rendered.contains("edgeproxy_responses_4xx_total 1"));
        assert!(rendered.contains("edgeproxy_responses_5xx_total 1"));
    }

    #[test]
    fn active_request_guard_returns_the_gauge_to_zero() {
        let metrics = Metrics::new();
        {
            let _first = metrics.request_started();
            let _second = metrics.request_started();
            assert_eq!(metrics.active_downstream.load(Ordering::Relaxed), 2);
        }
        assert_eq!(metrics.active_downstream.load(Ordering::Relaxed), 0);
        assert!(metrics
            .render(&balancer())
            .contains("edgeproxy_active_downstream_connections 0"));
    }

    #[test]
    fn each_instance_keeps_its_own_counters() {
        let first = Metrics::new();
        let second = Metrics::new();
        first.retry();
        first.retry();
        second.retry();
        assert!(first
            .render(&balancer())
            .contains("edgeproxy_retries_total 2"));
        assert!(second
            .render(&balancer())
            .contains("edgeproxy_retries_total 1"));
    }

    #[test]
    fn upstream_label_values_are_escaped_by_the_exporter() {
        let balancer = LoadBalancer::new(
            vec![UpstreamConfig {
                name: "needs\"escaping".to_owned(),
                address: "127.0.0.1:9002".parse().expect("address"),
                health_path: "/health".to_owned(),
                tls: false,
                tls_server_name: None,
                tls_insecure_skip_verify: false,
            }],
            Policy::RoundRobin,
        );
        let rendered = Metrics::new().render(&balancer);
        assert!(rendered.contains(r#"upstream="needs\"escaping""#));
    }
}
