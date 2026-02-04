use deltalake::action::Action;
use deltalake::operations::transaction::CommitBuilder;
use deltalake::DeltaOps;

use super::errors::DeltaTxnError;

pub async fn commit_actions(
    table: deltalake::DeltaTable,
    actions: Vec<Action>,
) -> Result<i64, DeltaTxnError> {
    let ops = DeltaOps::from(table);

    let result = CommitBuilder::new(ops)
        .with_actions(actions)
        .await
        .map_err(|e| DeltaTxnError::CommitFailed(e.to_string()))?;

    Ok(result.version() as i64)
}
