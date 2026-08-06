use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::load_balancer::LoadBalancer;

const LATENCY_BUCKETS_MS: [u64; 8] = [1, 5, 10, 25, 50, 100, 500, 1000];

#[derive(Debug)]
pub struct Metrics {
    requests: AtomicU64,
    responses_2xx: AtomicU64,
    responses_4xx: AtomicU64,
    responses_5xx: AtomicU64,
    retries: AtomicU64,
    rejected_backpressure: AtomicU64,
    upstream_failures: AtomicU64,
    circuit_opened: AtomicU64,
    health_checks: AtomicU64,
    health_check_failures: AtomicU64,
    bytes_from_clients: AtomicU64,
    bytes_to_clients: AtomicU64,
    active_downstream: AtomicU64,
    latency_buckets: [AtomicU64; LATENCY_BUCKETS_MS.len()],
    latency_count: AtomicU64,
    latency_sum_micros: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            responses_2xx: AtomicU64::new(0),
            responses_4xx: AtomicU64::new(0),
            responses_5xx: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            rejected_backpressure: AtomicU64::new(0),
            upstream_failures: AtomicU64::new(0),
            circuit_opened: AtomicU64::new(0),
            health_checks: AtomicU64::new(0),
            health_check_failures: AtomicU64::new(0),
            bytes_from_clients: AtomicU64::new(0),
            bytes_to_clients: AtomicU64::new(0),
            active_downstream: AtomicU64::new(0),
            latency_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            latency_count: AtomicU64::new(0),
            latency_sum_micros: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    pub fn request_started(&self) -> ActiveRequest<'_> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.active_downstream.fetch_add(1, Ordering::Relaxed);
        ActiveRequest { metrics: self }
    }

    pub fn response(&self, status: u16) {
        match status {
            200..=299 => &self.responses_2xx,
            400..=499 => &self.responses_4xx,
            500..=599 => &self.responses_5xx,
            _ => return,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub fn retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reject_backpressure(&self) {
        self.rejected_backpressure.fetch_add(1, Ordering::Relaxed);
    }

    pub fn upstream_failure(&self) {
        self.upstream_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn circuit_opened(&self) {
        self.circuit_opened.fetch_add(1, Ordering::Relaxed);
    }

    pub fn health_check(&self, success: bool) {
        self.health_checks.fetch_add(1, Ordering::Relaxed);
        if !success {
            self.health_check_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_client_bytes(&self, incoming: usize, outgoing: usize) {
        self.bytes_from_clients.fetch_add(
            u64::try_from(incoming).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.bytes_to_clients.fetch_add(
            u64::try_from(outgoing).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub fn observe_latency(&self, latency: Duration) {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.latency_count.fetch_add(1, Ordering::Relaxed);
        self.latency_sum_micros.fetch_add(micros, Ordering::Relaxed);
        let millis = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        for (index, bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if millis <= *bound {
                self.latency_buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn render(&self, balancer: &LoadBalancer) -> String {
        let mut output = String::with_capacity(4096);
        metric(
            &mut output,
            "edgeproxy_requests_total",
            self.requests.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_responses_2xx_total",
            self.responses_2xx.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_responses_4xx_total",
            self.responses_4xx.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_responses_5xx_total",
            self.responses_5xx.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_retries_total",
            self.retries.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_backpressure_rejections_total",
            self.rejected_backpressure.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_upstream_failures_total",
            self.upstream_failures.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_circuit_opened_total",
            self.circuit_opened.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_health_checks_total",
            self.health_checks.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_health_check_failures_total",
            self.health_check_failures.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_client_bytes_received_total",
            self.bytes_from_clients.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_client_bytes_sent_total",
            self.bytes_to_clients.load(Ordering::Relaxed),
        );
        metric(
            &mut output,
            "edgeproxy_active_downstream_connections",
            self.active_downstream.load(Ordering::Relaxed),
        );

        for (index, bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            let _ = writeln!(
                output,
                "edgeproxy_request_duration_seconds_bucket{{le=\"{}\"}} {}",
                (*bound as f64) / 1000.0,
                self.latency_buckets[index].load(Ordering::Relaxed)
            );
        }
        let count = self.latency_count.load(Ordering::Relaxed);
        let _ = writeln!(
            output,
            "edgeproxy_request_duration_seconds_bucket{{le=\"+Inf\"}} {count}"
        );
        let _ = writeln!(output, "edgeproxy_request_duration_seconds_count {count}");
        let _ = writeln!(
            output,
            "edgeproxy_request_duration_seconds_sum {}",
            self.latency_sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
        );

        for upstream in balancer.upstreams() {
            let name = escape_label(&upstream.config.name);
            let _ = writeln!(
                output,
                "edgeproxy_upstream_healthy{{upstream=\"{name}\"}} {}",
                u8::from(upstream.is_healthy())
            );
            let _ = writeln!(
                output,
                "edgeproxy_upstream_circuit_open{{upstream=\"{name}\"}} {}",
                u8::from(upstream.circuit_open())
            );
            let _ = writeln!(
                output,
                "edgeproxy_upstream_active_connections{{upstream=\"{name}\"}} {}",
                upstream.active_connections()
            );
        }
        output
    }
}

pub struct ActiveRequest<'a> {
    metrics: &'a Metrics,
}

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.metrics
            .active_downstream
            .fetch_sub(1, Ordering::Relaxed);
    }
}

pub type SharedMetrics = Arc<Metrics>;

fn metric(output: &mut String, name: &str, value: u64) {
    let _ = writeln!(output, "{name} {value}");
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('"', r#"\""#)
        .replace('\n', r"\n")
}
