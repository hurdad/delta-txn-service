# proto/delta_txn.proto

The single source of truth for `DeltaTxnService`'s wire API — every
language's client and this Rust server itself are generated from (or, for
Rust specifically, compiled directly from) this one file. Change the wire
contract here first; the Rust server/client bindings then follow from a
rebuild (see `build.rs` at the repo root).

## Service

`DeltaTxnService` exposes three RPCs:

- **`GetTable`** — unary. Returns a table's current version, schema, and
  protocol. No file listing (see `ListActiveFiles`).
- **`Commit`** — unary. Atomically applies a list of typed `Action`s to a
  table, with optional `expected_version`-based optimistic concurrency. If
  `table_uri` doesn't exist yet, this is also how a table gets created —
  see `CommitRequest.table_uri`'s own comment for the exact requirement
  (`Protocol` + `TableMetadata` actions, no `expected_version`).
- **`ListActiveFiles`** — server-streaming. Every currently-active data
  file for a table's latest version, the read-side counterpart to
  `Commit`'s `AddFile`/`RemoveFile` actions. Streamed (a header message
  followed by one or more batches) rather than unary, since a real table's
  file set can exceed gRPC's default 4 MiB message limit.

## Actions

`Action` is a `oneof` mirroring [Delta Lake's own log action
shapes](https://github.com/delta-io/delta/blob/master/PROTOCOL.md):
`AddFile`, `RemoveFile`, `Protocol`, `TableMetadata`, `CommitInfo`. See
each message's own field-level comments for the exact semantics and any
asymmetry between the write path (`Commit`) and read path
(`ListActiveFiles`) — e.g. `AddFile.data_change`/`.tags` are write-only
concepts and always report fixed values on a read.

## Regenerating bindings

- **Rust**: automatic — `build.rs` (repo root) recompiles this file into
  `delta_txn_service::grpc::server::pb` on every `cargo build`, tracked by
  Cargo as a `build.rs` dependency (both the server and, since this
  crate's own integration test suite under `tests/` needs one, the client
  stubs are generated).
- **Other languages**: run that language's own `protoc`/gRPC codegen
  against this file directly — this repo doesn't vendor or publish
  pre-generated bindings for anything but Rust.
