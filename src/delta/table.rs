use super::errors::DeltaTxnError;
use chrono::{DateTime, Utc};
use deltalake::logstore::LogStore;
use deltalake::{DeltaTable, DeltaTableBuilder, DeltaTableError};
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

/// Which state of a table a read RPC should see (GetTable /
/// ListActiveFiles time travel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAt {
    Latest,
    Version(u64),
    /// The latest version committed at or before this instant.
    Timestamp(DateTime<Utc>),
}

/// Like `open_table`, but at a specific version or timestamp. A version
/// or timestamp the log can't satisfy becomes
/// `DeltaTxnError::VersionNotFound` (gRPC NOT_FOUND) rather than an
/// opaque internal error, so a client can tell a bad request from a
/// storage failure.
///
/// delta-rs reports "version newer than the log" as a generic kernel error
/// string, so a requested version is checked against the log's latest
/// version first (a cheap `_delta_log` listing, no snapshot load). A
/// version at or below the latest that still fails to load has been
/// cleaned out of the log (log retention), which is also NOT_FOUND.
pub async fn open_table_at(
    table_url: &Url,
    storage_options: HashMap<String, String>,
    at: ReadAt,
) -> Result<DeltaTable, DeltaTxnError> {
    if at == ReadAt::Latest {
        return open_table(table_url, storage_options).await;
    }
    // build() does no I/O; the load_* calls below do the actual read.
    let mut table = DeltaTableBuilder::from_url(table_url.clone())
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?
        .with_storage_options(storage_options)
        .build()
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?;

    match at {
        ReadAt::Latest => unreachable!("handled above"),
        ReadAt::Version(v) => {
            let latest = table
                .log_store()
                .get_latest_version(0)
                .await
                .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?;
            if v > latest {
                return Err(DeltaTxnError::VersionNotFound(format!(
                    "version {v} does not exist (latest is {latest})"
                )));
            }
            table.load_version(v).await.map_err(|e| {
                tracing::warn!(error = %e, version = v, "time-travel load failed");
                DeltaTxnError::VersionNotFound(format!(
                    "version {v} is no longer available in the table's log"
                ))
            })?;
        }
        ReadAt::Timestamp(ts) => {
            let not_found = || {
                DeltaTxnError::VersionNotFound(format!(
                    "no table version at or before {}",
                    ts.to_rfc3339()
                ))
            };
            table.load_with_datetime(ts).await.map_err(|e| match e {
                DeltaTableError::InvalidVersion(_)
                | DeltaTableError::NotATable(_)
                | DeltaTableError::InvalidDateTimeString { .. } => not_found(),
                other => DeltaTxnError::OpenFailed(other.to_string()),
            })?;
            // delta-rs clamps a timestamp before the first commit to the
            // oldest version; Delta (Spark) treats that as "no version
            // existed yet", and so does this service.
            let loaded = table.version().unwrap_or(0);
            let committed_at = table
                .snapshot()
                .ok()
                .and_then(|snapshot| snapshot.version_timestamp(loaded));
            if committed_at.is_some_and(|t| t > ts.timestamp_millis()) {
                return Err(not_found());
            }
        }
    }
    Ok(table)
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
