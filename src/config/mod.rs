//! Process-startup configuration, read once from environment variables in
//! main() before the gRPC server starts listening. `grpc` covers the
//! server's own listen address/TLS/auth; `storage` covers the object-store
//! credentials handed to delta-rs per request and the optional
//! table_uri allowlist.

pub mod grpc;
pub mod storage;
