# edgeproxy

`edgeproxy` is a bounded, observable Layer-7 reverse proxy/load balancer written in
Rust and Tokio. It implements a deliberately narrow HTTP/1.1 profile so request
framing, retry safety, and overload behavior remain explicit and testable.

This is an infrastructure engineering project, not a claim of feature parity
with HAProxy, Envoy, or nginx.

## Features

- **TLS termination** via `rustls`, with SNI-based certificate selection and
  optional re-encryption to upstreams.
- Strict HTTP/1.1 request parsing with configurable request-line, header-count,
  header-size, body-size, and deadline limits.
- Request-smuggling defenses: exactly one nonempty `Host`, matching duplicate
  `Content-Length` values only, and rejection of `Content-Length` plus
  `Transfer-Encoding`. Enforced identically on the plaintext and TLS paths,
  which share one parser.
- Round-robin and least-connections balancing over actively checked upstreams.
- Per-upstream circuit breakers with a single half-open probe.
- Phase-aware retries: connect failures are safe for any method; failures after
  sending are retried only for idempotent methods; no retry occurs after a
  downstream response starts.
- Bounded request admission, fixed-size response streaming buffers, phase
  timeouts, graceful shutdown, JSON access logs, and Prometheus metrics via
  `metrics-exporter-prometheus`.
- Deterministic local-upstream integration tests, real-handshake TLS tests,
  parser property/fuzz tests, `criterion` microbenchmarks, and a self-contained
  release benchmark.

## Quick Start

Requirements: Rust 1.86 or newer (see [MSRV](#msrv)).

```powershell
Copy-Item config.example.toml edgeproxy.toml
cargo run --release -- --config edgeproxy.toml
```

The example upstream addresses are placeholders. Point them at HTTP/1.1 servers
before starting. `upstreams[].address` is an `IP:port` literal: edgeproxy does no
DNS resolution, so the request path has no resolver dependency. Metrics are
available at `GET /metrics` on `metrics_listen`. Set `RUST_LOG=edgeproxy=debug`
for health-check and TLS/SNI detail.

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

### Run the demo topology

```powershell
.\scripts\demo.ps1 -Upstreams 3 -Tls
```

Starts N upstream fixtures behind the proxy as local child processes, all
terminated on exit. Repeated `curl http://127.0.0.1:8080/` calls return
`upstream=a`, `upstream=b`, ... in rotation; killing a fixture shows the health
check ejecting it. With `-Tls` the script also generates a throwaway self-signed
certificate and serves `https://127.0.0.1:8443/` (`curl -k`). The generated
config is written to `demo/edgeproxy.demo.toml` for inspection.

Everything runs as a plain local process, so this path depends on nothing but a
Rust toolchain. The [Containers](#containers) section below runs the same
topology under Docker Compose when a container runtime is available.

### Benchmarks

```powershell
.\scripts\benchmark.ps1 -Requests 300 -Concurrency 1 -IncludeTls
```

Builds release binaries, starts two hidden local fixture servers and the proxy,
runs the included load generator against the plaintext listener and — with
`-IncludeTls` — the HTTPS one, writes `benchmark/latest.json` and
`benchmark/latest-tls.json`, and always terminates its child processes.

**Measured TLS overhead: +2.36 ms per request at p50** (plaintext 3.97 ms → TLS
6.27 ms), median of 13 paired runs, positive in all 13. Every request performs a
full handshake because the proxy uses one connection per request, so this is the
worst case for TLS and the best case for isolating its cost: session resumption
and connection reuse would amortize it away.

The low concurrency above is deliberate. Concurrent runs on the author's machine
are dominated by Windows ephemeral-port exhaustion — two ports per request
against a 16,384-port pool — which produced a 7x measurement envelope and even an
implausible "TLS is faster" reading. Only the serial figure is stable enough to
publish. Both the numbers and the reason the concurrent ones are withheld are in
[benchmark/RESULTS.md](benchmark/RESULTS.md), along with the withdrawal of the
pre-TLS figures this file used to quote.

`criterion` covers the microbenchmark side (request parsing and the
load-balancer pick path) where socket and scheduler noise would otherwise
dominate:

```powershell
cargo bench
```

Parsing a typical request head costs 3.80 us (95% CI [3.63–4.01]) and picking an
upstream 64.8 ns [61.5–67.3] — 605x and ~35,000x below the ~2.3 ms handshake cost
above, which is why that TLS figure is measuring cryptography and not proxy
overhead. The published table, with a confidence interval per row, is in
[benchmark/RESULTS.md](benchmark/RESULTS.md#microbenchmarks); it covers all 18 of
the suite's parameterized cases, and each row is backed by criterion's own
estimate committed under [benchmark/criterion/](benchmark/criterion), so the
figures can be checked without re-running the suite.
`scripts/check_documented_metrics.py` fails CI if a published row and its
artifact ever disagree.

## TLS

TLS is optional. Omit the `[tls]` section and edgeproxy runs exactly as before,
plaintext-only. With it present, edgeproxy serves both listeners concurrently;
it does not redirect HTTP to HTTPS.

```toml
[tls]
listen = "0.0.0.0:8443"
certificate = "/etc/edgeproxy/tls/fullchain.pem"
private_key  = "/etc/edgeproxy/tls/privkey.pem"

# Optional per-hostname certificates, selected from the TLS SNI extension.
# The certificate above is served when SNI is absent or unmatched.
[[tls.sni]]
server_name = "api.example.com"
certificate = "/etc/edgeproxy/tls/api-fullchain.pem"
private_key  = "/etc/edgeproxy/tls/api-privkey.pem"
```

`certificate` is a PEM chain (leaf first); `private_key` is PEM PKCS#8, PKCS#1,
or SEC1. Both are read at startup, so a bad path or unparseable PEM fails the
bind rather than the first connection — and rotating a certificate requires a
restart.

**SNI routing here means certificate selection, not upstream selection.** A
ClientHello naming `api.example.com` receives that hostname's certificate;
choosing a *different upstream pool* per hostname is host-based routing and is
not implemented. That is the natural next step and is listed as future work
below.

Protocol versions are TLS 1.2 and 1.3. ALPN advertises `http/1.1` only, so a
client offering nothing but h2 fails the handshake instead of silently
downgrading.

### Upstream TLS (backend re-encryption)

```toml
[[upstreams]]
name = "api-tls"
address = "10.0.0.13:8443"
tls = true
tls_server_name = "api.internal.example.com"
```

`tls_server_name` is sent as SNI, verified against the upstream certificate, and
used as the outgoing `Host` so the origin never sees a `Host` that disagrees with
the TLS identity. Without it the IP literal is verified, which requires an IP
SAN. Trust anchors come from `webpki-roots`, so verification is identical on
every host regardless of OS CA configuration; a privately-issued upstream
certificate will not verify. `tls_insecure_skip_verify` exists for self-signed
lab upstreams, is rejected unless `tls = true`, and logs a warning at startup.

The upstream handshake completes *before* any request bytes are written, so a
handshake failure stays in the "before send" phase and remains safe to retry for
any method. The retry matrix is unchanged. Health probes speak TLS for TLS
upstreams, so a certificate problem surfaces as an unhealthy upstream rather than
per-request 502s.

### Dependency rationale

| Crate | Version | Why |
| --- | --- | --- |
| `rustls` | 0.23 | Pure-Rust TLS; keeps `unsafe_code = "forbid"` intact. |
| `tokio-rustls` | 0.26 | `AsyncRead`/`AsyncWrite` adapter for the existing tokio I/O. |
| `rustls-pemfile` | 2.2 | PEM certificate/key loading. |
| `metrics` | 0.24 | Instrumentation facade. |
| `metrics-exporter-prometheus` | 0.18 | Prometheus exposition, `default-features = false` to avoid pulling in hyper. |
| `criterion` | 0.7 | Statistically sound microbenchmarks. |
| `rcgen` | 0.14 | Dev-only; generates throwaway test certificates. |

rustls is configured with the **`ring`** provider rather than `aws-lc-rs`. Both
are supported; `ring` builds without cmake or NASM, which keeps contributor
setup simple on Windows. Neither requires `unsafe` in this crate.

## Metrics

Metrics moved from hand-rolled atomics and a hand-written renderer to the
`metrics` facade with the Prometheus exporter. **Every metric name and semantic
is preserved**, so existing dashboards and alerts keep working: the same
counters, the same 2xx/4xx/5xx-only status classes (1xx and 3xx remain
deliberately uncounted), and the same 1 ms – 1 s latency bucket ladder, now
expressed in seconds. `# HELP`/`# TYPE` lines are newly emitted; the old
renderer omitted them.

Each proxy owns its own recorder rather than using the crate's process-global
one, preserving the previous per-instance isolation. Label cardinality stays
bounded to configured upstream names.

### Scraping with Prometheus

`metrics_listen` serves the Prometheus text exposition format at `/metrics`, so
any Prometheus instance can scrape it directly. This repo ships a ready config
for the demo topology but does **not** run Prometheus for you:

```powershell
.\scripts\demo.ps1
prometheus --config.file=demo/prometheus.yml --storage.tsdb.retention.time=1h
```

Point `scrape_configs[].static_configs[].targets` at your own `metrics_listen`
address to scrape a real deployment.

## Containers

A multi-stage `Dockerfile` compiles the proxy with a full Rust toolchain and
ships only the stripped binary onto `gcr.io/distroless/cc-debian12:nonroot` — no
shell, no package manager, running as the unprivileged `nonroot` user. Because
rustls uses the pure-Rust `ring` provider, the runtime image needs no OpenSSL.

`compose.yaml` runs the containerized counterpart of `scripts/demo.ps1`:
edgeproxy in front of two upstream fixtures, scraped by Prometheus, with an init
container generating a throwaway self-signed certificate into a volume so no key
material is committed. The proxy and fixtures run read-only with all capabilities
dropped.

```bash
docker compose up --build

curl http://localhost:8080/           # proxied, round robin over both upstreams
curl -k https://localhost:8443/       # same, TLS-terminated (self-signed)
curl http://localhost:9090/metrics    # edgeproxy metrics
```

Upstreams are pinned to static IPs on a user-defined bridge because
`upstreams[].address` is a `SocketAddr`: edgeproxy performs no DNS resolution, so
compose service names cannot be used there.

The image build is exercised by the `container` CI job (`docker buildx build`,
`push: false`), so a green check is the evidence that it compiles and assembles.
It was not built or run on the author's local machine — the CI runner is the only
environment in which the image has been produced.

## Design Boundaries

The current release accepts origin-form HTTP/1.1 requests with
`Content-Length` or no body. It intentionally rejects chunked request bodies,
`Expect: 100-continue`, upgrades, absolute-form targets, and non-HTTP/1.1
requests. Responses support fixed length, chunked pass-through, or
close-delimited bodies.

Each downstream request uses one upstream TCP connection and closes both sides
after the response. HTTP/2, WebSockets, downstream keep-alive, upstream
connection pooling, TLS session resumption, dynamic config reload, and
distributed circuit state are not implemented. These are explicit scope limits,
not hidden benchmark caveats — and the connection-per-request model is why the
measured TLS overhead above is a per-request cost rather than a per-connection
one amortized across many.

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
- Terminated TLS with `rustls` including SNI certificate selection and upstream
  re-encryption, and quantified the cost at +2.36 ms per request at p50 over 13
  paired runs.
- Validated protocol and failure behavior with 36 deterministic
  unit/property/integration tests, a libFuzzer target, CI quality gates, and a
  reproducible local load harness whose methodology documents which figures are
  measurable on the test machine and which are not.

## License

MIT

