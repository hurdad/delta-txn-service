# Delta Txn Service

[![Build](https://github.com/hurdad/delta-txn-service/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/hurdad/delta-txn-service/actions/workflows/docker-publish.yml)

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

✅ Opens Delta tables  
✅ Enforces optimistic concurrency (`expected_version`)  
✅ Applies ordered Delta actions (`AddFile`, `RemoveFile`, `Protocol`, `Metadata`)  
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
   Object Storage (S3 / MinIO / FS)
```

The read path (`GetTable`, `ListActiveFiles`) never blocks on or serializes
against the write path — it opens the table fresh and reads whatever
`_delta_log/*.json` snapshot is currently visible, taking no lock. See
"Concurrency model" below.

---

## gRPC API (summary)

### `GetTable`
Fetch table version, protocol, and metadata. No file listing — see
`ListActiveFiles` below for that. A plain read: takes no lock, reflects
whatever snapshot is visible at the moment the server opens the table.

### `Commit`
Atomically commit Delta actions.

- Optimistic concurrency via `expected_version`
- Fully typed protobuf actions (no JSON)

### `ListActiveFiles`
Server-streaming: every currently-active (not yet removed) data file for a
table's latest version — the read-side counterpart to `Commit`'s
`AddFile`/`RemoveFile` actions. Streamed rather than a single response
because a real table's active file set can exceed gRPC's default 4 MiB
message limit; the stream always starts with one header message (table
version/schema/protocol, the same info `GetTable` returns) followed by
zero or more batches of files. Like `GetTable`, a plain read — no lock,
no interaction with `Commit`'s optimistic-concurrency machinery.

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

Supported via `delta-rs`:
- Amazon S3
- MinIO
- Local filesystem (`file:///`)

Configuration is driven by environment variables (example for MinIO):

```bash
AWS_ENDPOINT_URL=http://minio:9000
AWS_ACCESS_KEY_ID=minioadmin
AWS_SECRET_ACCESS_KEY=minioadmin
AWS_REGION=us-east-1
AWS_ALLOW_HTTP=true
```

---

## Configuration (environment variables)

### gRPC
- `DELTA_TXN_GRPC_ADDR`: Bind address for the gRPC server (default: `0.0.0.0:50051`).
- `DELTA_TXN_GRPC_TLS_CERT`: Path to a PEM-encoded TLS certificate for gRPC.
- `DELTA_TXN_GRPC_TLS_KEY`: Path to a PEM-encoded TLS private key for gRPC.
- `DELTA_TXN_GRPC_API_KEY`: Optional API key for gRPC auth (clients send `x-api-key` or `authorization: Bearer ...`).
- `DELTA_TXN_ALLOWED_TABLE_PREFIXES`: Optional comma-separated list of `table_uri` prefixes. When set, `Commit`,
  `GetTable`, and `ListActiveFiles` all reject any `table_uri` that doesn't start with one of these prefixes. When
  unset (the default), a client may address any table URI the server's storage credentials can reach — set this in
  any deployment where the API key/network boundary isn't trusted to scope table access on its own.

### Storage (object-store)
- `AWS_*`: All `AWS_` environment variables are forwarded to `delta-rs` object-store configuration
  (e.g. `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
  `AWS_ALLOW_HTTP`).

### Telemetry (OpenTelemetry)
- `OTEL_SERVICE_NAME`: Service name used in traces/metrics (default: `delta-txn-service`).
- `OTEL_EXPORTER_OTLP_PROTOCOL`: Export protocol (`grpc`, `http/protobuf`, or `http/json`).
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
docker run -p 50051:50051 delta-txn-service
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
├── deploy/                # Helm / K8s / Compose
└── Dockerfile
```

---

## Concurrency model

- Delta Lake optimistic concurrency is always enforced
- Optional in-process per-table async locks reduce conflicts
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

- No JSON parsing in the hot path
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
- Edge / ARM64 environments

---

## License

Apache License 2.0

---

## Status

This project is **intentionally small and focused**.
