# Integration tests

Everything under this directory is the **gRPC end-to-end test suite** —
distinct from the unit tests embedded (`#[cfg(test)] mod tests { ... }`)
throughout `src/`, which check individual functions in isolation. These
tests instead spin up a real `DeltaTxnGrpcServer`, connect a real generated
gRPC client to it, and drive it exactly the way an external caller would.

## Running

```bash
cargo test
```

Both suites (unit + integration) run together; Cargo treats every file
directly under `tests/` as its own test binary. No external services are
required — each test gets its own throwaway `file://` Delta table under a
fresh `tempdir`, so this runs anywhere, including inside the Docker build's
`RUN cargo test --profile release-test` step, with no S3 dependency.
The one exception is `e2e_s3.rs` (below), which skips itself unless
pointed at a real S3-compatible backend.

## Layout

- **`common/mod.rs`** — shared harness. `TestServer::start(..)` binds an
  OS-assigned ephemeral localhost port, runs the server on it through
  `delta_txn_service::app::serve` (the same function main.rs calls, so
  layers, auth, health, TLS and limits are the production wiring), and
  hands out `file://` table URIs under its own tempdir.
  Not itself a test target — see Cargo's own convention that only files
  *directly* under `tests/` are compiled as separate test binaries; a
  `mod.rs` in a subdirectory is just a shared module each test file pulls
  in via `mod common;`.
- **`e2e_table_lifecycle.rs`** — the golden path: `Commit` creating a new
  table, `GetTable` reading it back, appending/removing files, and
  `ListActiveFiles` streaming the active-file set (including the
  multi-batch case beyond `FILE_BATCH_SIZE`).
- **`e2e_validation.rs`** — every deliberate rejection this service
  performs (missing table, malformed create request, invalid actions),
  checked against the actual gRPC status code returned, not just the
  mapping logic that produces it.
- **`e2e_security.rs`** — API-key auth and the `table_uri` allowlist,
  wired end to end.
- **`e2e_health.rs`** — the `grpc.health.v1.Health` service.
- **`e2e_server.rs`** — server-level behavior of `app::serve`: TLS and
  mutual TLS (certificates generated at test time with `rcgen`), startup
  failure on missing TLS files, graceful shutdown, the storage readiness
  probe, the request-size limit, and an incoming `traceparent`.
- **`e2e_binary.rs`** — runs the real `delta-txn-service` executable with
  a clean environment: env-var config, the fail-closed startup check,
  JSON logs and OTLP initialization, and SIGTERM graceful shutdown.
  cargo-llvm-cov collects coverage from the child process too.
- **`e2e_concurrency.rs`** — many concurrent writers against the same
  table using the documented read-then-commit-with-`expected_version`
  retry pattern, verifying `TableLockManager` and delta-rs's own
  optimistic concurrency produce no lost updates.
- **`e2e_delta_features.rs`** — time travel on the read RPCs,
  `SetTransaction` idempotency (`ALREADY_EXISTS` on a retried txn),
  protocol table features and deletion-vector pass-through, and
  rejection of file paths that escape the table root, and partition
  values round-tripping through `ListActiveFiles`.
- **`e2e_snapshot_cache.rs`** — the per-replica snapshot cache
  (`delta::cache`): cached reads and commits stay current with this
  server's commits and with another replica's; a deleted-and-recreated
  `_delta_log` is detected and reloaded; TTL expiry, LRU eviction,
  disabling, and time travel bypassing the cache. Every other test file
  also runs through the cache, since `tests/common` enables it.
- **`e2e_s3.rs`** — the same kind of coverage against a real
  S3-compatible backend (SeaweedFS in CI), since `object_store`'s S3 and
  local filesystem backends differ in conditional-put semantics — exactly
  what conflict detection depends on. Includes two server replicas racing
  on one table, which only the backend's conditional put can arbitrate.
  A no-op unless `AWS_ENDPOINT_URL` and `DELTA_TXN_TEST_S3_BUCKET` are
  set; CI's `s3-integration-tests` job sets them. To run it locally:

  ```bash
  docker run -d --name seaweedfs -p 8333:8333 chrislusf/seaweedfs:4.55 server -s3 -dir=/data
  AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test \
    aws --endpoint-url http://localhost:8333 --region us-east-1 s3 mb s3://delta-txn-service-ci
  AWS_ENDPOINT_URL=http://localhost:8333 AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test \
    AWS_REGION=us-east-1 AWS_ALLOW_HTTP=true DELTA_TXN_TEST_S3_BUCKET=delta-txn-service-ci \
    cargo test --test e2e_s3
  ```

## Why this suite exists

Unit tests in this crate all operate on pure functions and in-memory
`kernel::Action`/etc. values — none of them ever call
`open_table_with_storage_options` against a real, possibly-nonexistent
storage location. That gap is exactly what let a real bug ship
unnoticed: `Commit` could not actually create a brand-new Delta table
(`open_table()` failed outright for a `table_uri` with no existing
`_delta_log`), despite the API being designed to support it. This suite
is what would have caught that — and is the regression barrier against it
recurring.
