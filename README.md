# edgeproxy

`edgeproxy` is a bounded, observable Layer-7 reverse proxy/load balancer written in
Rust and Tokio. It implements a deliberately narrow HTTP/1.1 profile so request
framing, retry safety, and overload behavior remain explicit and testable.

This is an infrastructure engineering project, not a claim of feature parity
with HAProxy, Envoy, or nginx.

## Features

- Strict HTTP/1.1 request parsing with configurable request-line, header-count,
  header-size, body-size, and deadline limits.
- Request-smuggling defenses: exactly one nonempty `Host`, matching duplicate
  `Content-Length` values only, and rejection of `Content-Length` plus
  `Transfer-Encoding`.
- Round-robin and least-connections balancing over actively checked upstreams.
- Per-upstream circuit breakers with a single half-open probe.
- Phase-aware retries: connect failures are safe for any method; failures after
  sending are retried only for idempotent methods; no retry occurs after a
  downstream response starts.
- Bounded request admission, fixed-size response streaming buffers, phase
  timeouts, graceful shutdown, JSON access logs, and Prometheus metrics.
- Deterministic local-upstream integration tests, parser property/fuzz tests,
  and a self-contained release benchmark.

## Quick Start

Requirements: Rust 1.78 or newer.

```powershell
Copy-Item config.example.toml edgeproxy.toml
cargo run --release -- --config edgeproxy.toml
```

The example upstream addresses are placeholders. Point them at HTTP/1.1 servers
before starting. Metrics are available at `GET /metrics` on
`metrics_listen`. Set `RUST_LOG=edgeproxy=debug` for health-check detail.

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Run the reproducible local benchmark:

```powershell
.\scripts\benchmark.ps1 -Requests 5000 -Concurrency 64
```

The runner builds release binaries, starts two hidden local fixture servers and
the proxy, runs the included HTTP load generator, writes
`benchmark/latest.json`, and always terminates its child processes.

## Design Boundaries

The current release accepts origin-form HTTP/1.1 requests with
`Content-Length` or no body. It intentionally rejects chunked request bodies,
`Expect: 100-continue`, upgrades, absolute-form targets, and non-HTTP/1.1
requests. Responses support fixed length, chunked pass-through, or
close-delimited bodies.

Each downstream request uses one upstream TCP connection and closes both sides
after the response. TLS termination, HTTP/2, WebSockets, upstream connection
pooling, dynamic config reload, and distributed circuit state are not
implemented. These are explicit scope limits, not hidden benchmark caveats.

See [docs/protocol-and-failure-model.md](docs/protocol-and-failure-model.md) for
the wire profile, retry matrix, health/circuit behavior, and failure semantics.
Measured results and methodology are in
[benchmark/RESULTS.md](benchmark/RESULTS.md).

## Configuration

Configuration is strict TOML: unknown fields, duplicate upstream names or
addresses, invalid limits, invalid retry codes, and conflicting listener
addresses fail startup. See [config.example.toml](config.example.toml) for every
setting.

`max_in_flight` bounds admitted downstream requests. When exhausted, edgeproxy
returns `503` through a short bounded rejection path instead of allocating an
unbounded task queue.

## Resume Bullets

- Built an async Rust HTTP/1.1 reverse proxy with strict framing limits,
  round-robin/least-connections balancing, bounded admission, and streamed
  response forwarding.
- Implemented active health checks, per-upstream circuit breakers, phase-aware
  safe retries, graceful shutdown, structured access logs, and Prometheus
  metrics.
- Validated protocol and failure behavior with 13 deterministic
  unit/property/integration tests, a libFuzzer target, CI quality gates, and a
  reproducible local load harness with documented saturation results.

## License

MIT

