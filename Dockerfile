# ======================================================
# Builder
# ======================================================
# Pinned to the exact toolchain CI uses (dtolnay/rust-toolchain@1.100.0),
# rust-toolchain.toml and Cargo.toml's rust-version, so the image and CI
# build the same way.
# Pinned by digest (multi-arch index) as well as tag, so a rebuild can't
# silently pick up a different image under the same tag. Dependabot's docker
# ecosystem updates tag and digest together.
FROM rust:1.100.0-trixie@sha256:e84068000361bf3d4a5ba2b7647f62893528b1eb9f847d7a23b493f5ce8ea254 AS builder

WORKDIR /build

# protoc for tonic-prost-build; musl-tools (musl-gcc) for the C parts of
# the dependency graph (ring, aws-lc-sys, mimalloc). No OpenSSL: TLS is
# rustls throughout (nothing in Cargo.lock depends on openssl-sys).
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    musl-tools \
    && rm -rf /var/lib/apt/lists/*

# Fully static binary (musl), so the runtime image needs no libc at all
# and can be distroless/static. Derived from the build platform so an
# arm64 build picks aarch64-unknown-linux-musl. musl's own malloc is slow
# under multithreaded load, so musl builds use mimalloc (see main.rs).
RUN uname -m > /tmp/arch \
    && echo "$(cat /tmp/arch)-unknown-linux-musl" > /build/.target \
    && rustup target add "$(cat /build/.target)"

# Pre-build dependency layer for caching. The dummy main uses none of the
# dependencies, so cargo's unused_dependencies lint (Rust 1.100+) would
# warn about every one of them; it's allowed for these cache steps only, and
# the real Cargo.toml is restored below before the real build.
COPY Cargo.toml Cargo.lock ./
COPY .cargo/config.toml ./.cargo/config.toml
RUN printf '\n[lints.cargo]\nunused_dependencies = "allow"\n' >> Cargo.toml
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked --target "$(cat .target)"
# Warms the release-test-profile dependency build too (see Cargo.toml's
# [profile.release-test] comment) -- `cargo test` below uses that profile,
# and without this it would recompile the whole dependency graph from
# scratch under a differently-cached profile on every source change.
RUN cargo test --profile release-test --locked --target "$(cat .target)" --no-run
RUN rm -rf src

# Copy real source (and the unmodified manifest)
COPY Cargo.toml ./
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
RUN touch build.rs src/main.rs src/lib.rs \
    && cargo build --release --locked --target "$(cat .target)" \
    && cp "target/$(cat .target)/release/delta-txn-service" /build/delta-txn-service
# Run tests against the musl build. Uses the release-test profile
# (Cargo.toml), not --release directly -- see that profile's comment: fat
# LTO + codegen-units=1 against test binaries, on top of the
# deltalake/arrow/parquet dependency graph, is enough peak memory during
# linking to OOM a Docker build.
RUN cargo test --profile release-test --locked --target "$(cat .target)"
# Fail the build if the binary isn't actually static.
RUN file /build/delta-txn-service | grep -Eq "static(ally|-pie) linked"


# ======================================================
# Runtime
# ======================================================
# Distroless "static": CA certificates, tzdata and /etc/passwd basics, and
# nothing else -- no libc, no libssl, no shell, no package manager. The
# binary is fully static and TLS is rustls, so nothing more is needed.
# debian:trixie-slim had ~220 OS-package CVEs (53 HIGH) from tools the
# service never runs; this base had none when it was adopted. No shell
# means `docker exec ... sh` won't work; use an ephemeral debug container
# (kubectl debug) instead.
# Pinned by digest for the same reason as the builder; without it this
# image (tag has no version) would never get Dependabot updates either.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:8ce6dd87e9823c5b48dcfb2c8604714ccb351ce59607327bcc4957448da279b5

COPY --from=builder /build/delta-txn-service /usr/local/bin/delta-txn-service

EXPOSE 50051

# AWS_REGION is only a default: delta-rs' S3 client needs some region even
# for self-hosted S3-compatible endpoints. Real AWS deployments should set their own.
ENV RUST_LOG=info \
    AWS_REGION=us-east-1

# Numeric, so Kubernetes can verify runAsNonRoot; matches the Helm chart's
# runAsUser/runAsGroup (10001) rather than distroless's own nonroot (65532).
# No passwd entry is needed for a numeric user.
USER 10001:10001

ENTRYPOINT ["/usr/local/bin/delta-txn-service"]
