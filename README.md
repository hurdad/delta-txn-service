# Delta Txn Service

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
✅ Exposes a stable gRPC API

---

## What this service does *not* do

❌ Write Parquet files  
❌ Manage compute  
❌ Replace Spark  
❌ Perform query execution

Writers (Spark, Flow-Pipe, Arrow C++, etc.) are responsible for **data writes**.  
This service is responsible for **metadata correctness**.

---

## Architecture

```
Writer (Spark / Arrow / Flow-Pipe)
        |
        |  gRPC (CommitRequest)
        v
+----------------------+
|  Delta Txn Service   |
|  (Rust / tonic)     |
+----------------------+
        |
        |  atomic commit
        v
   _delta_log/*.json
        |
        v
   Object Storage (S3 / MinIO / FS)
```

---

## gRPC API (summary)

### `Commit`
Atomically commit Delta actions.

- Optimistic concurrency via `expected_version`
- Fully typed protobuf actions (no JSON)

### `GetTable`
Fetch table version, protocol, and metadata.

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

## Running locally

### Build
```bash
cargo build --release
```

### Run
```bash
./target/release/delta-txn-service
```

### Docker
```bash
docker build -t delta-txn-service .
docker run -p 50051:50051 delta-txn-service
```

---

## Repository layout

```
delta-txn-service/
├── proto/                 # gRPC + Delta action schema
├── src/
│   ├── grpc/              # tonic service + mappings
│   ├── delta/             # Delta table + commit logic
│   ├── locking/           # per-table commit locks
│   ├── config/            # storage config
│   └── telemetry/         # tracing
├── deploy/                # Helm / K8s / Compose
└── Dockerfile
```

---

## Concurrency model

- Delta Lake optimistic concurrency is always enforced
- Optional in-process per-table async locks reduce conflicts
- Safe to run multiple replicas (stateless)

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
- Flow-Pipe style DAG runtimes
- Lightweight Delta metadata services
- Edge / ARM64 environments

---

## Roadmap (non-binding)

- Idempotent `TxnId`
- Commit batching / coalescing
- Leader election / fencing
- Arrow schema protobuf
- Arrow Flight data ingestion
- OpenTelemetry metrics export

---

## License

Apache License 2.0

---

## Status

This project is **intentionally small and focused**.

I