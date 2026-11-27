//! Per-replica cache of loaded tables, so a read or commit on a hot table
//! catches up incrementally instead of loading it from scratch.
//!
//! A full load reads the latest checkpoint and replays every log file
//! after it. With a cached table, `DeltaTable::update_incremental` lists
//! `_delta_log` and replays only commits newer than the cached version.
//! The listing still happens on every call, so results are exactly as
//! current as an uncached load: the cache never serves a stale version.
//!
//! Safety valves, since the Delta log is the source of truth:
//! - Before an entry is used, its commit file (`_delta_log/<version>.json`)
//!   is checked with one HEAD request against the size/last-modified/ETag
//!   recorded when it was cached. If it's gone or different, the table's
//!   log was rewritten (deleted and recreated at the same location), and
//!   the entry is reloaded from scratch. Incremental updates can't detect
//!   that on their own: they only look for commits *newer* than the cached
//!   version, so a recreated log with fewer versions looked unchanged and
//!   the old table kept being served (tests/e2e_snapshot_cache.rs).
//! - Any error while catching up also falls back to a full load.
//! - An entry older than `ttl` (since its last full load) is reloaded from
//!   scratch, as a backstop for anything the checks above can't see.
//!
//! In-process on purpose: a cached entry is a live delta-rs table (Arrow
//! file list plus kernel state), not something worth serializing to an
//! external cache. Each replica warms its own entries.
//!
//! Only latest-version access goes through here; time-travel reads load
//! the requested version directly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deltalake::logstore::object_store::ObjectStoreExt;
use deltalake::{DeltaTable, ObjectMeta, Path};
use opentelemetry::metrics::{Counter, Meter};
use opentelemetry::KeyValue;
use url::Url;

use super::errors::DeltaTxnError;
use super::table::open_table;

pub const DEFAULT_SNAPSHOT_CACHE_SIZE: usize = 32;
pub const DEFAULT_SNAPSHOT_CACHE_TTL: Duration = Duration::from_secs(300);

/// How a lookup was served -- the `result` attribute on
/// `delta_txn.snapshot_cache.lookups`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupResult {
    /// Caught up incrementally from a cached entry.
    Hit,
    /// No usable entry (absent, expired, or cache disabled): full load.
    Miss,
    /// A cached entry couldn't be reused (its commit file changed, or
    /// catching up failed); fell back to a full load.
    RefreshFailed,
}

impl LookupResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::RefreshFailed => "refresh_failed",
        }
    }
}

/// Identity of the commit file an entry was cached at. A deleted and
/// recreated log gets a different (or no) file at that path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitMarker {
    size: u64,
    last_modified: chrono::DateTime<chrono::Utc>,
    e_tag: Option<String>,
}

impl From<ObjectMeta> for CommitMarker {
    fn from(meta: ObjectMeta) -> Self {
        Self {
            size: meta.size,
            last_modified: meta.last_modified,
            e_tag: meta.e_tag,
        }
    }
}

/// HEADs the commit file for the table's current version, through the
/// table-scoped store (`root_object_store` is rooted above the table for
/// some backends, e.g. at `/` for file://). None when the
/// table has no version or the request fails; an entry without a marker
/// can't be verified and so is never reused.
async fn commit_marker(table: &DeltaTable) -> Option<CommitMarker> {
    let version = table.version()?;
    let path = Path::from(format!("_delta_log/{version:020}.json"));
    table
        .log_store()
        .object_store(None)
        .head(&path)
        .await
        .ok()
        .map(CommitMarker::from)
}

struct Entry {
    table: DeltaTable,
    marker: Option<CommitMarker>,
    /// When this entry was last loaded from scratch (drives the TTL).
    full_load_at: Instant,
    /// Logical clock for LRU eviction.
    last_used: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    clock: u64,
}

/// Cheap to clone (shared state behind an Arc).
#[derive(Clone)]
pub struct SnapshotCache {
    state: Arc<Mutex<State>>,
    capacity: usize,
    ttl: Duration,
    lookups: Option<Counter<u64>>,
}

impl SnapshotCache {
    /// `capacity` 0 disables caching: every call does a full load.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            state: Arc::default(),
            capacity,
            ttl,
            lookups: None,
        }
    }

    pub fn disabled() -> Self {
        Self::new(0, DEFAULT_SNAPSHOT_CACHE_TTL)
    }

    /// Records `delta_txn.snapshot_cache.lookups` by result.
    pub fn with_metrics(mut self, meter: &Meter) -> Self {
        self.lookups = Some(
            meter
                .u64_counter("delta_txn.snapshot_cache.lookups")
                .with_description("Table loads by how the snapshot cache served them")
                .build(),
        );
        self
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The table at its latest version: from the cache, caught up, when
    /// possible; otherwise loaded from scratch. Callers have already
    /// confirmed the table exists (`table_exists`).
    pub async fn latest(
        &self,
        table_url: &Url,
        storage_options: HashMap<String, String>,
    ) -> Result<DeltaTable, DeltaTxnError> {
        let (table, result) = self.latest_with_result(table_url, storage_options).await?;
        if let Some(counter) = &self.lookups {
            counter.add(1, &[KeyValue::new("result", result.as_str())]);
        }
        Ok(table)
    }

    /// `latest`, also reporting how it was served (for tests and metrics).
    pub async fn latest_with_result(
        &self,
        table_url: &Url,
        storage_options: HashMap<String, String>,
    ) -> Result<(DeltaTable, LookupResult), DeltaTxnError> {
        if self.capacity == 0 {
            let table = open_table(table_url, storage_options).await?;
            return Ok((table, LookupResult::Miss));
        }
        let key = table_url.as_str().to_string();

        let mut result = LookupResult::Miss;
        if let Some((mut table, full_load_at, marker)) = self.checkout(&key) {
            let unchanged = marker.is_some() && commit_marker(&table).await == marker;
            let refreshed = if unchanged {
                let before = table.version();
                table.update_incremental(None).await.map(|()| before)
            } else {
                Err(deltalake::DeltaTableError::Generic(
                    "the cached version's commit file is missing or changed".into(),
                ))
            };
            match refreshed {
                Ok(before) => {
                    let marker = if table.version() == before {
                        marker
                    } else {
                        commit_marker(&table).await
                    };
                    self.store(&key, table.clone(), full_load_at, marker);
                    return Ok((table, LookupResult::Hit));
                }
                Err(e) => {
                    tracing::info!(
                        error = %e,
                        table_uri = %table_url,
                        "cached snapshot can't be reused; reloading the table"
                    );
                    self.remove(&key);
                    result = LookupResult::RefreshFailed;
                }
            }
        }

        let table = open_table(table_url, storage_options).await?;
        let marker = commit_marker(&table).await;
        self.store(&key, table.clone(), Instant::now(), marker);
        Ok((table, result))
    }

    /// A clone of the live entry, unless absent or past its TTL (expired
    /// entries are dropped here).
    fn checkout(&self, key: &str) -> Option<(DeltaTable, Instant, Option<CommitMarker>)> {
        let mut state = self.state.lock().unwrap();
        state.clock += 1;
        let clock = state.clock;
        let expired = match state.entries.get_mut(key) {
            None => return None,
            Some(entry) if entry.full_load_at.elapsed() >= self.ttl => true,
            Some(entry) => {
                entry.last_used = clock;
                return Some((
                    entry.table.clone(),
                    entry.full_load_at,
                    entry.marker.clone(),
                ));
            }
        };
        if expired {
            state.entries.remove(key);
        }
        None
    }

    /// Inserts or replaces an entry, never replacing a newer version with
    /// an older one (two concurrent callers can finish in either order),
    /// and evicts the least recently used entry when over capacity.
    fn store(
        &self,
        key: &str,
        table: DeltaTable,
        full_load_at: Instant,
        marker: Option<CommitMarker>,
    ) {
        let mut state = self.state.lock().unwrap();
        state.clock += 1;
        let clock = state.clock;
        if let Some(existing) = state.entries.get(key) {
            if existing.table.version() > table.version() {
                return;
            }
        }
        state.entries.insert(
            key.to_string(),
            Entry {
                table,
                marker,
                full_load_at,
                last_used: clock,
            },
        );
        while state.entries.len() > self.capacity {
            let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            state.entries.remove(&oldest);
        }
    }

    fn remove(&self, key: &str) {
        self.state.lock().unwrap().entries.remove(key);
    }
}

/// Reads DELTA_TXN_SNAPSHOT_CACHE_SIZE / DELTA_TXN_SNAPSHOT_CACHE_TTL_SECS
/// through `lookup`; unset or unparseable values use the defaults.
pub fn snapshot_cache_from_env(lookup: impl Fn(&str) -> Option<String>) -> SnapshotCache {
    let parse = |key: &str| lookup(key).and_then(|v| v.trim().parse::<u64>().ok());
    let capacity = parse("DELTA_TXN_SNAPSHOT_CACHE_SIZE")
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_SNAPSHOT_CACHE_SIZE);
    let ttl = parse("DELTA_TXN_SNAPSHOT_CACHE_TTL_SECS")
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SNAPSHOT_CACHE_TTL);
    SnapshotCache::new(capacity, ttl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parsing_uses_defaults_and_overrides() {
        let c = snapshot_cache_from_env(|_| None);
        assert_eq!(c.capacity, DEFAULT_SNAPSHOT_CACHE_SIZE);
        assert_eq!(c.ttl, DEFAULT_SNAPSHOT_CACHE_TTL);

        let c = snapshot_cache_from_env(|k| match k {
            "DELTA_TXN_SNAPSHOT_CACHE_SIZE" => Some(" 0 ".into()),
            "DELTA_TXN_SNAPSHOT_CACHE_TTL_SECS" => Some("10".into()),
            _ => None,
        });
        assert_eq!(c.capacity, 0);
        assert_eq!(c.ttl, Duration::from_secs(10));

        let c = snapshot_cache_from_env(|_| Some("not a number".into()));
        assert_eq!(c.capacity, DEFAULT_SNAPSHOT_CACHE_SIZE);
    }

    #[test]
    fn lookup_result_labels() {
        assert_eq!(LookupResult::Hit.as_str(), "hit");
        assert_eq!(LookupResult::Miss.as_str(), "miss");
        assert_eq!(LookupResult::RefreshFailed.as_str(), "refresh_failed");
    }
}
