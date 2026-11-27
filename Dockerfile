# ======================================================
# Builder
# ======================================================
# Pinned to the exact toolchain CI uses (dtolnay/rust-toolchain@1.94.1) and
# Cargo.toml's rust-version, so the image and CI build the same way.
FROM rust:1.99.1-trixie AS builder

WORKDIR /build

# protoc for tonic-prost-build. No OpenSSL: TLS is rustls throughout
# (nothing in Cargo.lock depends on openssl-sys).
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Pre-build dependency layer for caching
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked
# Warms the release-test-profile dependency build too (see Cargo.toml's
# [profile.release-test] comment) -- `cargo test` below uses that profile,
# and without this it would recompile the whole dependency graph from
# scratch under a differently-cached profile on every source change.
RUN cargo test --profile release-test --locked --no-run
RUN rm -rf src

# Copy real source
COPY build.rs .
COPY proto ./proto
COPY src ./src
# The gRPC integration test suite (tests/README.md) -- without this, `cargo
# test` below finds no tests/ directory at all and silently runs zero
# integration tests, passing "successfully" while covering nothing beyond
# the unit tests embedded in src/.
COPY tests ./tests

# Build the real binary. COPY keeps the source files' original mtimes, which
# can be older than the dummy build above; touching them makes sure cargo
# rebuilds this crate instead of treating the dummy artifacts as fresh.
RUN touch build.rs src/main.rs src/lib.rs && cargo build --release --locked
# Run tests to validate the build artifacts. Uses the release-test profile
# (Cargo.toml), not --release directly -- see that profile's comment: fat
# LTO + codegen-units=1 against test binaries, on top of the
# deltalake/arrow/parquet dependency graph, is enough peak memory during
# linking to OOM a Docker build.
RUN cargo test --profile release-test --locked


# ======================================================
# Runtime
# ======================================================
FROM debian:trixie-slim

WORKDIR /app

# Runtime deps only: CA roots for rustls' platform verifier.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 delta-txn \
    && useradd --system --no-create-home --uid 10001 --gid 10001 delta-txn

# ---- IMPORTANT ----
# Adjust the binary name here if you rename the crate
# Default assumes:
#   [package]
#   name = "delta-txn-service"
COPY --from=builder /build/target/release/delta-txn-service /usr/local/bin/delta-txn-service

EXPOSE 50051

# AWS_REGION is only a default: delta-rs' S3 client needs some region even
# for self-hosted S3-compatible endpoints. Real AWS deployments should set their own.
ENV RUST_LOG=info \
    AWS_REGION=us-east-1

# Numeric, so Kubernetes can verify runAsNonRoot without the pod spec
# having to set runAsUser.
USER 10001:10001

ENTRYPOINT ["/usr/local/bin/delta-txn-service"]
