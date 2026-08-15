# Protocol and Failure Model

## Scope

edgeproxy is an HTTP/1.1 reverse proxy over TCP. The accepted downstream request
profile is intentionally smaller than all of RFC 9112:

- HTTP version must be exactly 1.1.
- Request target must be origin-form and begin with `/`.
- Exactly one nonempty `Host` header is required.
- Headers and request bodies are read under absolute deadlines.
- Requests may have no body or a bounded `Content-Length` body.
- Duplicate `Content-Length` is accepted only when every value matches.
- `Transfer-Encoding`, `Content-Length` plus `Transfer-Encoding`,
  `Expect`, upgrades, and header values containing CR/LF/NUL are rejected.

The parser first finds a bounded `CRLF CRLF`, then delegates token parsing to
`httparse`, then applies framing and semantic validation. Bodies are buffered
only up to `limits.max_body_bytes`. This bounded buffering makes eligible
requests replayable while avoiding unbounded memory growth.

The proxy strips hop-by-hop headers, writes a normalized `Host`, replaces
`X-Forwarded-For` with the immediate client IP, adds `Via: 1.1 edgeproxy`, and
uses `Connection: close` upstream. Upstream response headers are bounded and
reparsed. Conflicting response framing is rejected; fixed-length bodies are
copied exactly, while chunked and close-delimited bodies stream until close.

## Request Lifecycle

1. The listener accepts a TCP connection.
2. `Semaphore::try_acquire_owned` admits it or returns a bounded `503`.
3. Header and body readers enforce independent absolute deadlines and limits.
4. The selected balancing policy chooses a healthy, circuit-eligible upstream.
5. Connect, write, response-header, idle-body, and downstream-write phases each
   have independent timeouts.
6. No response bytes are sent downstream until a complete valid upstream
   response head is available.
7. The response body streams through a 16 KiB heap buffer.
8. A structured access event records method, target, status, upstream, retries,
   bytes, latency, client, and terminal error.

One connection carries one request. Pipelining and downstream keep-alive are not
implemented.

## Balancing

Round-robin uses an atomic cursor and skips unavailable upstreams.
Least-connections chooses the eligible upstream with the lowest atomic active
count, using the rotating cursor to avoid a permanent tie preference. The
active count covers connect through response completion.

An upstream is eligible only when active health state is healthy and its
circuit is closed. After the open duration, a compare-and-swap admits one
half-open request; other requests continue to skip that upstream.

## Retry Matrix

| Failure point | Non-idempotent method | Idempotent method |
| --- | --- | --- |
| TCP connect failed/timed out; no request bytes sent | Retry | Retry |
| Request write failed or timed out | Do not retry | Retry |
| Upstream closed/timed out before a complete response head | Do not retry | Retry |
| Configured 5xx response before downstream forwarding | Do not retry | Retry |
| Any failure after downstream response forwarding starts | Never retry | Never retry |

Idempotent methods are `GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT`, and `DELETE`.
The attempt count includes the first request and is capped at five by config
validation. Retrying `PUT` or `DELETE` after an uncertain write follows HTTP
idempotency semantics but still requires idempotent upstream implementations.

## Health Checks and Circuit Breakers

Every interval, edgeproxy connects to each upstream and sends:

```http
GET /configured-health-path HTTP/1.1
Host: upstream-address
Connection: close
```

Statuses 200 through 399 are healthy. Consecutive success/failure thresholds
prevent single-probe flapping. A successful active check closes the circuit and
resets passive failures.

Passive network failures, malformed response heads, configured retryable 5xx
statuses, and premature fixed-length response termination increment the
circuit breaker. Client disconnects do not penalize the upstream.

Health and circuit state are process-local and reset on restart.

## Backpressure and Shutdown

`max_in_flight` is a hard process-local admission limit. It bounds connection
tasks and their request-body allocations. The kernel accept backlog remains the
outer queue. Over-capacity clients receive `503 Service Unavailable`; edgeproxy
does not create an unbounded rejection task per connection.

On Ctrl+C, the listener stops admitting new work, health and metrics listeners
are signaled to stop, and admitted requests drain for `shutdown_grace_ms`.
Remaining tasks are aborted after that deadline.

## Observability

Application and access logs are newline-delimited JSON through `tracing`.
Prometheus text metrics include:

- request/response totals by broad status class
- retries, upstream failures, circuit openings, and overload rejections
- active downstream and per-upstream connection counts
- health-check totals and failures
- bytes received/sent
- cumulative request-latency histogram
- per-upstream health and circuit gauges

Metrics have bounded cardinality: the only label is configured upstream name.

## Security and Operational Boundaries

- There is no TLS termination or upstream TLS. Deploy behind a trusted TLS
  terminator or on a protected network.
- Forwarded client identity trusts the immediate peer only; incoming
  `X-Forwarded-For` is replaced.
- Configuration is read once at startup and is not dynamically reloaded.
- No request body is streamed upstream; the configured body cap is therefore
  part of capacity planning.
- No upstream connection pool exists. This simplifies failure attribution but
  increases TCP churn and limits high-load efficiency, especially on Windows.
- Circuit breakers are not distributed across replicas.

