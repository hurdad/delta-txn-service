use thiserror::Error;

/// Every failure mode this service's own Delta-table logic (as opposed to
/// gRPC/transport-level failures, which use tonic::Status directly) can
/// produce. Each variant's `#[error(...)]` message is for *internal*
/// logging (see the `tracing::error!` call below) -- it's never sent to a
/// client as-is, since delta-rs's own underlying error strings can include
/// storage paths/internal details a client has no legitimate need to see.
#[derive(Error, Debug)]
pub enum DeltaTxnError {
    /// deltalake::open_table_with_storage_options() (table::open_table)
    /// failed -- bad table_uri, missing/unreachable object storage,
    /// malformed or unreadable log, etc. Wraps delta-rs's own
    /// DeltaTableError::to_string(), not something this service
    /// distinguishes further.
    #[error("Delta table open failed: {0}")]
    OpenFailed(String),

    /// CommitBuilder::build() failed -- delta-rs's own commit path already
    /// retries a losing optimistic-concurrency race internally (against a
    /// freshly re-read snapshot) up to its own retry limit; this variant
    /// covers what's left once that's exhausted, or any other commit-time
    /// failure (storage error, conflicting non-retryable actions, etc).
    #[error("Delta commit failed: {0}")]
    CommitFailed(String),

    /// This service's *own* pre-commit optimistic-concurrency check (see
    /// grpc::server::DeltaTxnGrpcServer::commit()): the caller's
    /// CommitRequest.expected_version didn't match the table's actual
    /// current version at the time of the check. Distinct from
    /// CommitFailed's delta-rs-internal conflict handling -- this one is
    /// this service rejecting the request before ever calling
    /// CommitBuilder at all.
    #[error("Version conflict: expected {expected}, found {actual}")]
    VersionConflict { expected: i64, actual: i64 },

    /// Another writer's commit won the race at the storage level (see
    /// delta::commit::map_commit_error). Logged in full; the client gets a
    /// generic ABORTED so it re-reads and retries.
    #[error("Concurrent commit conflict: {0}")]
    CommitConflict(String),

    /// A time-travel read (GetTable/ListActiveFiles `version` or
    /// `timestamp_ms`) asked for a version that doesn't exist: newer than
    /// the latest, older than the oldest the log still holds, or a
    /// timestamp before the table's first commit. The message is
    /// client-safe (it only echoes what the client asked for).
    #[error("{0}")]
    VersionNotFound(String),

    /// A SetTransaction in a Commit whose (app_id, version) the table has
    /// already recorded: the same or a later version for that app id is
    /// already committed, so this commit is a retry of one that already
    /// landed (or is out of order) and must not be applied again.
    #[error("app_id '{app_id}' already committed transaction version {committed}")]
    TransactionAlreadyCommitted { app_id: String, committed: i64 },
}

impl From<DeltaTxnError> for tonic::Status {
    fn from(err: DeltaTxnError) -> Self {
        match &err {
            // ABORTED (not FAILED_PRECONDITION or INVALID_ARGUMENT): gRPC's
            // own status-code guidance reserves ABORTED specifically for
            // "the client should retry at a higher level" conflicts like
            // this one -- the request itself was well-formed, it just lost
            // a race, and issuing it again (typically with a freshly
            // re-read expected_version) is the correct client-side
            // response. Safe to include expected/actual in the message:
            // both came from the client's own request and this service's
            // own version counter, no internal detail leaked.
            DeltaTxnError::VersionConflict { expected, actual } => tonic::Status::aborted(format!(
                "version conflict: expected {expected}, found {actual}"
            )),
            // OpenFailed/CommitFailed wrap delta-rs's own error strings,
            // which can include storage paths and other internal detail a
            // client has no legitimate need to see -- logged in full here
            // (server-side, operator-visible), but the client only gets a
            // generic message. This is the one place either of these
            // errors' real detail is observable at all, so losing it here
            // means losing it entirely -- log before converting, not after.
            DeltaTxnError::VersionNotFound(msg) => tonic::Status::not_found(msg.clone()),
            DeltaTxnError::CommitConflict(_) => {
                tracing::info!(error = %err, "commit lost a race to a concurrent writer");
                tonic::Status::aborted("concurrent commit conflict: re-read the table and retry")
            }
            // ALREADY_EXISTS: the work this commit describes is already in
            // the table. A client retrying after an unknown outcome should
            // treat this as success, not retry again.
            DeltaTxnError::TransactionAlreadyCommitted { .. } => {
                tonic::Status::already_exists(err.to_string())
            }
            DeltaTxnError::OpenFailed(_) | DeltaTxnError::CommitFailed(_) => {
                tracing::error!(error = %err, "internal delta error");
                tonic::Status::internal("internal error processing delta table")
            }
        }
    }
}
