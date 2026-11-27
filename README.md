# Delta Txn Service

[![Build](https://github.com/hurdad/delta-txn-service/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/hurdad/delta-txn-service/actions/workflows/docker-publish.yml)
[![codecov](https://codecov.io/gh/hurdad/delta-txn-service/graph/badge.svg)](https://codecov.io/gh/hurdad/delta-txn-service)

A **high-performance Delta Lake transaction coordinator** implemented in **Rust + gRPC**, designed to provide **atomic, typed, non-Spark commits** to Delta Lake tables.

This service owns **Delta log commits only**.  
It does **not** write data files.

---

## Why this exists

Delta Lake commits are:
- metadata-heavy
- latency-sensitive
- correctness-critical

Most implementations route commits through:
- Spark drivers
- JVM services
- Python glue code

That adds **latency, memory overhead, and operational complexity**.

This project provides:
- a **native Delta commit path**
- **no JVM**
- **no Python**
- **strongly typed protobuf actions**
- **predictable latency under load**

---

## What this service does

✅ Opens Delta tables, and creates new ones on the first `Commit` to a table_uri that doesn't exist yet  
✅ Enforces optimistic concurrency (`expected_version`)  
✅ Applies ordered Delta actions (`AddFile`, `RemoveFile`, `Protocol`, `Metadata`, `CommitInfo`,
   `SetTransaction`, `AddCDCFile`, `DomainMetadata`), including deletion vectors and table features  
✅ Makes retries safe: a `Commit` carrying a `SetTransaction` that's already recorded is refused (`ALREADY_EXISTS`)  
✅ Time travel: `GetTable` / `ListActiveFiles` at a past version or timestamp  
✅ Commits atomically using `delta-rs`  
✅ Streams a table's currently-active file list (`ListActiveFiles`) for readers  
✅ Exposes a stable gRPC API

---

## What this service does *not* do

❌ Write Parquet files  
❌ Manage compute  
❌ Replace Spark  
❌ Perform query execution

Writers (Spark, Arrow C++, other native pipelines, etc.) are responsible for **data writes**.  
This service is responsible for **metadata correctness**.

---

## Architecture

```
Writer (Spark / Arrow / etc)         Reader (query engine / client)
        |                                     |
        |  gRPC Commit                        |  gRPC GetTable / ListActiveFiles
        v                                     v
  +------------------------------------------------+
  |               Delta Txn Service                |
  |                (Rust / tonic)                  |
  +------------------------------------------------+
        |                                     ^
        |  atomic commit                      |  version / schema /
        v                                     |  active file list
   _delta_log/*.json  ------------------------+
        |
        v
   Object Storage (S3 / S3-compatible / FS)
```

The read path (`GetTable`, `ListActiveFiles`) never blocks on or serializes
against the write path — it opens the table fresh and reads whatever
`_delta_log/*.json` snapshot is currently visible, taking no lock. See
"Concurrency model" below.

---

## gRPC API (summary)

### `GetTable`
Fetch table version, protocol (including table features), and metadata. No file listing — see
`ListActiveFiles` below for that. A plain read: takes no lock, reflects
whatever snapshot is visible at the moment the server opens the table.

- Time travel: set `version` or `timestamp_ms` (not both) to read an earlier state; a version
  that doesn't exist, or a timestamp before the table's first commit, is `NOT_FOUND`.
- Idempotent writers: set `app_id` to get `app_transaction_version`, the last `SetTransaction`
  version that application committed.

### `Commit`
Atomically commit Delta actions.

- Optimistic concurrency via `expected_version`
- Fully typed protobuf actions (no JSON on the wire)
- File paths in `AddFile` / `RemoveFile` / `AddCDCFile` must be relative to the table root: absolute
  paths, URIs and `..` segments are rejected (`INVALID_ARGUMENT`), so a writer can't point readers
  at files outside the table. Absolute-path (`"p"`) deletion vectors are rejected for the same reason.
- A `SetTransaction(app_id, version)` whose version the table already recorded for that `app_id`
  (same or newer) is refused with `ALREADY_EXISTS`; treat that as "already committed".
- If `table_uri` doesn't exist yet, this Commit creates it as the table's version-0 commit —
  `actions` must include both a `Protocol` and a `TableMetadata` action, and `expected_version`
  must be left unset. Every subsequent `Commit` to that `table_uri` behaves as an ordinary
  append/update against the now-existing table.

### `ListActiveFiles`
Server-streaming: every currently-active (not yet removed) data file for a
table's latest version — the read-side counterpart to `Commit`'s
`AddFile`/`RemoveFile` actions. Streamed rather than a single response
because a real table's active file set can exceed gRPC's default 4 MiB
message limit; the stream always starts with one header message (table
version/schema/protocol, the same info `GetTable` returns) followed by
zero or more batches of files. Like `GetTable`, a plain read — no lock,
no interaction with `Commit`'s optimistic-concurrency machinery. Supports the same `version` /
`timestamp_ms` time travel. Each file carries its deletion vector, if any: readers must apply it,
since the data file still contains the deleted rows.

---

## Protobuf

The service uses a **fully typed Delta commit schema**.

Key highlights:
- `Action` is a `oneof` (`AddFile`, `RemoveFile`, `Protocol`, `TableMetadata`, `CommitInfo`)
- `CommitOperation` is an enum (`WRITE`, `MERGE`, `OPTIMIZE`, etc.)
- `DataChange` is explicit (no ambiguous booleans)

See:
```
proto/delta_txn.proto
```

---

## Storage backends

Supported via [`delta-rs`](https://github.com/delta-io/delta-rs) — currently the
`deltalake` crate **1.1.2** (see `Cargo.toml`; keep this line in sync when bumping it):
- Amazon S3
- S3-compatible stores that support conditional writes (`If-None-Match` on PutObject), which
  delta-rs's S3 commit protocol relies on. Tested in CI against SeaweedFS.
- Local filesystem (`file:///`)

Configuration is driven by environment variables (example for a local SeaweedFS S3 gateway):

```bash
AWS_ENDPOINT_URL=http://seaweedfs:8333
AWS_ACCESS_KEY_ID=dev
AWS_SECRET_ACCESS_KEY=dev
AWS_REGION=us-east-1
AWS_ALLOW_HTTP=true
```

---

## Configuration (environment variables)

### gRPC
- `DELTA_TXN_GRPC_ADDR`: Bind address for the gRPC server (default: `0.0.0.0:50051`).
- `DELTA_TXN_GRPC_TLS_CERT`: Path to a PEM-encoded TLS certificate for gRPC.
- `DELTA_TXN_GRPC_TLS_KEY`: Path to a PEM-encoded TLS private key for gRPC.
- `DELTA_TXN_GRPC_TLS_CLIENT_CA`: Optional PEM CA bundle. When set (requires TLS), clients must present a
  certificate signed by it (mutual TLS).
- `DELTA_TXN_GRPC_API_KEY`: Optional API key for gRPC auth (clients send `x-api-key` or `authorization: Bearer ...`).
- `DELTA_TXN_GRPC_API_KEYS`: Optional comma-separated `name=key` list of accepted keys. The matched key's name is
  recorded as the `client` field on the request's trace span. To rotate a key, list the old and new key, move
  clients over, then remove the old one.
- `DELTA_TXN_ALLOWED_TABLE_PREFIXES`: Optional comma-separated list of `table_uri` prefixes. When set, `Commit`,
  `GetTable`, and `ListActiveFiles` all reject any `table_uri` that doesn't start with one of these prefixes. When
  unset (the default), a client may address any table URI the server's storage credentials can reach — set this in
  any deployment where the API key/network boundary isn't trusted to scope table access on its own.
- `DELTA_TXN_ALLOW_INSECURE`: The service refuses to start with neither an API key nor a table allowlist.
  Set this to `true` to allow that anyway (local development only).
- `DELTA_TXN_MAX_ACTIONS_PER_COMMIT`: Largest number of actions one `Commit` may carry (default `100000`).
- `DELTA_TXN_GRPC_MAX_MESSAGE_BYTES`: Largest request message accepted (default 4 MiB).
- `DELTA_TXN_GRPC_REQUEST_TIMEOUT_SECS`: Per-request timeout, including time waiting on the per-table
  commit lock (default `60`; `0` disables). Clients get `DEADLINE_EXCEEDED`.
- `DELTA_TXN_GRPC_MAX_CONCURRENT_REQUESTS_PER_CONNECTION`: Concurrent requests per client connection
  (default `64`).

### Storage (object-store)
- `AWS_*`: All `AWS_` environment variables are forwarded to `delta-rs` object-store configuration
  (e.g. `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
  `AWS_ALLOW_HTTP`).

### Logging
- `RUST_LOG`: Log filter (default `info`).
- `DELTA_TXN_LOG_FORMAT`: `json` for one JSON object per line (for log aggregators); default is plain text.

### Telemetry (OpenTelemetry)
- `OTEL_SERVICE_NAME`: Service name used in traces/metrics (default: `delta-txn-service`).
- OTLP export is gRPC only: `OTEL_EXPORTER_OTLP_PROTOCOL` is not honored, and a non-`grpc` value logs a
  startup warning. Point the endpoint at the collector's gRPC port (usually 4317).
- `OTEL_EXPORTER_OTLP_ENDPOINT`: Shared OTLP endpoint for traces + metrics.
- `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: Overrides traces endpoint.
- `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`: Overrides metrics endpoint.

---

## Running locally

### Build
```bash
cargo build --release
```

### Run
```bash
DELTA_TXN_GRPC_ADDR=0.0.0.0:50051 ./target/release/delta-txn-service
```
Set `DELTA_TXN_GRPC_ADDR` to change the gRPC bind address (default `0.0.0.0:50051`).

#### gRPC TLS

Provide both certificate and key files to enable TLS:

```bash
DELTA_TXN_GRPC_TLS_CERT=/path/to/server.crt
DELTA_TXN_GRPC_TLS_KEY=/path/to/server.key
```

#### gRPC API key auth

Set an API key to require authentication for all gRPC calls. Clients must send it as an
`x-api-key` metadata header or as a `Bearer` token in the `authorization` header.

```bash
DELTA_TXN_GRPC_API_KEY=super-secret
```

### Docker
```bash
docker build -t delta-txn-service .
# Local try-out with no auth; see "Security" before running it anywhere else.
docker run -p 50051:50051 -e DELTA_TXN_ALLOW_INSECURE=true delta-txn-service
```

---

## Metrics

When OTLP export is enabled (set any `OTEL_EXPORTER_OTLP_*` endpoint), the service emits
gRPC server metrics via OpenTelemetry:

- `grpc.server.requests` (counter): total gRPC requests received.
- `grpc.server.errors` (counter): total gRPC requests that returned non-OK status.
- `grpc.server.latency_ms` (histogram, unit `ms`): end-to-end gRPC handler latency.

All metrics include standard RPC attributes:
`rpc.system`, `rpc.service`, `rpc.method`, and `rpc.grpc.status_code`.

Commit-specific metrics, recorded by the `Commit` handler itself:

- `delta_txn.commits` (counter, attribute `outcome`): `created`, `committed`, `version_conflict`,
  `already_committed`, `rejected` or `error`. A rising `version_conflict` rate means writers are racing.
- `delta_txn.commit.lock_wait_ms` (histogram): time waiting for the table's in-process commit lock.
- `delta_txn.commit.duration_ms` (histogram, attribute `outcome`): end-to-end `Commit` time.

Metric attributes are kept low-cardinality on purpose: `table_uri` is never a label (it's on the trace span).

`grpc.server.errors` correctly counts every non-OK request, including application-level
failures like "table not found" or a `ListActiveFiles` stream that fails partway through
— not just requests rejected before a handler runs (e.g. a missing/invalid API key). gRPC's
real status for those in-handler cases is carried in HTTP/2 trailers, sent only after the
response body; the metrics middleware observes the response body itself to catch this case
(see `telemetry/metrics.rs`'s own doc comment for the mechanics).

---

## Tracing

Independent of the metrics above: when OTLP export is enabled, every request gets a real
`grpc.request` span (`rpc.system`/`rpc.service`/`rpc.method` attributes), parented to an
incoming [W3C `traceparent`](https://www.w3.org/TR/trace-context/) header when the caller
sends one — so a caller that also participates in W3C trace-context propagation gets a
genuinely correlated, cross-process trace rather than two disconnected trace trees. Every
`tracing::info!`/`warn!`/`error!` call anywhere under a request's handling runs inside that
request's span.

---

## Security

What the service enforces:

- **Authentication**: optional API keys (`DELTA_TXN_GRPC_API_KEY` / `DELTA_TXN_GRPC_API_KEYS`), compared in
  constant time, and optional mutual TLS (`DELTA_TXN_GRPC_TLS_CLIENT_CA`). The health service is always
  unauthenticated so orchestrator probes work.
- **Table scoping**: `DELTA_TXN_ALLOWED_TABLE_PREFIXES` limits which `table_uri`s any caller can use, and file
  paths inside actions must stay under the table root.
- **Fail closed**: with neither API keys nor an allowlist the service won't start unless
  `DELTA_TXN_ALLOW_INSECURE=true`. API keys without TLS log a startup warning.
- **Limits**: per-request timeout, per-connection concurrency, maximum message size and maximum actions per
  commit (see Configuration).

Limits of the model:

- Every valid key has the same access: there are no per-client permissions (read-only clients, or a key
  limited to its own tables). Key names identify callers in traces, but don't restrict them.
- A `Bearer` token is compared as a plain string; it isn't validated as a JWT/OIDC token.

For production: always use TLS, set an allowlist, and give each client its own named key.

## Client retry guidance

| Status | Meaning | What to do |
|---|---|---|
| `ABORTED` | `expected_version` didn't match | Re-read with `GetTable`, rebuild the commit, retry |
| `ALREADY_EXISTS` | The commit's `SetTransaction` version is already recorded | Treat as success; don't retry |
| `DEADLINE_EXCEEDED`, `UNAVAILABLE` | Outcome unknown: the commit may or may not have landed | Retry only if the commit carries a `SetTransaction` (then a duplicate is refused); otherwise check `GetTable` / `ListActiveFiles` first |
| `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `PERMISSION_DENIED`, `UNAUTHENTICATED` | Request is wrong | Fix the request; retrying won't help |

Writers that retry should always include a `SetTransaction` with a monotonically increasing version per
`app_id`, which makes retries safe.

## Repository layout

```
delta-txn-service/
├── proto/                 # gRPC + Delta action schema
├── src/
│   ├── grpc/              # tonic service + mappings
│   ├── delta/             # Delta table + commit logic
│   ├── locking/           # per-table commit locks
│   ├── config/            # storage + gRPC server config
│   └── telemetry/         # tracing setup, request tracing/metrics middleware
├── tests/                 # gRPC end-to-end suite (see tests/README.md)
├── deploy/                # Helm / K8s / Compose
└── Dockerfile
```

---

## Concurrency model

- Delta Lake optimistic concurrency is always enforced
- In-process per-table async locks (always on) reduce conflicts within a replica —
  an optimization, not a correctness requirement
- Safe to run multiple replicas (stateless)

The internal Delta operation type used for the server's own conflict-detection
isolation-level choice is derived from the client's `CommitInfo.operation`
(`WRITE`/`MERGE`/`UPDATE`/`DELETE`/`OPTIMIZE`/`VACUUM`/`RESTORE`), so non-data-changing
operations like `OPTIMIZE`/`VACUUM` get the isolation-level downgrade delta-rs allows for
them rather than always being treated as a data-changing `Write`. See `delta/commit.rs`'s
own doc comment for residual imprecision (e.g. `Merge`'s per-clause predicates aren't
represented on the wire today).

---

## Performance characteristics

- No JSON on the wire (protobuf in, Delta log JSON written only by delta-rs)
- No GIL
- No JVM
- Low RSS
- Flat p99 latency under commit bursts

This is **significantly faster and more predictable** than Python-based coordinators.

---

## Intended use cases

- Centralized Delta commit coordinator
- Arrow / C++ data pipelines
- Lightweight Delta metadata services
- Edge / ARM64 environments (build from source — published images are amd64-only for now)

---

## License

Apache License 2.0

---

## Status

This project is **intentionally small and focused**.
