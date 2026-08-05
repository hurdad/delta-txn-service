use super::errors::DeltaTxnError;
use deltalake::{ensure_table_uri, DeltaTable};
use std::collections::HashMap;

pub async fn open_table(
    table_uri: &str,
    storage_options: HashMap<String, String>,
) -> Result<DeltaTable, DeltaTxnError> {
    let table_url =
        ensure_table_uri(table_uri).map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?;
    deltalake::open_table_with_storage_options(table_url, storage_options)
        .await
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))
}
