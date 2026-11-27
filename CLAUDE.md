# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A native Rust + gRPC transaction coordinator for Delta Lake: it owns `_delta_log` commits only and never writes data files itself. Writers (Spark, Arrow C++, other native pipelines) handle data; this service handles commit atomicity, optimistic concurrency, and table metadata. See `README.md` for the full pitch and `proto/delta_txn.proto` for the wire contract (the single source of truth for the API — read that file's own comments before changing request/response shapes).

## Commands

```bash
cargo build --release          # build
cargo test                     # unit tests (src/) + integration tests (tests/), file://-backed
cargo test --test e2e_s3       # the one integration suite gated on a real S3 endpoint (SeaweedFS in CI, see below); a no-op skip without it
cargo test <substring>         # run tests whose name contains <substring>, across all binaries
cargo run                      # run locally; needs DELTA_TXN_GRPC_ADDR etc., see README's Configuration section

docker build -t delta-txn-service .                              # full multi-stage build (also runs cargo test --profile release-test)
docker compose -f deploy/docker-compose.yaml up -d --build        # local dev stack: service + SeaweedFS
```

Formatting is enforced: `rustfmt.toml` pins `edition`, and `.github/workflows/ci.yml`'s `fmt` job runs `cargo fmt --all -- --check` on every PR — run `cargo fmt --all` before committing. CI also runs `cargo clippy --all-targets --locked -- -D warnings` (`clippy` job), `cargo audit` (`audit` job; justified ignores live in `.cargo/audit.toml`, each with its removal condition), and `helm lint`/`helm template` on the chart (`helm` job, which also asserts the default no-auth render is refused).

**Docker build must include `tests/`.** The Dockerfile's builder stage explicitly `COPY`s `build.rs`, `proto/`, `src/`, and `tests/` before running `cargo build`/`cargo test` — if you ever restructure the Dockerfile, dropping the `tests/` copy makes `cargo test --profile release-test` silently run zero integration tests (no error, no output about them, `tests/` just doesn't exist in that build context). This already happened once; it's why that `COPY` line has a comment on it.

## Architecture

### Request flow

A request enters through `grpc::server::DeltaTxnGrpcServer` (implements the tonic-generated `DeltaTxnService` trait from `proto/delta_txn.proto`, compiled via `build.rs` into `grpc::server::pb`). Three RPCs, two very different concurrency models:

- **`Commit`**: takes `self.locks.lock_for(table_uri)` (an in-process, per-table_uri async lock — see `locking::table_lock`'s own doc comment: an optimization that reduces conflicts within one replica, *not* a correctness requirement, since delta-rs's own atomic conditional-put commit protocol is safe without it) before doing anything else. Inside the lock: check whether the table exists (`delta::table::table_exists`), then either create it (`delta::commit::create_table`, if `actions` carry `Protocol` + `TableMetadata` and no `expected_version`) or append to it (`delta::commit::commit_actions`, with the `expected_version` optimistic-concurrency check). Validation of `actions` themselves happens *before* the lock is taken, so a malformed request fails fast without paying for I/O or holding the lock.
- **`GetTable`/`ListActiveFiles`**: no lock at all — Delta readers never block on writers. Both open the table fresh and reflect whatever snapshot is visible at that moment.

`grpc::mapping` is the bidirectional translation layer between the wire protobuf types (`pb::*`) and delta-rs's own `kernel::Action`/`Add`/`Remove`/`Protocol`/`Metadata`/`CommitInfo` types — proto→kernel for `Commit`, kernel→proto for `ListActiveFiles`. Several kernel types are built by constructing a `serde_json::Value` in Delta's own log JSON shape and deserializing through delta-rs's existing `Deserialize` impls, rather than field-by-field struct literals — reuses delta-rs's own parsing/validation.

### `table_exists` / `create_table` split — why it exists

delta-rs's `open_table_with_storage_options` (what `delta::table::open_table` wraps) fails outright — `DeltaTableError::NotATable` — for a `table_uri` with *no* existing `_delta_log` at all; that's indistinguishable at that point from a genuine storage/permission failure. `delta::table::table_exists` checks first, cheaply (`DeltaTableBuilder::build()` + `LogStore::is_delta_table_location()`, no full load), so every handler can treat "doesn't exist yet" as its own clean case: `Status::not_found` for the read RPCs, or the trigger for `commit()`'s create-a-new-table path (`delta::commit::create_table`, which commits with `table_data: None` since there's nothing yet to conflict with, and `DeltaOperation::Create` instead of whatever `build_operation()` would otherwise infer).

### `table_uri` allowlist check happens twice per handler

Once against the *raw* client-supplied string, once against the *normalized* one (`ensure_table_uri`'s output). This isn't redundancy — `ensure_table_uri` creates a local directory as a side effect for `file://`/bare-local-path URIs, unconditionally, *before* any authorization check would otherwise run. The raw-string check is what stops a disallowed `table_uri` from causing that side effect at all; the normalized check afterward remains the actual source of truth (normalization can change which configured prefix a URI matches).

### Layer/interceptor order (`app.rs`)

`main.rs` is only the process shell (install the rustls crypto provider, init tracing, load env config, `check_security`, bind, wait for SIGTERM/Ctrl+C). Everything between config and shutdown is `app::serve`, which the integration test harness calls too, so tests run on the production wiring. `app::install_crypto_provider` must stay: the dependency graph enables both rustls backends (ring and aws-lc-rs), and without an explicit process default rustls panics on the first TLS handshake.

`TraceContextLayer` (outermost — establishes the `grpc.request` span, parented to an incoming W3C `traceparent` if present) → `GrpcMetricsLayer` (records `grpc.server.requests`/`errors`/`latency_ms`, observing HTTP/2 trailers for the real status since it doesn't arrive in initial headers) → tonic's own per-service `make_auth_interceptor` (API-key check; runs *after* the two tower layers, so a rejected request still gets a trace span and shows up in metrics rather than being invisible). The `grpc.health.v1.Health` service is added *outside* the auth interceptor entirely — an orchestrator's liveness/readiness probe has no practical way to supply an API key.

### Health status: liveness vs. readiness are deliberately different signals

The overall-server (`""`) status only ever reflects "process is alive and serving gRPC" — the Helm chart's *liveness* probe checks this, unconditionally `SERVING` from startup. The `delta.txn.v1.DeltaTxnService`-specific status is what the *readiness* probe checks, and — only when `AWS_ENDPOINT_URL` is configured (the self-hosted S3-compatible deployment shape, where there's one fixed endpoint to probe; real AWS S3 has no such single fixed thing to check) — `app::spawn_storage_health_probe` periodically reflects real reachability into it. Storage being down should pull the pod out of Service endpoints (readiness), not restart it (liveness would do that, and restarting doesn't fix a downstream outage).

### Config loading

`config::grpc`/`config::storage` read everything from environment variables once at startup (see root `README.md`'s "Configuration" section for the full documented list). `DeltaTxnGrpcServer::new()` does that env read; `DeltaTxnGrpcServer::with_config(storage_opts, allowed_table_prefixes)` takes already-resolved config directly and is what the integration test suite uses instead — mutating process-global env vars per-test is fragile under Rust's default parallel test execution.

### Testing

Unit tests are colocated (`#[cfg(test)] mod tests` in the same file as the code they test) throughout `src/` — pure functions and in-memory `kernel::Action` values only, no real storage I/O. The integration suite lives in `tests/` (see `tests/README.md`): `tests/common/mod.rs` spins up a real `DeltaTxnGrpcServer` via `app::serve` on an ephemeral localhost port with a real generated gRPC client (`build.rs` generates both server *and* client stubs — the client exists specifically for this suite, and for any downstream Rust consumer of this crate as a library). `tests/e2e_binary.rs` runs the compiled executable itself (env config, startup failure, logging/OTLP init, SIGTERM); cargo-llvm-cov picks up coverage from that child process, which is why it passes `LLVM_PROFILE_FILE` through its otherwise-cleared environment. Line coverage is about 93% and `codecov.yml` fails the Codecov status below 90%. Every test file except `tests/e2e_s3.rs` uses a `file://` tempdir backend (no external dependency, runs anywhere `cargo test` does). `tests/e2e_s3.rs` runs the same kind of coverage against a real S3-compatible backend (SeaweedFS in CI; MinIO was dropped as its community edition is no longer maintained) specifically because `object_store`'s backends don't share identical semantics — conditional-put behavior in particular, which is exactly what this service's optimistic-concurrency conflict detection depends on; it silently no-ops unless `AWS_ENDPOINT_URL`/`DELTA_TXN_TEST_S3_BUCKET` are set, which only `.github/workflows/ci.yml`'s `s3-integration-tests` job does. Any S3-compatible backend used here must support conditional writes (`If-None-Match` on PutObject). It includes a two-replica race test (two servers, separate in-process locks), the only test where storage-level conflicts actually occur; delta-rs reports those as `VersionAlreadyExists`/`CommitConflict`/`MaxCommitAttempts`, which `delta::commit::map_commit_error` turns into `ABORTED`.

### Deployment

`deploy/docker-compose.yaml` — local dev, service + SeaweedFS, not production-safe (no S3 auth, no TLS, `DELTA_TXN_ALLOW_INSECURE=true`). `deploy/helm/delta-txn-service/` — the production path; see `deploy/README.md` and `values.yaml`'s own inline comments (most values map 1:1 onto the env vars in the root README's Configuration section).
