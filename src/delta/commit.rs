use super::errors::DeltaTxnError;
use deltalake::kernel::transaction::{CommitBuilder, TableReference};
use deltalake::kernel::Action;
use deltalake::protocol::{DeltaOperation, SaveMode};

pub async fn commit_actions(
    table: deltalake::DeltaTable,
    actions: Vec<Action>,
) -> Result<i64, DeltaTxnError> {
    let table_state = table
        .snapshot()
        .map_err(|e| DeltaTxnError::CommitFailed(e.to_string()))?;
    let operation = DeltaOperation::Write {
        mode: SaveMode::Append,
        partition_by: None,
        predicate: None,
    };

    let result = CommitBuilder::default()
        .with_actions(actions)
        .build(
            Some(table_state as &dyn TableReference),
            table.log_store(),
            operation,
        )
        .await
        .map_err(|e| DeltaTxnError::CommitFailed(e.to_string()))?;

    Ok(result.version() as i64)
}
