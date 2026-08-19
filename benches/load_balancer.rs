//! Criterion microbenchmarks for the load-balancer selection path.
//!
//! `LoadBalancer::select` runs on every request, so its cost is a per-request
//! tax. Round-robin is O(1) in the common case; least-connections scans all
//! upstreams, so the two policies are measured across pool sizes to make that
//! difference explicit.
//!
//! The selection guard is dropped immediately inside each iteration, which also
//! exercises the `Drop` impl that decrements the active-connection counter —
//! that pairing is part of the real per-request cost.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use edgeproxy::config::{CircuitBreaker, Policy, UpstreamConfig};
use edgeproxy::load_balancer::LoadBalancer;
use std::hint::black_box;

fn upstreams(count: usize) -> Vec<UpstreamConfig> {
    (0..count)
        .map(|index| UpstreamConfig {
            name: format!("upstream-{index}"),
            // Distinct ports keep config validation's duplicate-address rule happy.
            address: format!("127.0.0.1:{}", 9000 + index)
                .parse()
                .expect("valid address"),
            health_path: "/health".to_owned(),
            tls: false,
            tls_server_name: None,
            tls_insecure_skip_verify: false,
        })
        .collect()
}

/// Selection cost by policy and pool size, with every upstream available.
fn select_healthy(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("load_balancer_select");

    for policy in [Policy::RoundRobin, Policy::LeastConnections] {
        let label = match policy {
            Policy::RoundRobin => "round_robin",
            Policy::LeastConnections => "least_connections",
        };
        for count in [2_usize, 8, 32] {
            let balancer = LoadBalancer::new(upstreams(count), policy);
            group.bench_with_input(
                BenchmarkId::new(label, count),
                &balancer,
                |bencher, balancer| {
                    bencher.iter(|| {
                        // Dropping inside the loop returns the active count to
                        // its baseline, so each iteration measures the same work.
                        let selected = black_box(balancer.select());
                        drop(selected);
                    });
                },
            );
        }
    }

    group.finish();
}

/// Selection cost when most of the pool is unavailable.
///
/// This is the interesting degraded case: the scan must skip open circuits to
/// reach the one upstream still serving traffic.
fn select_with_open_circuits(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("load_balancer_select_degraded");
    let breaker = CircuitBreaker {
        failure_threshold: 1,
        // Long enough that the circuits stay open for the whole measurement.
        open_ms: 600_000,
    };

    for policy in [Policy::RoundRobin, Policy::LeastConnections] {
        let label = match policy {
            Policy::RoundRobin => "round_robin",
            Policy::LeastConnections => "least_connections",
        };
        let count = 32;
        let balancer = LoadBalancer::new(upstreams(count), policy);
        // Trip every circuit except the last, leaving exactly one candidate.
        for upstream in balancer.upstreams().iter().take(count - 1) {
            upstream.record_failure(&breaker);
        }

        group.bench_with_input(
            BenchmarkId::new(label, "31_of_32_open"),
            &balancer,
            |bencher, balancer| {
                bencher.iter(|| {
                    let selected = black_box(balancer.select());
                    drop(selected);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, select_healthy, select_with_open_circuits);
criterion_main!(benches);
