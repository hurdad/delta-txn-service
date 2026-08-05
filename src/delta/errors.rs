use thiserror::Error;

#[derive(Error, Debug)]
pub enum DeltaTxnError {
    #[error("Delta table open failed: {0}")]
    OpenFailed(String),

    #[error("Delta commit failed: {0}")]
    CommitFailed(String),

    #[error("Version conflict: expected {expected}, found {actual}")]
    VersionConflict { expected: i64, actual: i64 },
}

impl From<DeltaTxnError> for tonic::Status {
    fn from(err: DeltaTxnError) -> Self {
        match &err {
            DeltaTxnError::VersionConflict { expected, actual } => tonic::Status::aborted(
                format!("version conflict: expected {expected}, found {actual}"),
            ),
            DeltaTxnError::OpenFailed(_) | DeltaTxnError::CommitFailed(_) => {
                tracing::error!(error = %err, "internal delta error");
                tonic::Status::internal("internal error processing delta table")
            }
        }
    }
}
