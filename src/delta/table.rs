use deltalake::{ensure_table_uri, DeltaTable};
use std::collections::HashMap;

pub async fn open_table(
    table_uri: &str,
    storage_options: HashMap<String, String>,
) -> Result<DeltaTable, deltalake::DeltaTableError> {
    let table_url = ensure_table_uri(table_uri)?;
    deltalake::open_table_with_storage_options(table_url, storage_options).await
}
