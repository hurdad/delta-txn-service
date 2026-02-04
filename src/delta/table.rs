use deltalake::DeltaTable;
use std::collections::HashMap;

pub async fn open_table(
    table_uri: &str,
    storage_options: HashMap<String, String>,
) -> Result<DeltaTable, deltalake::DeltaTableError> {
    deltalake::open_table_with_storage_options(table_uri, storage_options).await
}
