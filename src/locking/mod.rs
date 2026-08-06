//! Optional in-process per-table_uri commit locking (TableLockManager),
//! held for the duration of one Commit RPC's version-check-then-write
//! sequence (see grpc::server::DeltaTxnGrpcServer::commit()). Purely an
//! optimization: it reduces how often two concurrent commits to the same
//! table race each other into a storage-level conflict delta-rs's own
//! optimistic-concurrency retry has to resolve the hard way -- it is not
//! required for correctness (delta-rs's atomic conditional-put commit
//! protocol is safe without it), and read RPCs (GetTable,
//! ListActiveFiles) never take it at all, matching Delta's own "readers
//! never block on writers" model.

pub mod table_lock;
