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
`RUN cargo test --release` step, with no S3/MinIO dependency.

## Layout

- **`common/mod.rs`** — shared harness. `TestServer::start(..)` binds an
  OS-assigned ephemeral localhost port, runs a real server on it (mirroring
  main.rs's own service construction, including the `grpc.health.v1.Health`
  service), and hands out `file://` table URIs under its own tempdir.
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
- **`e2e_concurrency.rs`** — many concurrent writers against the same
  table using the documented read-then-commit-with-`expected_version`
  retry pattern, verifying `TableLockManager` and delta-rs's own
  optimistic concurrency produce no lost updates.

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
