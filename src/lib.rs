//! delta-txn-service: a native Delta Lake transaction coordinator exposed
//! over gRPC (see proto/delta_txn.proto and README.md for the full
//! picture). `delta` wraps delta-rs (opening tables, committing actions);
//! `grpc` is the DeltaTxnService implementation and its protobuf<->kernel
//! type mapping; `locking` provides optional per-table_uri commit
//! serialization; `config`/`telemetry` are startup configuration and
//! observability. main.rs (not part of this library crate) wires all of
//! it into a running server.

pub mod config;
pub mod delta;
pub mod grpc;
pub mod locking;
pub mod telemetry;
