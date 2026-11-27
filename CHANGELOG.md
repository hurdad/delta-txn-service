# Changelog

## 0.2.0

### Upgrade notes (action may be needed)

- **The service refuses to start without access control.** Set an API key
  (`DELTA_TXN_GRPC_API_KEY` / `DELTA_TXN_GRPC_API_KEYS`) or a table allowlist
  (`DELTA_TXN_ALLOWED_TABLE_PREFIXES`), or `DELTA_TXN_ALLOW_INSECURE=true` for
  local development. The Helm chart likewise refuses to render without one.
- **Tables at writer versions 3–6 can no longer be written.** 0.1.1 built
  delta-rs with its `datafusion` feature, which let it commit to tables with
  check constraints, change data feed, generated columns or column mapping
  (writer versions 3–5, and the matching writer-version-7 features). 0.2.0
  drops that feature, so those tables can be read but not committed to.
  Writable: writer versions 1 and 2, and 7 with appendOnly, deletionVectors,
  timestampNtz, variantType(-preview), v2Checkpoint and checkpointProtection.
  Anything else is refused with `FAILED_PRECONDITION` at create, on a protocol
  upgrade, or on append. (0.1.1 accepted creating a table with any feature,
  even one it couldn't write, and then failed every append with `INTERNAL`.)
- **Table properties must match the protocol.** Properties that turn a feature
  on (`delta.enableChangeDataFeed`, `delta.columnMapping.mode`,
  `delta.enableDeletionVectors`, `delta.enableRowTracking`, ...) are refused
  unless the protocol has that feature.
- **DomainMetadata actions are refused**: they need the `domainMetadata`
  writer feature, which delta-rs can't write yet.
- **Status codes changed.** Storage-level commit races are `ABORTED` (were
  `INTERNAL`); unwritable tables and append-only violations are
  `FAILED_PRECONDITION` (were `INTERNAL`); a retried `SetTransaction` is
  `ALREADY_EXISTS`.
- **CommitInfo.timestamp 0** now records the server's current time (the log
  entry previously had none).
- **Container image**: a static binary on `distroless/static`, running as uid
  10001, with no shell. amd64 only for now.

### Added

- `GetHistory` RPC: commit history, newest first, one entry per version, paged.
- Time travel on `GetTable` / `ListActiveFiles` (`version`, `timestamp_ms`).
- Per-replica snapshot cache (`DELTA_TXN_SNAPSHOT_CACHE_SIZE`,
  `DELTA_TXN_SNAPSHOT_CACHE_TTL_SECS`): on S3, GetTable p50 drops from 12–196 ms
  to 6–10 ms depending on table size.
- Delta checkpoints every `delta.checkpointInterval` commits (none were ever
  written before, so loads replayed the whole log).
- Multiple named API keys for rotation, mutual TLS, request timeout,
  message-size and per-connection concurrency limits, max actions per commit.
- Server-side `SetTransaction` idempotency (`ALREADY_EXISTS` on a retry).
- Commit metrics and snapshot-cache metrics over OTLP; JSON logs.
- Helm: new settings, NetworkPolicy, spread constraints, `extraEnv`.

### Fixed

- Panic on the first TLS handshake (server TLS, and likely S3 over HTTPS and
  OTLP over TLS): no rustls crypto provider was installed.
- Column stats read back from the log could be one ULP off (now parsed exactly).
- A table whose `_delta_log` was deleted and recreated could be served stale.
- Several Delta correctness gaps in the action mapping.

### Internal

- deltalake 0.32.4 → 1.1.2 (deltalake-core 1.1.4), without the `datafusion`
  feature (455 normal dependencies, was 556). Rust 1.100.0, OpenTelemetry 0.33.
- Image went from 223 known CVEs to 0; base images pinned by digest; Trivy in CI.
- Property tests for the proto ↔ Delta mapping; S3 integration tests on
  SeaweedFS; line coverage about 93%.
