use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use edgeproxy::config::{
    CircuitBreaker, HealthCheck, Limits, Policy, Retries, Timeouts, UpstreamConfig,
};
use edgeproxy::{BoundProxy, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

#[derive(Clone)]
enum Behavior {
    Respond { body: &'static str, delay: Duration },
    Drop,
}

struct Fixture {
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(behavior: Behavior) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let address = listener.local_addr().expect("fixture address");
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let behavior = behavior.clone();
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let _ = timeout(Duration::from_secs(1), stream.read(&mut request)).await;
                match behavior {
                    Behavior::Drop => {}
                    Behavior::Respond { body, delay } => {
                        sleep(delay).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\
                             content-type: text/plain\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                }
            });
        }
    });
    Fixture {
        address,
        connections,
        task,
    }
}

struct TestProxy {
    address: SocketAddr,
    metrics_address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestProxy {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        timeout(Duration::from_secs(2), self.task)
            .await
            .expect("proxy shutdown timeout")
            .expect("proxy task panicked")
            .expect("proxy shutdown failed");
    }
}

fn test_config(upstreams: Vec<SocketAddr>, policy: Policy) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().expect("address"),
        metrics_listen: "127.0.0.1:0".parse().expect("address"),
        policy,
        max_in_flight: 32,
        shutdown_grace_ms: 1000,
        preserve_host: false,
        limits: Limits {
            max_header_bytes: 1024,
            max_headers: 32,
            max_request_line_bytes: 512,
            max_body_bytes: 4096,
            max_response_header_bytes: 4096,
        },
        timeouts: Timeouts {
            request_header_ms: 1000,
            request_body_ms: 1000,
            connect_ms: 250,
            upstream_write_ms: 500,
            upstream_response_header_ms: 500,
            upstream_idle_ms: 500,
            downstream_write_ms: 500,
        },
        retries: Retries {
            max_attempts: 2,
            retry_statuses: vec![502, 503, 504],
        },
        circuit_breaker: CircuitBreaker {
            failure_threshold: 10,
            open_ms: 1000,
        },
        health_check: HealthCheck {
            interval_ms: 60_000,
            timeout_ms: 100,
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
            })
            .collect(),
    }
}

async fn spawn_proxy(config: Config) -> TestProxy {
    let proxy = BoundProxy::bind(config).await.expect("proxy bind");
    let address = proxy.listen_addr().expect("proxy address");
    let metrics_address = proxy.metrics_addr().expect("metrics address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(proxy.run(async move {
        let _ = shutdown_rx.await;
    }));
    TestProxy {
        address,
        metrics_address,
        shutdown: Some(shutdown_tx),
        task,
    }
}

async fn exchange(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    stream.write_all(request).await.expect("request write");
    stream.shutdown().await.expect("request shutdown");
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .expect("response timeout")
        .expect("response read");
    response
}

fn get_request(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nhost: test\r\n\r\n").into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_robin_is_visible_on_the_wire() {
    let one = fixture(Behavior::Respond {
        body: "one",
        delay: Duration::ZERO,
    })
    .await;
    let two = fixture(Behavior::Respond {
        body: "two",
        delay: Duration::ZERO,
    })
    .await;
    let proxy = spawn_proxy(test_config(
        vec![one.address, two.address],
        Policy::RoundRobin,
    ))
    .await;

    let first = exchange(proxy.address, &get_request("/")).await;
    let second = exchange(proxy.address, &get_request("/")).await;
    let third = exchange(proxy.address, &get_request("/")).await;
    assert!(first.ends_with(b"one"));
    assert!(second.ends_with(b"two"));
    assert!(third.ends_with(b"one"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn least_connections_avoids_a_busy_upstream() {
    let slow = fixture(Behavior::Respond {
        body: "slow",
        delay: Duration::from_millis(150),
    })
    .await;
    let fast = fixture(Behavior::Respond {
        body: "fast",
        delay: Duration::ZERO,
    })
    .await;
    let proxy = spawn_proxy(test_config(
        vec![slow.address, fast.address],
        Policy::LeastConnections,
    ))
    .await;

    let address = proxy.address;
    let first = tokio::spawn(async move { exchange(address, &get_request("/slow")).await });
    sleep(Duration::from_millis(25)).await;
    let second = exchange(proxy.address, &get_request("/fast")).await;
    assert!(second.ends_with(b"fast"));
    assert!(first.await.expect("first request").ends_with(b"slow"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_get_but_not_post_after_sending() {
    let broken = fixture(Behavior::Drop).await;
    let healthy = fixture(Behavior::Respond {
        body: "recovered",
        delay: Duration::ZERO,
    })
    .await;
    let proxy = spawn_proxy(test_config(
        vec![broken.address, healthy.address],
        Policy::RoundRobin,
    ))
    .await;

    let get_response = exchange(proxy.address, &get_request("/retry")).await;
    assert!(get_response.starts_with(b"HTTP/1.1 200"));
    assert!(get_response.ends_with(b"recovered"));

    // Round robin returns to the broken upstream. POST may have been observed there,
    // so edgeproxy must not replay it to the healthy server.
    let post = b"POST /write HTTP/1.1\r\nhost: test\r\ncontent-length: 4\r\n\r\ndata";
    let post_response = exchange(proxy.address, post).await;
    assert!(post_response.starts_with(b"HTTP/1.1 502"));
    assert_eq!(healthy.connections.load(Ordering::SeqCst), 1);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_limits_and_metrics_are_exposed() {
    let upstream = fixture(Behavior::Respond {
        body: "ok",
        delay: Duration::ZERO,
    })
    .await;
    let mut config = test_config(vec![upstream.address], Policy::RoundRobin);
    config.limits.max_header_bytes = 128;
    config.limits.max_request_line_bytes = 64;
    let proxy = spawn_proxy(config).await;

    let oversized = format!(
        "GET / HTTP/1.1\r\nhost: test\r\nx-large: {}\r\n\r\n",
        "x".repeat(256)
    );
    let response = exchange(proxy.address, oversized.as_bytes()).await;
    assert!(response.starts_with(b"HTTP/1.1 431"));

    let metrics = exchange(
        proxy.metrics_address,
        b"GET /metrics HTTP/1.1\r\nhost: metrics\r\n\r\n",
    )
    .await;
    let metrics = String::from_utf8(metrics).expect("metrics UTF-8");
    assert!(metrics.contains("edgeproxy_requests_total 1"));
    assert!(metrics.contains("edgeproxy_responses_4xx_total 1"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overload_is_rejected_and_inflight_work_drains() {
    let upstream = fixture(Behavior::Respond {
        body: "finished",
        delay: Duration::from_millis(150),
    })
    .await;
    let mut config = test_config(vec![upstream.address], Policy::RoundRobin);
    config.max_in_flight = 1;
    let mut proxy = spawn_proxy(config).await;

    let address = proxy.address;
    let first = tokio::spawn(async move { exchange(address, &get_request("/slow")).await });
    sleep(Duration::from_millis(25)).await;
    let overloaded = exchange(proxy.address, &get_request("/overload")).await;
    assert!(overloaded.starts_with(b"HTTP/1.1 503"));

    proxy
        .shutdown
        .take()
        .expect("shutdown sender")
        .send(())
        .expect("shutdown send");
    assert!(first.await.expect("first request").ends_with(b"finished"));
    timeout(Duration::from_secs(2), proxy.task)
        .await
        .expect("graceful shutdown timeout")
        .expect("proxy task")
        .expect("proxy result");
}
