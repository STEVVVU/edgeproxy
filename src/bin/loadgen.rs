use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::timeout;

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
}

#[derive(Serialize)]
struct Report {
    address: SocketAddr,
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
    let request = Arc::new(format!(
        "GET {} HTTP/1.1\r\nhost: benchmark\r\nconnection: close\r\n\r\n",
        args.path
    ));
    let started = Instant::now();
    let mut tasks = JoinSet::new();

    for _ in 0..args.concurrency.min(args.requests) {
        let next = Arc::clone(&next);
        let request = Arc::clone(&request);
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
                match timeout(request_timeout, exchange(address, request.as_bytes())).await {
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
        methodology: "HTTP/1.1 connection-per-request; fixed local upstream response",
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    if failed > 0 {
        anyhow::bail!("{failed} requests failed");
    }
    Ok(())
}

async fn exchange(address: SocketAddr, request: &[u8]) -> Result<bool> {
    let mut stream = TcpStream::connect(address).await?;
    stream.set_nodelay(true)?;
    stream.write_all(request).await?;
    stream.shutdown().await?;
    let mut response = Vec::with_capacity(512);
    stream.read_to_end(&mut response).await?;
    Ok(response.starts_with(b"HTTP/1.1 200 "))
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    sorted[index]
}
