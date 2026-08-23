# Multi-stage build: a full Rust toolchain to compile, a distroless runtime to ship.
#
# The runtime stage is `gcr.io/distroless/cc-debian12`, which carries glibc and
# libgcc but no shell and no package manager. `cc` rather than `static` because
# the default `x86_64-unknown-linux-gnu` target links dynamically against glibc.
# rustls uses the pure-Rust `ring` provider, so no OpenSSL is needed at runtime.

FROM rust:1.86-bookworm AS builder

WORKDIR /build

# Copy manifests first so dependency compilation caches independently of source
# edits. `--locked` fails loudly if Cargo.lock is out of date rather than
# silently resolving different versions.
COPY Cargo.toml Cargo.lock ./

# A stub tree lets cargo resolve and compile dependencies before the real sources
# land. The stubs are removed afterwards so the real build cannot reuse them.
RUN mkdir -p src benches \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > benches/request_parsing.rs \
    && echo 'fn main() {}' > benches/load_balancer.rs \
    && cargo build --release --locked --bin edgeproxy \
    && rm -rf src benches

COPY src ./src
COPY benches ./benches

# Touch the entry points so cargo cannot mistake the stub fingerprints for
# current ones.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked --bin edgeproxy \
    && strip target/release/edgeproxy

FROM gcr.io/distroless/cc-debian12:nonroot

# distroless `nonroot` is uid/gid 65532. The proxy writes nothing to disk, so the
# container also runs with a read-only root filesystem (see compose.yaml).
USER nonroot:nonroot

WORKDIR /app

COPY --from=builder /build/target/release/edgeproxy /usr/local/bin/edgeproxy

# Documentation only; actual binds come from the config file.
# 8080 plaintext, 8443 TLS, 9090 metrics.
EXPOSE 8080 8443 9090

# No shell exists in distroless, so this must be exec form.
ENTRYPOINT ["/usr/local/bin/edgeproxy"]
CMD ["--config", "/etc/edgeproxy/edgeproxy.toml"]
