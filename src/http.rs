use std::fmt::Write as _;
use std::str;

use bytes::{Buf, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout_at, Duration, Instant};

use crate::config::{Limits, Timeouts};

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn is_idempotent(&self) -> bool {
        matches!(
            self.method.as_str(),
            "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE"
        )
    }

    pub fn upstream_bytes(
        &self,
        upstream_authority: &str,
        client_ip: &str,
        preserve_host: bool,
    ) -> Vec<u8> {
        let mut output = Vec::with_capacity(
            self.target.len() + self.body.len() + self.headers.len().saturating_mul(32),
        );
        output.extend_from_slice(self.method.as_bytes());
        output.extend_from_slice(b" ");
        output.extend_from_slice(self.target.as_bytes());
        output.extend_from_slice(b" HTTP/1.1\r\n");

        let mut original_host = None;
        for (name, value) in &self.headers {
            if name == "host" {
                original_host = Some(value.as_slice());
                continue;
            }
            if is_hop_by_hop(name) || name == "content-length" || name == "x-forwarded-for" {
                continue;
            }
            output.extend_from_slice(name.as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(value);
            output.extend_from_slice(b"\r\n");
        }

        output.extend_from_slice(b"host: ");
        output.extend_from_slice(if preserve_host {
            original_host.unwrap_or(upstream_authority.as_bytes())
        } else {
            upstream_authority.as_bytes()
        });
        output.extend_from_slice(b"\r\nx-forwarded-for: ");
        output.extend_from_slice(client_ip.as_bytes());
        output
            .extend_from_slice(b"\r\nvia: 1.1 edgeproxy\r\nconnection: close\r\ncontent-length: ");
        output.extend_from_slice(self.body.len().to_string().as_bytes());
        output.extend_from_slice(b"\r\n\r\n");
        output.extend_from_slice(&self.body);
        output
    }
}

#[derive(Debug)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub content_length: usize,
    pub consumed: usize,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("request timed out")]
    Timeout,
    #[error("connection closed before request completed")]
    UnexpectedEof,
    #[error("malformed HTTP request")]
    Malformed,
    #[error("only HTTP/1.1 is supported")]
    Version,
    #[error("request headers exceed configured limit")]
    HeadersTooLarge,
    #[error("request line exceeds configured limit")]
    RequestLineTooLarge,
    #[error("too many request headers")]
    TooManyHeaders,
    #[error("Host header is missing or duplicated")]
    InvalidHost,
    #[error("ambiguous request body framing")]
    AmbiguousFraming,
    #[error("chunked request bodies are not supported")]
    UnsupportedTransferEncoding,
    #[error("Expect requests are not supported")]
    UnsupportedExpectation,
    #[error("request body exceeds configured limit")]
    BodyTooLarge,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl ParseError {
    pub const fn status(&self) -> u16 {
        match self {
            Self::Timeout => 408,
            Self::HeadersTooLarge | Self::RequestLineTooLarge | Self::TooManyHeaders => 431,
            Self::BodyTooLarge => 413,
            Self::UnsupportedTransferEncoding => 501,
            Self::UnsupportedExpectation => 417,
            Self::UnexpectedEof
            | Self::Malformed
            | Self::Version
            | Self::InvalidHost
            | Self::AmbiguousFraming
            | Self::Io(_) => 400,
        }
    }
}

pub fn parse_request_head(input: &[u8], limits: &Limits) -> Result<RequestHead, ParseError> {
    if input.len() > limits.max_header_bytes {
        return Err(ParseError::HeadersTooLarge);
    }

    let request_line_end = find_bytes(input, b"\r\n");
    if request_line_end.is_none() && input.len() > limits.max_request_line_bytes {
        return Err(ParseError::RequestLineTooLarge);
    }
    if request_line_end.is_some_and(|end| end > limits.max_request_line_bytes) {
        return Err(ParseError::RequestLineTooLarge);
    }

    let mut headers = vec![httparse::EMPTY_HEADER; limits.max_headers];
    let mut parsed = httparse::Request::new(&mut headers);
    let consumed = match parsed.parse(input) {
        Ok(httparse::Status::Complete(consumed)) => consumed,
        Ok(httparse::Status::Partial) => return Err(ParseError::UnexpectedEof),
        Err(httparse::Error::TooManyHeaders) => return Err(ParseError::TooManyHeaders),
        Err(_) => return Err(ParseError::Malformed),
    };

    if parsed.version != Some(1) {
        return Err(ParseError::Version);
    }
    let method = parsed.method.ok_or(ParseError::Malformed)?;
    if !valid_token(method.as_bytes()) {
        return Err(ParseError::Malformed);
    }
    let target = parsed.path.ok_or(ParseError::Malformed)?;
    if !target.starts_with('/') || target.contains('#') {
        return Err(ParseError::Malformed);
    }

    let mut host_count = 0;
    let mut content_lengths = Vec::new();
    let mut has_transfer_encoding = false;
    let mut normalized = Vec::with_capacity(parsed.headers.len());

    for header in parsed.headers {
        let name = header.name.to_ascii_lowercase();
        if header
            .value
            .iter()
            .any(|byte| matches!(byte, b'\r' | b'\n' | 0))
        {
            return Err(ParseError::Malformed);
        }
        match name.as_str() {
            "host" => {
                host_count += 1;
                if trim_ascii(header.value).is_empty() {
                    return Err(ParseError::InvalidHost);
                }
            }
            "content-length" => {
                let raw = str::from_utf8(trim_ascii(header.value))
                    .map_err(|_| ParseError::AmbiguousFraming)?;
                let length = raw
                    .parse::<usize>()
                    .map_err(|_| ParseError::AmbiguousFraming)?;
                content_lengths.push(length);
            }
            "transfer-encoding" => has_transfer_encoding = true,
            "expect" => return Err(ParseError::UnsupportedExpectation),
            _ => {}
        }
        normalized.push((name, trim_ascii(header.value).to_vec()));
    }

    if host_count != 1 {
        return Err(ParseError::InvalidHost);
    }
    if has_transfer_encoding && !content_lengths.is_empty() {
        return Err(ParseError::AmbiguousFraming);
    }
    if has_transfer_encoding {
        return Err(ParseError::UnsupportedTransferEncoding);
    }
    if content_lengths.windows(2).any(|pair| pair[0] != pair[1]) {
        return Err(ParseError::AmbiguousFraming);
    }
    let content_length = content_lengths.first().copied().unwrap_or(0);
    if content_length > limits.max_body_bytes {
        return Err(ParseError::BodyTooLarge);
    }

    Ok(RequestHead {
        method: method.to_owned(),
        target: target.to_owned(),
        headers: normalized,
        content_length,
        consumed,
    })
}

/// Reads one request from any byte stream (plaintext TCP or TLS).
///
/// Generic over the transport so TLS termination reuses the exact same framing
/// and validation path as plaintext; there is no second parser to keep in sync.
pub async fn read_request<S>(
    stream: &mut S,
    limits: &Limits,
    timeouts: &Timeouts,
) -> Result<Request, ParseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let header_deadline = Instant::now() + Duration::from_millis(timeouts.request_header_ms);
    let mut buffer = BytesMut::with_capacity(limits.max_header_bytes.min(4096));

    let head = loop {
        if let Some(end) = find_bytes(&buffer, b"\r\n\r\n") {
            let head_length = end + 4;
            if head_length > limits.max_header_bytes {
                return Err(ParseError::HeadersTooLarge);
            }
            break parse_request_head(&buffer[..head_length], limits)?;
        }
        if buffer.len() >= limits.max_header_bytes {
            return Err(ParseError::HeadersTooLarge);
        }
        let read = timeout_at(header_deadline, stream.read_buf(&mut buffer))
            .await
            .map_err(|_| ParseError::Timeout)??;
        if read == 0 {
            return Err(ParseError::UnexpectedEof);
        }
    };

    buffer.advance(head.consumed);
    let mut body = Vec::with_capacity(head.content_length);
    let buffered = head.content_length.min(buffer.len());
    body.extend_from_slice(&buffer[..buffered]);

    let body_deadline = Instant::now() + Duration::from_millis(timeouts.request_body_ms);
    while body.len() < head.content_length {
        let remaining = head.content_length - body.len();
        let mut chunk = vec![0_u8; remaining.min(16 * 1024)];
        let read = timeout_at(body_deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| ParseError::Timeout)??;
        if read == 0 {
            return Err(ParseError::UnexpectedEof);
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(Request {
        method: head.method,
        target: head.target,
        headers: head.headers,
        body,
    })
}

pub async fn write_error<S>(stream: &mut S, status: u16, message: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let reason = reason_phrase(status);
    let body = format!("{status} {reason}\n{message}\n");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub consumed: usize,
    pub body_forbidden: bool,
    pub content_length: Option<usize>,
}

pub fn parse_response_head(
    input: &[u8],
    max_headers: usize,
    request_method: &str,
) -> Result<ResponseHead, &'static str> {
    let mut headers = vec![httparse::EMPTY_HEADER; max_headers];
    let mut response = httparse::Response::new(&mut headers);
    let consumed = match response.parse(input) {
        Ok(httparse::Status::Complete(consumed)) => consumed,
        Ok(httparse::Status::Partial) => return Err("partial response"),
        Err(_) => return Err("malformed response"),
    };
    if !matches!(response.version, Some(0 | 1)) {
        return Err("unsupported response version");
    }
    let status = response.code.ok_or("missing response status")?;
    let mut output = String::with_capacity(consumed + 64);
    write!(output, "HTTP/1.1 {} {}\r\n", status, reason_phrase(status))
        .map_err(|_| "formatting response failed")?;

    let mut content_length = None;
    let mut chunked = false;
    for header in response.headers.iter() {
        let name = header.name.to_ascii_lowercase();
        match name.as_str() {
            "content-length" => {
                let value = str::from_utf8(trim_ascii(header.value))
                    .map_err(|_| "invalid response content length")?;
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| "invalid response content length")?;
                if content_length.is_some_and(|existing| existing != parsed) {
                    return Err("conflicting response content lengths");
                }
                content_length = Some(parsed);
            }
            "transfer-encoding" => {
                let value = str::from_utf8(header.value).map_err(|_| "non-UTF8 response header")?;
                if !value.eq_ignore_ascii_case("chunked") {
                    return Err("unsupported response transfer encoding");
                }
                chunked = true;
            }
            _ => {}
        }
    }
    if content_length.is_some() && chunked {
        return Err("ambiguous response framing");
    }

    for header in response.headers.iter() {
        let name = header.name.to_ascii_lowercase();
        if is_hop_by_hop(&name) || matches!(name.as_str(), "content-length" | "transfer-encoding") {
            continue;
        }
        let value = str::from_utf8(header.value).map_err(|_| "non-UTF8 response header")?;
        if value.contains(['\r', '\n', '\0']) {
            return Err("invalid response header");
        }
        write!(output, "{name}: {value}\r\n").map_err(|_| "formatting response failed")?;
    }
    if let Some(length) = content_length {
        write!(output, "content-length: {length}\r\n").map_err(|_| "formatting response failed")?;
    } else if chunked {
        output.push_str("transfer-encoding: chunked\r\n");
    }
    output.push_str("connection: close\r\n\r\n");
    let body_forbidden =
        request_method == "HEAD" || (100..200).contains(&status) || matches!(status, 204 | 304);
    Ok(ResponseHead {
        status,
        bytes: output.into_bytes(),
        consumed,
        body_forbidden,
        content_length,
    })
}

fn valid_token(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

pub const fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        417 => "Expectation Failed",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_strict_http_11_request() {
        let limits = Limits::default();
        let head = parse_request_head(
            b"GET /items?q=1 HTTP/1.1\r\nHost: example.test\r\nAccept: */*\r\n\r\n",
            &limits,
        )
        .expect("valid request");
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/items?q=1");
        assert_eq!(head.content_length, 0);
    }

    #[test]
    fn rejects_request_smuggling_framing() {
        let limits = Limits::default();
        let result = parse_request_head(
            b"POST / HTTP/1.1\r\nHost: example.test\r\nContent-Length: 4\r\n\
              Transfer-Encoding: chunked\r\n\r\n",
            &limits,
        );
        assert!(matches!(result, Err(ParseError::AmbiguousFraming)));
    }

    #[test]
    fn rejects_conflicting_content_lengths() {
        let limits = Limits::default();
        let result = parse_request_head(
            b"POST / HTTP/1.1\r\nHost: example.test\r\nContent-Length: 4\r\n\
              Content-Length: 5\r\n\r\n",
            &limits,
        );
        assert!(matches!(result, Err(ParseError::AmbiguousFraming)));
    }

    proptest! {
        #[test]
        fn parser_never_panics(input in proptest::collection::vec(any::<u8>(), 0..20_000)) {
            let _ = parse_request_head(&input, &Limits::default());
        }
    }
}
