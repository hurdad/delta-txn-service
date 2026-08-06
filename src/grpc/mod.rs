//! The gRPC-facing layer: `server` implements the DeltaTxnService trait
//! generated from proto/delta_txn.proto and owns the actual request
//! handling; `mapping` translates between the generated protobuf types and
//! delta-rs's own kernel::Action/Add/etc. types (both directions -- proto
//! to kernel for Commit, kernel to proto for ListActiveFiles); `auth`
//! provides the optional API-key request interceptor main.rs wires in.

pub mod auth;
pub mod mapping;
pub mod server;
