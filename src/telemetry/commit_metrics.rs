//! Commit-specific metrics: what an operator of a commit coordinator
//! actually watches (conflicts, lock contention, commit latency), beyond
//! the generic per-RPC counters in `metrics`. Recorded by
//! grpc::server::DeltaTxnGrpcServer::commit().
//!
//! Attributes are deliberately low-cardinality: `outcome` only, never the
//! table_uri (that belongs on spans, not metric labels).

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::KeyValue;
use tonic::Code;

/// How one Commit call ended -- the `outcome` attribute value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Created a new table (version 0).
    Created,
    /// Appended to an existing table.
    Committed,
    /// `expected_version` didn't match (ABORTED).
    VersionConflict,
    /// A SetTransaction version was already recorded (ALREADY_EXISTS).
    AlreadyCommitted,
    /// The request was invalid or not allowed.
    Rejected,
    /// The request timed out or storage/delta-rs failed.
    Error,
}

impl CommitOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Committed => "committed",
            Self::VersionConflict => "version_conflict",
            Self::AlreadyCommitted => "already_committed",
            Self::Rejected => "rejected",
            Self::Error => "error",
        }
    }

    /// Classifies a failed Commit by its gRPC status code.
    pub fn from_code(code: Code) -> Self {
        match code {
            Code::Aborted => Self::VersionConflict,
            Code::AlreadyExists => Self::AlreadyCommitted,
            Code::InvalidArgument
            | Code::FailedPrecondition
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::NotFound => Self::Rejected,
            _ => Self::Error,
        }
    }
}

#[derive(Clone)]
pub struct CommitMetrics {
    commits: Counter<u64>,
    lock_wait_ms: Histogram<f64>,
    duration_ms: Histogram<f64>,
}

impl CommitMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            commits: meter
                .u64_counter("delta_txn.commits")
                .with_description("Commit calls, by outcome.")
                .build(),
            lock_wait_ms: meter
                .f64_histogram("delta_txn.commit.lock_wait_ms")
                .with_description("Time a Commit waited for its table's in-process lock.")
                .with_unit("ms")
                .build(),
            duration_ms: meter
                .f64_histogram("delta_txn.commit.duration_ms")
                .with_description("End-to-end Commit handler time, by outcome.")
                .with_unit("ms")
                .build(),
        }
    }

    pub fn record_lock_wait(&self, ms: f64) {
        self.lock_wait_ms.record(ms, &[]);
    }

    pub fn record_commit(&self, outcome: CommitOutcome, duration_ms: f64) {
        let attrs = [KeyValue::new("outcome", outcome.as_str())];
        self.commits.add(1, &attrs);
        self.duration_ms.record(duration_ms, &attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_are_stable() {
        let all = [
            (CommitOutcome::Created, "created"),
            (CommitOutcome::Committed, "committed"),
            (CommitOutcome::VersionConflict, "version_conflict"),
            (CommitOutcome::AlreadyCommitted, "already_committed"),
            (CommitOutcome::Rejected, "rejected"),
            (CommitOutcome::Error, "error"),
        ];
        for (outcome, label) in all {
            assert_eq!(outcome.as_str(), label);
        }
    }

    #[test]
    fn outcome_from_status_code() {
        assert_eq!(
            CommitOutcome::from_code(Code::Aborted),
            CommitOutcome::VersionConflict
        );
        assert_eq!(
            CommitOutcome::from_code(Code::AlreadyExists),
            CommitOutcome::AlreadyCommitted
        );
        for code in [
            Code::InvalidArgument,
            Code::FailedPrecondition,
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::NotFound,
        ] {
            assert_eq!(CommitOutcome::from_code(code), CommitOutcome::Rejected);
        }
        for code in [Code::Internal, Code::DeadlineExceeded, Code::Unavailable] {
            assert_eq!(CommitOutcome::from_code(code), CommitOutcome::Error);
        }
    }

    #[test]
    fn recording_with_a_noop_meter_does_not_panic() {
        let meter = opentelemetry::global::meter("test");
        let metrics = CommitMetrics::new(&meter);
        metrics.record_lock_wait(1.5);
        metrics.record_commit(CommitOutcome::Committed, 3.0);
    }
}
