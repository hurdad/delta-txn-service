use super::errors::DeltaTxnError;
use deltalake::logstore::LogStore;
use deltalake::{DeltaTable, DeltaTableBuilder};
use std::collections::HashMap;
use url::Url;

/// Opens (loads) an already-existing table. Fails outright --
/// `DeltaTableError::NotATable`, wrapped as `DeltaTxnError::OpenFailed` --
/// for a `table_uri` with no `_delta_log` at all; callers that need to
/// tell "doesn't exist yet" apart from a genuine open failure (every
/// grpc::server handler) call `table_exists` first and only reach this
/// function once that's confirmed `true`.
///
/// Takes an already-`ensure_table_uri`-normalized `Url` rather than a raw
/// `&str`/re-normalizing internally: every caller in grpc::server has
/// already normalized (and allowlist-checked) the client-supplied
/// table_uri once by the time either this or `table_exists` is called, so
/// normalizing again here would just be a second, redundant pass over the
/// same string.
pub async fn open_table(
    table_url: &Url,
    storage_options: HashMap<String, String>,
) -> Result<DeltaTable, DeltaTxnError> {
    deltalake::open_table_with_storage_options(table_url.clone(), storage_options)
        .await
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))
}

/// Checks whether `table_uri` already has an initialized Delta log
/// (`_delta_log/*.json`/a checkpoint), without paying for a full load.
///
/// This exists because `open_table`/`open_table_with_storage_options`
/// fail outright -- `NotATable("... No files in log segment")` -- for a
/// location with no log at all, and that's indistinguishable at that
/// point from a genuine storage/permission failure; wrapped as
/// `DeltaTxnError::OpenFailed`, it used to surface as an opaque `Internal`
/// status even for the completely ordinary case of a client asking about
/// a table that simply hasn't been created yet. Calling this first lets
/// every handler treat "doesn't exist yet" as its own clean case: a
/// `Status::not_found` for the read RPCs (GetTable/ListActiveFiles), or
/// the trigger for grpc::server::commit()'s create-a-new-table path.
///
/// `DeltaTableBuilder::build()` (unlike `.load()`, which `open_table`
/// calls internally) does no I/O of its own -- it only constructs a log
/// store handle bound to `table_uri` -- so calling
/// `LogStore::is_delta_table_location()` on it here is the cheap check,
/// not a disguised full open.
///
/// Takes an already-normalized `Url` -- see `open_table`'s matching doc
/// comment for why.
pub async fn table_exists(
    table_url: &Url,
    storage_options: HashMap<String, String>,
) -> Result<bool, DeltaTxnError> {
    let table = DeltaTableBuilder::from_url(table_url.clone())
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?
        .with_storage_options(storage_options)
        .build()
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?;
    table
        .log_store()
        .is_delta_table_location()
        .await
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))
}
