# Benchmark Results

## Reproduction

```powershell
.\scripts\benchmark.ps1 -Requests 5000 -Concurrency 64
```

The script builds with `cargo build --release --bins --examples`, starts two
local deterministic upstreams, starts edgeproxy with round-robin balancing, and
runs the included Tokio load generator. Every request opens one downstream TCP
connection; edgeproxy opens one upstream TCP connection. Responses contain 128
bytes. TLS, network latency, and external services are absent.

Environment:

- Date: 2026-07-16
- OS: Microsoft Windows NT 10.0.26200.0
- CPU: Intel64 Family 6 Model 181, 14 logical processors exposed
- Rust: `rustc 1.96.0 (ac68faa20 2026-05-25)`
- Build: release, thin LTO, one codegen unit

## Successful Baseline

| Metric | Result |
| --- | ---: |
| Requests | 5,000 |
| Concurrency | 64 |
| Successes / failures | 5,000 / 0 |
| Throughput | 331.28 requests/s |
| Average latency | 190.37 ms |
| p50 / p95 / p99 | 155.30 / 380.50 / 602.56 ms |
| Elapsed | 15.09 s |

## Saturation Observation

A 20,000-request run at concurrency 128 produced 15,564 successes and 4,436
failures over 84.67 seconds (183.82 successful requests/s, p95 984.77 ms).
The harness creates two new TCP connections per request, so this result includes
Windows socket/ephemeral-port pressure and host contention. It is recorded as a
failure boundary, not advertised as successful capacity.

These numbers are a reproducible measurement on one development machine, not a
cross-platform performance claim. Upstream connection pooling and downstream
keep-alive are the primary next steps before production-scale benchmarking.

