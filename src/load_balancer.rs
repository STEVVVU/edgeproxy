use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{CircuitBreaker, Policy, UpstreamConfig};

#[derive(Debug)]
pub struct Upstream {
    pub config: UpstreamConfig,
    healthy: AtomicBool,
    active_connections: AtomicUsize,
    consecutive_failures: AtomicU32,
    circuit_open_until_ms: AtomicU64,
    half_open_probe: AtomicBool,
    health_successes: AtomicU32,
    health_failures: AtomicU32,
}

impl Upstream {
    fn new(config: UpstreamConfig) -> Self {
        Self {
            config,
            healthy: AtomicBool::new(true),
            active_connections: AtomicUsize::new(0),
            consecutive_failures: AtomicU32::new(0),
            circuit_open_until_ms: AtomicU64::new(0),
            half_open_probe: AtomicBool::new(false),
            health_successes: AtomicU32::new(0),
            health_failures: AtomicU32::new(0),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }

    pub fn circuit_open(&self) -> bool {
        self.circuit_open_until_ms.load(Ordering::Relaxed) > now_ms()
    }

    fn available(&self) -> bool {
        if !self.is_healthy() {
            return false;
        }
        let open_until = self.circuit_open_until_ms.load(Ordering::Acquire);
        if open_until == 0 {
            return true;
        }
        if open_until > now_ms() {
            return false;
        }
        self.half_open_probe
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.circuit_open_until_ms.store(0, Ordering::Release);
        self.half_open_probe.store(false, Ordering::Release);
    }

    pub fn record_failure(&self, config: &CircuitBreaker) -> bool {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        self.half_open_probe.store(false, Ordering::Release);
        if failures >= config.failure_threshold {
            self.circuit_open_until_ms
                .store(now_ms().saturating_add(config.open_ms), Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn record_health_result(
        &self,
        success: bool,
        healthy_threshold: u32,
        unhealthy_threshold: u32,
    ) {
        if success {
            self.health_failures.store(0, Ordering::Relaxed);
            let successes = self.health_successes.fetch_add(1, Ordering::AcqRel) + 1;
            if successes >= healthy_threshold {
                self.healthy.store(true, Ordering::Release);
                self.health_successes
                    .store(healthy_threshold, Ordering::Relaxed);
            }
        } else {
            self.health_successes.store(0, Ordering::Relaxed);
            let failures = self.health_failures.fetch_add(1, Ordering::AcqRel) + 1;
            if failures >= unhealthy_threshold {
                self.healthy.store(false, Ordering::Release);
                self.health_failures
                    .store(unhealthy_threshold, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Debug)]
pub struct LoadBalancer {
    upstreams: Vec<Arc<Upstream>>,
    policy: Policy,
    cursor: AtomicUsize,
}

impl LoadBalancer {
    pub fn new(configs: Vec<UpstreamConfig>, policy: Policy) -> Self {
        Self {
            upstreams: configs
                .into_iter()
                .map(|config| Arc::new(Upstream::new(config)))
                .collect(),
            policy,
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn upstreams(&self) -> &[Arc<Upstream>] {
        &self.upstreams
    }

    pub fn select(&self) -> Option<SelectedUpstream> {
        let length = self.upstreams.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % length;
        let upstream = match self.policy {
            Policy::RoundRobin => (0..length)
                .map(|offset| &self.upstreams[(start + offset) % length])
                .find(|candidate| candidate.available())
                .cloned(),
            Policy::LeastConnections => (0..length)
                .map(|offset| &self.upstreams[(start + offset) % length])
                .filter(|candidate| candidate.available())
                .min_by_key(|candidate| candidate.active_connections())
                .cloned(),
        }?;

        upstream.active_connections.fetch_add(1, Ordering::AcqRel);
        Some(SelectedUpstream { upstream })
    }
}

#[derive(Debug)]
pub struct SelectedUpstream {
    upstream: Arc<Upstream>,
}

impl SelectedUpstream {
    pub fn upstream(&self) -> &Arc<Upstream> {
        &self.upstream
    }
}

impl Drop for SelectedUpstream {
    fn drop(&mut self) {
        self.upstream
            .active_connections
            .fetch_sub(1, Ordering::AcqRel);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configs() -> Vec<UpstreamConfig> {
        vec![
            UpstreamConfig {
                name: "a".to_owned(),
                address: "127.0.0.1:9001".parse().expect("address"),
                health_path: "/health".to_owned(),
            },
            UpstreamConfig {
                name: "b".to_owned(),
                address: "127.0.0.1:9002".parse().expect("address"),
                health_path: "/health".to_owned(),
            },
        ]
    }

    #[test]
    fn round_robin_is_deterministic() {
        let balancer = LoadBalancer::new(configs(), Policy::RoundRobin);
        let first = balancer.select().expect("first");
        assert_eq!(first.upstream().config.name, "a");
        drop(first);
        let second = balancer.select().expect("second");
        assert_eq!(second.upstream().config.name, "b");
    }

    #[test]
    fn circuit_opens_after_threshold() {
        let balancer = LoadBalancer::new(configs(), Policy::RoundRobin);
        let config = CircuitBreaker {
            failure_threshold: 2,
            open_ms: 1000,
        };
        let upstream = &balancer.upstreams()[0];
        assert!(!upstream.record_failure(&config));
        assert!(upstream.record_failure(&config));
        assert!(upstream.circuit_open());
    }
}
