//! Criterion microbenchmarks for the HTTP/1.1 request-head parser.
//!
//! These complement `scripts/benchmark.ps1`, which measures end-to-end proxy
//! throughput over real sockets. That harness answers "how many requests per
//! second can the proxy serve"; this one isolates parsing cost so a regression
//! in `http::parse_request_head` is visible without socket and scheduler noise.
//!
//! Rejection paths are benchmarked alongside the happy path because the parser
//! is reachable by untrusted input: a malformed or hostile request must be
//! cheap to refuse, not just correct to refuse.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use edgeproxy::config::Limits;
use edgeproxy::http::parse_request_head;
use std::hint::black_box;

/// Minimal valid request: the cheapest realistic parse.
const MINIMAL: &[u8] = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";

/// Request with a browser-like header set, the common production shape.
const TYPICAL: &[u8] = b"GET /api/v1/items?page=3&limit=50 HTTP/1.1\r\n\
host: api.example.test\r\n\
user-agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko)\r\n\
accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,*/*;q=0.8\r\n\
accept-language: en-US,en;q=0.9\r\n\
accept-encoding: gzip, deflate, br\r\n\
cookie: session=abc123def456; theme=dark; consent=1\r\n\
referer: https://api.example.test/items\r\n\
connection: keep-alive\r\n\
\r\n";

/// POST with a declared body, exercising the Content-Length framing path.
const WITH_BODY: &[u8] = b"POST /api/v1/items HTTP/1.1\r\n\
host: api.example.test\r\n\
content-type: application/json\r\n\
content-length: 27\r\n\
\r\n\
{\"name\":\"widget\",\"count\":3}";

/// CL + TE: the classic request-smuggling vector. Must be rejected quickly.
const SMUGGLING_CL_TE: &[u8] = b"POST / HTTP/1.1\r\n\
host: example.test\r\n\
content-length: 4\r\n\
transfer-encoding: chunked\r\n\
\r\n";

/// Conflicting duplicate Content-Length, also a smuggling vector.
const SMUGGLING_DUPLICATE_CL: &[u8] = b"POST / HTTP/1.1\r\n\
host: example.test\r\n\
content-length: 4\r\n\
content-length: 5\r\n\
\r\n";

/// Missing Host: rejected by the single-nonempty-Host rule.
const MISSING_HOST: &[u8] = b"GET / HTTP/1.1\r\naccept: */*\r\n\r\n";

fn parse_benchmarks(criterion: &mut Criterion) {
    let limits = Limits::default();
    let mut group = criterion.benchmark_group("parse_request_head");

    for (name, input) in [
        ("minimal", MINIMAL),
        ("typical_browser", TYPICAL),
        ("post_with_body", WITH_BODY),
    ] {
        // Throughput is reported over the head bytes actually parsed so results
        // are comparable across inputs of different sizes.
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("accept", name),
            &input,
            |bencher, input| {
                bencher.iter(|| parse_request_head(black_box(input), black_box(&limits)));
            },
        );
    }

    for (name, input) in [
        ("cl_plus_te", SMUGGLING_CL_TE),
        ("duplicate_content_length", SMUGGLING_DUPLICATE_CL),
        ("missing_host", MISSING_HOST),
    ] {
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("reject", name),
            &input,
            |bencher, input| {
                bencher.iter(|| parse_request_head(black_box(input), black_box(&limits)));
            },
        );
    }

    group.finish();
}

/// Measures how parse cost scales with header count, which is what
/// `limits.max_headers` bounds.
fn header_count_scaling(criterion: &mut Criterion) {
    let limits = Limits::default();
    let mut group = criterion.benchmark_group("parse_request_head_header_count");

    for count in [4_usize, 16, 32, 60] {
        let mut request = b"GET /scaling HTTP/1.1\r\nhost: example.test\r\n".to_vec();
        for index in 0..count {
            request.extend_from_slice(format!("x-header-{index}: value-{index}\r\n").as_bytes());
        }
        request.extend_from_slice(b"\r\n");

        group.throughput(Throughput::Bytes(request.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &request,
            |bencher, request| {
                bencher.iter(|| parse_request_head(black_box(request), black_box(&limits)));
            },
        );
    }

    group.finish();
}

criterion_group!(benches, parse_benchmarks, header_count_scaling);
criterion_main!(benches);
