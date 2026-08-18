use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub metrics_listen: SocketAddr,
    /// Optional HTTPS listener. When present, edgeproxy terminates TLS on
    /// `tls.listen` in addition to serving plaintext HTTP on `listen`.
    #[serde(default)]
    pub tls: Option<TlsListener>,
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

/// HTTPS listener settings.
///
/// `certificate`/`private_key` are the default identity, served when a client
/// sends no SNI or an unmatched one. Each `[[tls.sni]]` entry adds a
/// hostname-specific certificate.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsListener {
    pub listen: SocketAddr,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    #[serde(default)]
    pub sni: Vec<SniCertificate>,
}

/// A certificate selected by TLS SNI hostname.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SniCertificate {
    pub server_name: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    pub address: SocketAddr,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    /// Re-encrypt to this upstream over TLS (backend TLS).
    #[serde(default)]
    pub tls: bool,
    /// SNI/hostname presented to the upstream and verified against its
    /// certificate. Defaults to the upstream IP literal, which requires an IP
    /// SAN, so a hostname is usually required in practice.
    #[serde(default)]
    pub tls_server_name: Option<String>,
    /// Skip upstream certificate-chain and hostname verification.
    ///
    /// For self-signed lab upstreams only. Logged as a warning at startup.
    #[serde(default)]
    pub tls_insecure_skip_verify: bool,
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
        self.validate_tls()?;
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
            if let Some(server_name) = &upstream.tls_server_name {
                anyhow::ensure!(
                    upstream.tls,
                    "upstream {} sets tls_server_name but tls is false",
                    upstream.name
                );
                anyhow::ensure!(
                    !server_name.trim().is_empty()
                        && !server_name.contains(char::is_whitespace)
                        && !server_name.contains('/'),
                    "tls_server_name for {} must be a bare hostname",
                    upstream.name
                );
            }
            anyhow::ensure!(
                upstream.tls || !upstream.tls_insecure_skip_verify,
                "upstream {} sets tls_insecure_skip_verify but tls is false",
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

    /// Whether any upstream is configured for backend re-encryption.
    pub fn has_tls_upstreams(&self) -> bool {
        self.upstreams.iter().any(|upstream| upstream.tls)
    }

    /// Validates the HTTPS listener: distinct addresses and well-formed,
    /// non-duplicated SNI hostnames. Certificate *contents* are validated when
    /// they are loaded in `tls::server_config`, which is the only place that can
    /// report a parse error usefully.
    fn validate_tls(&self) -> Result<()> {
        let Some(tls) = &self.tls else {
            return Ok(());
        };
        // Port 0 means "any free port" and is only used by tests, where a
        // collision is impossible; compare only for concrete ports.
        anyhow::ensure!(
            tls.listen != self.listen || tls.listen.port() == 0,
            "tls.listen and listen must differ"
        );
        anyhow::ensure!(
            tls.listen != self.metrics_listen || tls.listen.port() == 0,
            "tls.listen and metrics_listen must differ"
        );

        let mut server_names = HashSet::new();
        for entry in &tls.sni {
            anyhow::ensure!(
                !entry.server_name.trim().is_empty()
                    && !entry.server_name.contains(char::is_whitespace),
                "tls.sni server_name cannot be empty or contain whitespace"
            );
            anyhow::ensure!(
                entry.server_name == entry.server_name.to_ascii_lowercase(),
                "tls.sni server_name must be lowercase: {}",
                entry.server_name
            );
            anyhow::ensure!(
                server_names.insert(entry.server_name.as_str()),
                "duplicate tls.sni server_name: {}",
                entry.server_name
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Config {
        Config {
            listen: "127.0.0.1:8080".parse().expect("address"),
            metrics_listen: "127.0.0.1:9090".parse().expect("address"),
            tls: None,
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
                tls: false,
                tls_server_name: None,
                tls_insecure_skip_verify: false,
            }],
        }
    }

    fn tls_listener(listen: &str) -> TlsListener {
        TlsListener {
            listen: listen.parse().expect("address"),
            certificate: PathBuf::from("tests/fixtures/test-only-default.crt"),
            private_key: PathBuf::from("tests/fixtures/test-only-default.key"),
            sni: Vec::new(),
        }
    }

    #[test]
    fn rejects_duplicate_upstreams() {
        let mut config = valid_config();
        config.upstreams.push(config.upstreams[0].clone());
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_a_distinct_tls_listener() {
        let mut config = valid_config();
        config.tls = Some(tls_listener("127.0.0.1:8443"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_a_tls_listener_that_collides_with_the_plaintext_listener() {
        let mut config = valid_config();
        config.tls = Some(tls_listener("127.0.0.1:8080"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_sni_server_names() {
        let mut config = valid_config();
        let mut listener = tls_listener("127.0.0.1:8443");
        let entry = SniCertificate {
            server_name: "a.test".to_owned(),
            certificate: PathBuf::from("tests/fixtures/test-only-a.crt"),
            private_key: PathBuf::from("tests/fixtures/test-only-a.key"),
        };
        listener.sni = vec![entry.clone(), entry];
        config.tls = Some(listener);
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_uppercase_sni_server_names() {
        let mut config = valid_config();
        let mut listener = tls_listener("127.0.0.1:8443");
        listener.sni = vec![SniCertificate {
            server_name: "A.test".to_owned(),
            certificate: PathBuf::from("tests/fixtures/test-only-a.crt"),
            private_key: PathBuf::from("tests/fixtures/test-only-a.key"),
        }];
        config.tls = Some(listener);
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_upstream_tls_options_without_tls_enabled() {
        let mut config = valid_config();
        config.upstreams[0].tls_server_name = Some("api.internal".to_owned());
        assert!(config.validate().is_err());

        let mut config = valid_config();
        config.upstreams[0].tls_insecure_skip_verify = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_a_tls_upstream() {
        let mut config = valid_config();
        config.upstreams[0].tls = true;
        config.upstreams[0].tls_server_name = Some("api.internal".to_owned());
        assert!(config.validate().is_ok());
        assert!(config.has_tls_upstreams());
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
