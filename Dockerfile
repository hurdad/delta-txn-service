# ======================================================
# Builder
# ======================================================
FROM rust:1.94-trixie AS builder

WORKDIR /build

# Native deps needed for:
# - tonic-build (protoc)
# - delta-rs (openssl)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Pre-build dependency layer for caching
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
# Warms the release-test-profile dependency build too (see Cargo.toml's
# [profile.release-test] comment) -- `cargo test` below uses that profile,
# and without this it would recompile the whole dependency graph from
# scratch under a differently-cached profile on every source change.
RUN cargo test --profile release-test --no-run
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

# Build the real binary
RUN cargo build --release
# Run tests to validate the build artifacts. Uses the release-test profile
# (Cargo.toml), not --release directly -- see that profile's comment: fat
# LTO + codegen-units=1 against test binaries, on top of the
# deltalake+datafusion dependency graph, is enough peak memory during
# linking to OOM a Docker build.
RUN cargo test --profile release-test


# ======================================================
# Runtime
# ======================================================
FROM debian:trixie-slim

WORKDIR /app

# Runtime deps only
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 delta-txn

# ---- IMPORTANT ----
# Adjust the binary name here if you rename the crate
# Default assumes:
#   [package]
#   name = "delta-txn-service"
COPY --from=builder /build/target/release/delta-txn-service /usr/local/bin/delta-txn-service

EXPOSE 50051

ENV RUST_LOG=info \
    AWS_REGION=us-east-1

USER delta-txn

ENTRYPOINT ["/usr/local/bin/delta-txn-service"]
