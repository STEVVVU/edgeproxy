//! Throwaway self-signed certificate generation for tests.
//!
//! Certificates are generated at test time with `rcgen` and written to a
//! temporary directory, so no key material is ever committed to the repository.
//! Everything produced here is test-only and lives for the duration of the run.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A generated certificate/key pair on disk plus its DER form for client trust.
pub struct TestCertificate {
    pub certificate_path: PathBuf,
    pub private_key_path: PathBuf,
    pub certificate_der: Vec<u8>,
}

/// Creates a unique temporary directory for one test's key material.
pub fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "edgeproxy-test-only-{tag}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp certificate directory");
    dir
}

/// Generates a self-signed certificate covering `names` and writes it as PEM.
///
/// The file stem is prefixed `test-only-` so the intent is obvious if the files
/// are ever inspected outside the test run.
pub fn generate(dir: &Path, stem: &str, names: &[&str]) -> TestCertificate {
    let subject_alt_names = names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let generated = rcgen::generate_simple_self_signed(subject_alt_names)
        .expect("generate self-signed test certificate");

    let certificate_path = dir.join(format!("test-only-{stem}.crt"));
    let private_key_path = dir.join(format!("test-only-{stem}.key"));
    fs::write(&certificate_path, generated.cert.pem()).expect("write test certificate");
    fs::write(&private_key_path, generated.signing_key.serialize_pem())
        .expect("write test private key");

    TestCertificate {
        certificate_path,
        private_key_path,
        certificate_der: generated.cert.der().to_vec(),
    }
}
