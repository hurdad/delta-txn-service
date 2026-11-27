//! Commit history (GetHistory): one entry per table version, newest first,
//! read straight from the `_delta_log/<version>.json` commit files.
//!
//! Not delta-rs's `DeltaTable::history`: that returns bare CommitInfos
//! with no version numbers, and silently skips commits that have no
//! commitInfo action, so a caller can't even recover versions by position.
//! Here every version gets an entry, and a commit without commitInfo
//! still reports its version, timestamp (the file's) and file counts.

use deltalake::kernel::CommitInfo;
use deltalake::logstore::object_store::{self, ObjectStoreExt};
use deltalake::{DeltaTable, Path};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::de::IgnoredAny;
use serde::Deserialize;
use serde_json::Value;

use super::errors::DeltaTxnError;

pub const DEFAULT_HISTORY_LIMIT: usize = 100;
pub const MAX_HISTORY_LIMIT: usize = 1000;
/// Commit files fetched concurrently.
const FETCH_CONCURRENCY: usize = 16;

/// One table version's commit.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub version: u64,
    /// When the commit happened, in ms since the epoch: the commit's
    /// in-commit timestamp, else commitInfo.timestamp, else the commit
    /// file's last-modified time.
    pub timestamp_ms: i64,
    /// The commit's commitInfo action, if it has one.
    pub commit_info: Option<CommitInfo>,
    pub num_add_files: u64,
    pub num_remove_files: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    /// Newest first.
    pub entries: Vec<HistoryEntry>,
    pub latest_version: u64,
    /// Pass as `before_version` for the next (older) page; None when the
    /// oldest available commit is in this page.
    pub next_before_version: Option<u64>,
}

/// Up to `limit` commits older than `before_version` (exclusive; None
/// starts at the latest), from `table` loaded at its latest version.
///
/// Stops early, with no next page, at the first missing commit file: the
/// log has been cleaned up (log retention) below that version.
pub async fn read_history(
    table: &DeltaTable,
    limit: usize,
    before_version: Option<u64>,
) -> Result<HistoryPage, DeltaTxnError> {
    let latest = table
        .version()
        .ok_or_else(|| DeltaTxnError::OpenFailed("table has no version".into()))?;
    let start = match before_version {
        Some(before) if before > latest + 1 => {
            return Err(DeltaTxnError::VersionNotFound(format!(
                "before_version {before} is past the latest version {latest} plus one"
            )));
        }
        Some(0) => {
            return Ok(HistoryPage {
                entries: Vec::new(),
                latest_version: latest,
                next_before_version: None,
            });
        }
        Some(before) => before - 1,
        None => latest,
    };
    let oldest = start.saturating_sub(limit.saturating_sub(1) as u64);

    let store = table.log_store().object_store(None);
    let fetched: Vec<Option<HistoryEntry>> = stream::iter((oldest..=start).rev())
        .map(|version| {
            let store = store.clone();
            async move { read_commit(store.as_ref(), version).await }
        })
        .buffered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;

    let mut entries = Vec::with_capacity(fetched.len());
    let mut truncated = false;
    for entry in fetched {
        match entry {
            Some(e) => entries.push(e),
            None => {
                truncated = true;
                break;
            }
        }
    }
    let next_before_version = match entries.last() {
        Some(last) if !truncated && last.version > 0 => Some(last.version),
        _ => None,
    };
    Ok(HistoryPage {
        entries,
        latest_version: latest,
        next_before_version,
    })
}

/// None when the commit file doesn't exist.
async fn read_commit(
    store: &dyn object_store::ObjectStore,
    version: u64,
) -> Result<Option<HistoryEntry>, DeltaTxnError> {
    let path = Path::from(format!("_delta_log/{version:020}.json"));
    let result = match store.get(&path).await {
        Ok(r) => r,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(DeltaTxnError::OpenFailed(e.to_string())),
    };
    let file_modified = result.meta.last_modified.timestamp_millis();
    let bytes = result
        .bytes()
        .await
        .map_err(|e| DeltaTxnError::OpenFailed(e.to_string()))?;
    let mut entry = parse_commit(version, &bytes)
        .map_err(|e| DeltaTxnError::OpenFailed(format!("commit {version}: {e}")))?;
    if entry.timestamp_ms == 0 {
        entry.timestamp_ms = file_modified;
    }
    Ok(Some(entry))
}

/// One line of a commit file. Delta actions are single-key objects; add
/// and remove are skipped without being built (IgnoredAny), so a page of
/// large commits doesn't hold every file action in memory.
#[derive(Deserialize)]
struct LogLine {
    add: Option<IgnoredAny>,
    remove: Option<IgnoredAny>,
    #[serde(rename = "commitInfo")]
    commit_info: Option<Value>,
}

/// Parses one commit file (newline-delimited JSON actions). Only the
/// commitInfo action is deserialized in full; others are just counted.
/// `timestamp_ms` is 0 when the commit records none.
pub fn parse_commit(version: u64, bytes: &[u8]) -> Result<HistoryEntry, serde_json::Error> {
    let mut entry = HistoryEntry {
        version,
        timestamp_ms: 0,
        commit_info: None,
        num_add_files: 0,
        num_remove_files: 0,
    };
    for line in serde_json::Deserializer::from_slice(bytes).into_iter::<LogLine>() {
        let line = line?;
        if line.add.is_some() {
            entry.num_add_files += 1;
        }
        if line.remove.is_some() {
            entry.num_remove_files += 1;
        }
        if let Some(info) = line.commit_info {
            // A malformed commitInfo is history metadata, not table state:
            // keep the entry without it rather than failing the request.
            entry.commit_info = serde_json::from_value(info).ok();
        }
    }
    if let Some(info) = &entry.commit_info {
        entry.timestamp_ms = info.in_commit_timestamp.or(info.timestamp).unwrap_or(0);
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_counts_and_commit_info() {
        let commit = br#"{"commitInfo":{"timestamp":1700000000000,"operation":"WRITE","operationParameters":{"mode":"Append"},"readVersion":3,"isBlindAppend":true,"engineInfo":"spark/3.5"}}
{"add":{"path":"a.parquet","partitionValues":{},"size":1,"modificationTime":1,"dataChange":true}}
{"add":{"path":"b.parquet","partitionValues":{},"size":1,"modificationTime":1,"dataChange":true}}
{"remove":{"path":"c.parquet","dataChange":true}}
"#;
        let entry = parse_commit(4, commit).unwrap();
        assert_eq!(entry.version, 4);
        assert_eq!(entry.timestamp_ms, 1_700_000_000_000);
        assert_eq!((entry.num_add_files, entry.num_remove_files), (2, 1));
        let info = entry.commit_info.unwrap();
        assert_eq!(info.operation.as_deref(), Some("WRITE"));
        assert_eq!(info.read_version, Some(3));
        assert_eq!(info.engine_info.as_deref(), Some("spark/3.5"));
    }

    #[test]
    fn in_commit_timestamp_wins() {
        let commit = br#"{"commitInfo":{"timestamp":1,"inCommitTimestamp":2}}"#;
        assert_eq!(parse_commit(0, commit).unwrap().timestamp_ms, 2);
    }

    #[test]
    fn commits_without_commit_info_still_parse() {
        let commit = br#"{"add":{"path":"a.parquet","partitionValues":{},"size":1,"modificationTime":1,"dataChange":true}}"#;
        let entry = parse_commit(7, commit).unwrap();
        assert!(entry.commit_info.is_none());
        assert_eq!(entry.timestamp_ms, 0);
        assert_eq!(entry.num_add_files, 1);
    }

    #[test]
    fn malformed_commit_info_is_dropped_not_fatal() {
        let commit = br#"{"commitInfo":{"timestamp":"not a number"}}"#;
        assert!(parse_commit(0, commit).unwrap().commit_info.is_none());
        assert!(parse_commit(0, b"{not json").is_err());
    }
}
