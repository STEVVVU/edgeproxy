# Benchmark Results

Measured 2026-07-28 on one development machine. Every figure below was produced
by the harness in this repository on that machine; nothing is quoted from
elsewhere. See [Measurement caveats](#measurement-caveats) — the headline result
is a latency *delta*, not a capacity claim.

## Reproduction

```powershell
.\scripts\benchmark.ps1 -Requests 300 -Concurrency 1 -IncludeTls
```

The script builds with `cargo build --release --bins --examples`, starts two
local deterministic upstreams, starts edgeproxy with round-robin balancing, and
runs the included Tokio load generator against the plaintext listener and then
the HTTPS one. Both legs use the same configuration apart from the `[tls]`
section, so the two measurements differ only in the transport.

Every request opens one downstream TCP connection; edgeproxy opens one upstream
TCP connection. Under `--tls` each request therefore also performs a **full TLS
handshake** — there is no session resumption and no connection reuse. Responses
contain 128 bytes. Network latency and external services are absent.

Environment:

- OS: Microsoft Windows NT 10.0.26200.0
- CPU: Intel64 Family 6 Model 181, 14 logical processors exposed
- Rust: `rustc 1.96.0 (ac68faa20 2026-05-25)`
- Build: release, thin LTO, one codegen unit
- rustls 0.23 with the `ring` provider; TLS 1.3, self-signed RSA-2048
  certificate, ALPN `http/1.1`

## TLS overhead (the headline result)

Concurrency 1, 13 paired runs (7 x 300 requests + 6 x 600 requests),
11,400 requests total, **0 failures**. Median with min/max envelope:

| Metric | Plaintext | TLS | Overhead |
| --- | ---: | ---: | ---: |
| p50 latency | **3.97 ms** [2.96–6.79] | **6.27 ms** [5.14–9.98] | **+2.3 ms** |
| Average latency | 7.03 ms [3.22–39.68] | 8.34 ms [5.52–33.27] | +1.3 ms |
| Throughput | 142 req/s [25–311] | 120 req/s [30–181] | −15% |

**Per-request TLS cost: +2.36 ms median at p50, range +1.56 ms to +3.19 ms.**
The delta is computed per run and is **positive in 13 of 13 runs**, and the p50
min/max envelopes do not overlap (plaintext tops out at 6.79 ms, TLS starts at
5.14 ms — overlapping only across the two differently-sized series, never within
one). That non-overlap is the reason this figure is stated at all while the
concurrent numbers below are not.

That cost is what one full handshake costs here: RSA-2048 signing dominates, plus
key exchange, certificate transmission, and per-record AEAD on a 128-byte
response. It is a per-*connection* cost, not a per-*request* one — it looks
expensive in this benchmark precisely because the proxy's connection-per-request
model amortizes it over exactly one request. Connection reuse or session
resumption would spread it across many; neither is implemented (see the design
boundaries in the README). About 58% added to a 4 ms request (+2.30 ms on
3.97 ms) is a real and honest number for this workload, and it is also the
workload least favourable to TLS that this proxy can produce.

Throughput falls only ~15% (142 → 120 req/s), well short of what the latency
delta alone would predict: throughput scaling purely with p50 latency would fall
*to* 3.97/6.27 = ~63% of the plaintext rate, i.e. *by* ~37%. It falls by ~15%
instead because at concurrency 1 the client, not the proxy, is the bottleneck for
part of each cycle.

## What could not be measured here: concurrent load

**Concurrent measurements on this machine are dominated by Windows ephemeral-port
exhaustion, not by proxy or TLS cost, and no concurrent TLS-overhead figure is
published as a result.**

The port pool is 16,384 (`netsh int ipv4 show dynamicport tcp`: start 49152,
16384 ports) and `TcpTimedWaitDelay` is at its default. Each request consumes
**two** ephemeral ports (downstream client → proxy, proxy → upstream), and both
sit in `TIME_WAIT` after close. A 5,000-request paired run therefore needs
~20,000 ports against a 16,384-port pool.

Observed consequences, all of which are measurement artifacts:

- At 5,000 requests / concurrency 64, **2 of 6 runs failed outright** (1,920 and
  963 failed requests). The harness correctly refuses to report a partial run.
- At 2,000 requests / concurrency 64 (7 paired runs, 0 failures) the medians came
  out *faster for TLS* — p50 −10.8%, throughput +41.5% — with min/max envelopes
  spanning **7x** (plaintext p50 72–482 ms). TLS does not make a proxy faster;
  this is noise, and it is reported here only to show why the serial figure is
  the one used.
- Concurrency 16 and 8 were also tried. Concurrency 16 ran with 14,000–16,400
  sockets already in `TIME_WAIT`, i.e. at the pool limit. Concurrency 8 stayed
  clean (~1,700) but its per-run p50 delta still ranged −1.5 ms to +23.5 ms,
  straddling zero.

Serial runs use ~2 ports per request with no overlap, which is why only they
produce a stable signal. Fixing this properly needs downstream keep-alive and
upstream connection pooling — the same two items the README lists as the primary
next steps — or a machine with a widened port range.

## Microbenchmarks

`criterion` medians, which avoid the socket and scheduler noise above entirely.
Each row below is criterion's median point estimate over 100 samples, with its
**95% bootstrap confidence interval** for that median in brackets — the same
"envelope, not a point claim" treatment the TLS figures get, from criterion's own
interval data rather than from repeated whole-run invocations.

**All 18 of the suite's parameterized cases are published below, and each one is
backed by a committed artifact.** An earlier version of this file published only
11 and disclosed the other 7 as omitted, because `target/criterion/` is
gitignored and transcribing numbers from an untracked local artifact would
publish figures no reader could check. The estimates from that same measurement
pass are now committed under
[`benchmark/criterion/`](criterion) — one directory per case, holding
criterion's own `estimates.json`, `benchmark.json`, and `sample.json` copied
verbatim. `/target/` stays gitignored; only the small JSON estimates were
copied out.

So every row here is checkable: the median, both confidence bounds, and the
sample count each come from a file in this repository, and
`scripts/check_documented_metrics.py` fails CI if any row drifts from its
artifact or if any committed artifact stops being published. The 11 previously
published medians are unchanged — the 7 new rows were added beside them, none
was revised.

The two interior header-count points (`/16`, `/32`) were the substantive loss,
because without them the table showed only the endpoints and so could not speak
to the *shape* of the scaling at all. With all four points:

- Cost per additional header, taken between successive medians, is 262 ns
  (4→16), 313 ns (16→32), and 339 ns (32→60). The trend is upward, but
  propagating the confidence bounds through the same subtraction gives
  228–315 ns for the first interval against 247–410 ns for the last: the ranges
  overlap, so **this data does not resolve a superlinear term**, and none is
  claimed. What it does support is that growth is close to proportional in
  header count over 4–60 headers.
- The total is *sub*-proportional — 15x the headers costs 9.25x the time
  (2.14 us → 19.78 us) — which is the fixed per-request part of the parse
  (request line, framing checks, allocation) being amortized over more headers.
- Pool size now has a curve rather than a single point. `round_robin` is flat
  across 2/8/32 upstreams (60.8 / 63.1 / 64.8 ns, all three intervals
  overlapping) as an index increment should be. `least_connections` is flat from
  2 to 8 (74.2 / 75.3 ns, overlapping) and then 2.5x higher at 32 (191 ns,
  no overlap), consistent with its scan over upstreams only becoming visible
  once the pool is large enough to dominate the fixed cost. Two pool sizes
  would not have distinguished those two shapes; three do.

| Benchmark | Median | 95% CI | Samples |
| --- | ---: | ---: | ---: |
| `parse_request_head/accept/minimal` | 734 ns | [701–779] | 100 |
| `parse_request_head/accept/typical_browser` | 3.80 us | [3.63–4.01] | 100 |
| `parse_request_head/accept/post_with_body` | 2.66 us | [2.50–2.74] | 100 |
| `parse_request_head/reject/missing_host` | 743 ns | [710–769] | 100 |
| `parse_request_head/reject/cl_plus_te` | 2.53 us | [2.23–3.11] | 100 |
| `parse_request_head/reject/duplicate_content_length` | 1.42 us | [1.36–1.54] | 100 |
| `parse_request_head_header_count/4` | 2.14 us | [1.95–2.27] | 100 |
| `parse_request_head_header_count/16` | 5.28 us | [5.00–5.73] | 100 |
| `parse_request_head_header_count/32` | 10.28 us | [9.74–10.95] | 100 |
| `parse_request_head_header_count/60` | 19.78 us | [17.86–21.23] | 100 |
| `load_balancer_select/round_robin/2` | 60.8 ns | [57.7–63.8] | 100 |
| `load_balancer_select/round_robin/8` | 63.1 ns | [58.2–65.0] | 100 |
| `load_balancer_select/round_robin/32` | 64.8 ns | [61.5–67.3] | 100 |
| `load_balancer_select/least_connections/2` | 74.2 ns | [69.5–76.0] | 100 |
| `load_balancer_select/least_connections/8` | 75.3 ns | [71.8–80.2] | 100 |
| `load_balancer_select/least_connections/32` | 191 ns | [178–199] | 100 |
| `load_balancer_select_degraded/round_robin/31_of_32_open` | 1.25 us | [1.17–1.32] | 100 |
| `load_balancer_select_degraded/least_connections/31_of_32_open` | 2.38 us | [2.25–2.50] | 100 |

Each CI is in the same unit as the median beside it. These intervals are much
tighter than the load-generator envelopes above — a few percent rather than 7x —
because criterion measures in-process CPU work with no socket, no scheduler
hand-off, and no port table involved.

Parsing a typical request head costs 3.80 us [3.63–4.01] and picking an upstream
64.8 ns [61.5–67.3], against a ~2.3 ms TLS handshake. That is **605x** for the
parse and **~35,000x** for the selection — between two and four and a half orders
of magnitude below the handshake cost, a gap far wider than either envelope — so
the TLS figure above is measuring cryptography, not proxy overhead.

## Corrections to previously published figures

The previous version of this file reported a 2026-07-16 run of 5,000 requests at
concurrency 64: **331.28 req/s, 190.37 ms average, p50/p95/p99 155.30 / 380.50 /
602.56 ms**, plus a 20,000-request "saturation observation" of 15,564 successes
and 4,436 failures.

**Those figures are withdrawn.** They do not reproduce and they were never
comparable in the first place:

1. They are single runs. Repeating that workload here spans a 7x envelope, and
   both the old throughput and the old latencies fall inside it — so the old
   numbers were reporting run-to-run variance as a result.
2. The workload was above the ephemeral-port limit described above, so it was
   measuring socket-table pressure. The old file attributed the 4,436 failures to
   "Windows socket/ephemeral-port pressure" without drawing the conclusion that
   the *successful* baseline was affected by the same thing.
3. They predate TLS termination, so they cannot serve as the plaintext baseline
   for a TLS comparison. That is what the serial figures above are for.

No number in this file is a point claim. Every load-generator figure is a median
over at least 6 paired runs with its min/max envelope shown, and every
microbenchmark figure is a criterion median over 100 samples with its 95%
confidence interval shown. Coverage is now complete on the microbenchmark side:
all 18 of the suite's parameterized cases are published, each against a
committed estimate under [`benchmark/criterion/`](criterion), and
`scripts/check_documented_metrics.py` enforces that in CI. The load-generator
side is still deliberately partial — no concurrent figure is published at all,
for the reasons under
[What could not be measured here](#what-could-not-be-measured-here-concurrent-load).

## Measurement caveats

- The measurement box is a **power-managed laptop with unpinned clocks**, running
  a normal desktop workload. Background activity was observed cycling to ~10,000
  `TIME_WAIT` sockets independently of this benchmark. Figures are envelopes, not
  point claims; republishing on a quiesced machine with a widened port range
  would tighten them.
- **No comparison against nginx, HAProxy, or Envoy was run, and no parity with
  them is claimed.** These numbers characterize this proxy against itself
  (plaintext vs TLS) on one machine.
- Loopback only. There is no network RTT, so TLS handshake cost appears as pure
  CPU; over a real WAN the extra round trips would dominate the ~2.3 ms measured
  here.
- The benchmark client skips certificate-chain and hostname verification because
  the certificate is self-signed and throwaway. Key exchange and transcript
  signing — the expensive parts — still happen.
- `benchmark/latest.json` and `benchmark/latest-tls.json` hold the most recent
  run of each leg and are gitignored; they are overwritten on every invocation
  and are not the source of the medians above.
