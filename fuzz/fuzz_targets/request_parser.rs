#![no_main]

use edgeproxy::config::Limits;
use edgeproxy::http::parse_request_head;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_request_head(data, &Limits::default());
});

