use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub metrics_listen: SocketAddr,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    #[serde(default = "default_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
    #[serde(default)]
    pub preserve_host: bool,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub retries: Retries,
    #[serde(default)]
    pub circuit_breaker: CircuitBreaker,
    #[serde(default)]
    pub health_check: HealthCheck,
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    RoundRobin,
    LeastConnections,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    pub address: SocketAddr,
    #[serde(default = "default_health_path")]
    pub health_path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub max_request_line_bytes: usize,
    pub max_body_bytes: usize,
    pub max_response_header_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024,
            max_headers: 64,
            max_request_line_bytes: 8 * 1024,
            max_body_bytes: 1024 * 1024,
            max_response_header_bytes: 32 * 1024,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub request_header_ms: u64,
    pub request_body_ms: u64,
    pub connect_ms: u64,
    pub upstream_write_ms: u64,
    pub upstream_response_header_ms: u64,
    pub upstream_idle_ms: u64,
    pub downstream_write_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request_header_ms: 5_000,
            request_body_ms: 10_000,
            connect_ms: 500,
            upstream_write_ms: 5_000,
            upstream_response_header_ms: 5_000,
            upstream_idle_ms: 15_000,
            downstream_write_ms: 15_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retries {
    pub max_attempts: usize,
    pub retry_statuses: Vec<u16>,
}

impl Default for Retries {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            retry_statuses: vec![502, 503, 504],
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreaker {
    pub failure_threshold: u32,
    pub open_ms: u64,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_ms: 10_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthCheck {
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub unhealthy_threshold: u32,
    pub healthy_threshold: u32,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            interval_ms: 2_000,
            timeout_ms: 500,
            unhealthy_threshold: 2,
            healthy_threshold: 2,
        }
    }
}

const fn default_max_in_flight() -> usize {
    1024
}

const fn default_shutdown_grace_ms() -> u64 {
    15_000
}

fn default_health_path() -> String {
    "/health".to_owned()
}

impl Config {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self =
            toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.listen != self.metrics_listen || self.listen.port() == 0,
            "listen and metrics_listen must differ"
        );
        anyhow::ensure!(self.max_in_flight > 0, "max_in_flight must be positive");
        anyhow::ensure!(
            (1..=5).contains(&self.retries.max_attempts),
            "retries.max_attempts must be between 1 and 5"
        );
        anyhow::ensure!(
            self.circuit_breaker.failure_threshold > 0,
            "circuit_breaker.failure_threshold must be positive"
        );
        anyhow::ensure!(
            self.health_check.unhealthy_threshold > 0 && self.health_check.healthy_threshold > 0,
            "health thresholds must be positive"
        );
        anyhow::ensure!(
            !self.upstreams.is_empty(),
            "at least one upstream is required"
        );

        for (name, value) in [
            ("limits.max_header_bytes", self.limits.max_header_bytes),
            ("limits.max_headers", self.limits.max_headers),
            (
                "limits.max_request_line_bytes",
                self.limits.max_request_line_bytes,
            ),
            ("limits.max_body_bytes", self.limits.max_body_bytes),
            (
                "limits.max_response_header_bytes",
                self.limits.max_response_header_bytes,
            ),
        ] {
            anyhow::ensure!(value > 0, "{name} must be positive");
        }

        anyhow::ensure!(
            self.limits.max_request_line_bytes <= self.limits.max_header_bytes,
            "request line limit cannot exceed header limit"
        );

        for (name, value) in [
            (
                "timeouts.request_header_ms",
                self.timeouts.request_header_ms,
            ),
            ("timeouts.request_body_ms", self.timeouts.request_body_ms),
            ("timeouts.connect_ms", self.timeouts.connect_ms),
            (
                "timeouts.upstream_write_ms",
                self.timeouts.upstream_write_ms,
            ),
            (
                "timeouts.upstream_response_header_ms",
                self.timeouts.upstream_response_header_ms,
            ),
            ("timeouts.upstream_idle_ms", self.timeouts.upstream_idle_ms),
            (
                "timeouts.downstream_write_ms",
                self.timeouts.downstream_write_ms,
            ),
            ("health_check.interval_ms", self.health_check.interval_ms),
            ("health_check.timeout_ms", self.health_check.timeout_ms),
            ("shutdown_grace_ms", self.shutdown_grace_ms),
        ] {
            anyhow::ensure!(value > 0, "{name} must be positive");
        }

        let mut names = HashSet::new();
        let mut addresses = HashSet::new();
        for upstream in &self.upstreams {
            anyhow::ensure!(
                !upstream.name.trim().is_empty(),
                "upstream name cannot be empty"
            );
            anyhow::ensure!(
                names.insert(upstream.name.as_str()),
                "duplicate upstream name: {}",
                upstream.name
            );
            anyhow::ensure!(
                addresses.insert(upstream.address),
                "duplicate upstream address: {}",
                upstream.address
            );
            anyhow::ensure!(
                upstream.health_path.starts_with('/')
                    && !upstream.health_path.contains(char::is_whitespace),
                "health path for {} must be an origin-form path",
                upstream.name
            );
        }

        for status in &self.retries.retry_statuses {
            anyhow::ensure!(
                (500..=599).contains(status),
                "retry status must be a 5xx code: {status}"
            );
        }
        Ok(())
    }

    pub const fn shutdown_grace(&self) -> Duration {
        Duration::from_millis(self.shutdown_grace_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Config {
        Config {
            listen: "127.0.0.1:8080".parse().expect("address"),
            metrics_listen: "127.0.0.1:9090".parse().expect("address"),
            policy: Policy::RoundRobin,
            max_in_flight: 32,
            shutdown_grace_ms: 1000,
            preserve_host: false,
            limits: Limits::default(),
            timeouts: Timeouts::default(),
            retries: Retries::default(),
            circuit_breaker: CircuitBreaker::default(),
            health_check: HealthCheck::default(),
            upstreams: vec![UpstreamConfig {
                name: "one".to_owned(),
                address: "127.0.0.1:9001".parse().expect("address"),
                health_path: "/health".to_owned(),
            }],
        }
    }

    #[test]
    fn rejects_duplicate_upstreams() {
        let mut config = valid_config();
        config.upstreams.push(config.upstreams[0].clone());
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unknown_toml_fields() {
        let raw = r#"
            listen = "127.0.0.1:8080"
            metrics_listen = "127.0.0.1:9090"
            typo = true
            upstreams = [{ name = "one", address = "127.0.0.1:9001" }]
        "#;
        assert!(toml::from_str::<Config>(raw).is_err());
    }
}
