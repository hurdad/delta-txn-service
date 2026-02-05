# ======================================================
# Builder
# ======================================================
FROM rust:1.93-trixie AS builder

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
RUN rm -rf src

# Copy real source
COPY build.rs .
COPY proto ./proto
COPY src ./src

# Build the real binary
RUN cargo build --release
# Run tests to validate the build artifacts
RUN cargo test --release


# ======================================================
# Runtime
# ======================================================
FROM debian:trixie-slim

WORKDIR /app

# Runtime deps only
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# ---- IMPORTANT ----
# Adjust the binary name here if you rename the crate
# Default assumes:
#   [package]
#   name = "delta-txn-service"
COPY --from=builder /build/target/release/delta-txn-service /usr/local/bin/delta-txn-service

EXPOSE 50051

ENV RUST_LOG=info \
    AWS_REGION=us-east-1

ENTRYPOINT ["/usr/local/bin/delta-txn-service"]
