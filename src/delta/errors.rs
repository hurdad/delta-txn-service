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
