use dashmap::DashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct TableLockManager {
    locks: Arc<DashMap<String, Arc<LockEntry>>>,
}

impl TableLockManager {
    pub fn lock_for(&self, table_uri: &str) -> TableLock {
        let entry = self
            .locks
            .entry(table_uri.to_string())
            .or_insert_with(|| Arc::new(LockEntry::new()));
        entry.ref_count.fetch_add(1, Ordering::AcqRel);
        TableLock {
            key: table_uri.to_string(),
            entry: entry.clone(),
            manager: self.clone(),
        }
    }

    // Correctness-critical: the ref_count==0 check and the map removal must
    // happen as one atomic step under this key's DashMap shard lock, not as
    // two separate operations (a `.load()` followed later by a `.remove()`,
    // which is what this used to do). Two concurrent commits to the same
    // table_uri, if one finishes (dropping its TableLock, ref_count -> 0)
    // at the exact moment the other calls lock_for(), used to race like
    // this: dropper reads ref_count==0 and *starts* removing the entry;
    // meanwhile the new caller's lock_for() -- itself only synchronized on
    // the same DashMap shard, not on this function's now-stale read --
    // finds the still-present entry, bumps its ref_count back to 1, and
    // returns a TableLock built on it; the dropper then completes its
    // removal anyway, evicting an entry a live TableLock still points to.
    // A *third* concurrent commit for the same table_uri would then find no
    // entry in the map and create a brand new LockEntry (a different
    // Mutex) -- so the second and third commits would each believe they
    // hold "the" per-table lock while actually holding two different
    // mutexes, with no mutual exclusion between them at all. Delta's own
    // commit protocol (atomic conditional-put on the log file) still
    // prevents actual data corruption if that happens, but it defeats the
    // whole reason this lock exists (see README's "Optional in-process
    // per-table async locks reduce conflicts") -- a spurious storage-level
    // version conflict where a clean, serialized commit was supposed to be
    // guaranteed. Using DashMap's Entry API (which holds the shard lock for
    // the entry's *entire* match arm, exactly like lock_for()'s own
    // `.entry()` call does) closes the window: a concurrent lock_for() for
    // this exact key cannot run at all until this function's Occupied/
    // Vacant match completes, since both take the same shard lock.
    fn remove_if_unused(&self, key: &str, entry: &Arc<LockEntry>) {
        if let dashmap::mapref::entry::Entry::Occupied(occupied) = self.locks.entry(key.to_string()) {
            if Arc::ptr_eq(occupied.get(), entry) && entry.ref_count.load(Ordering::Acquire) == 0 {
                occupied.remove();
            }
        }
    }
}

pub struct TableLock {
    key: String,
    entry: Arc<LockEntry>,
    manager: TableLockManager,
}

impl TableLock {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.entry.mutex.lock().await
    }
}

impl Drop for TableLock {
    fn drop(&mut self) {
        if self.entry.ref_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.manager.remove_if_unused(&self.key, &self.entry);
        }
    }
}

struct LockEntry {
    mutex: Mutex<()>,
    ref_count: AtomicUsize,
}

impl LockEntry {
    fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            ref_count: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    // Regression test for the exact bug remove_if_unused's Entry-API
    // rewrite fixes: many tasks repeatedly acquiring/releasing the lock for
    // the *same* table_uri, with a task yielding while "inside" the
    // critical section so a same-mutex violation has a real window to
    // manifest rather than needing to win an unlikely race by chance. Ran
    // against the old load()-then-remove() implementation, this test
    // reliably fails within a handful of runs; against the Entry-API
    // version it should never fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_lock_for_calls_for_the_same_key_never_grant_overlapping_access() {
        let manager = TableLockManager::default();
        let in_critical_section = Arc::new(AtomicBool::new(false));
        let overlap_detected = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for _ in 0..64 {
            let manager = manager.clone();
            let in_critical_section = in_critical_section.clone();
            let overlap_detected = overlap_detected.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..50 {
                    let table_lock = manager.lock_for("same-table");
                    let _guard = table_lock.lock().await;
                    if in_critical_section.swap(true, Ordering::SeqCst) {
                        overlap_detected.store(true, Ordering::SeqCst);
                    }
                    tokio::task::yield_now().await;
                    in_critical_section.store(false, Ordering::SeqCst);
                }
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        assert!(
            !overlap_detected.load(Ordering::SeqCst),
            "two concurrent lock_for(\"same-table\") holders executed inside the critical section \
             at the same time -- mutual exclusion was violated"
        );
    }

    // Complements the test above: heavy concurrent lock/unlock churn across
    // a small set of keys (so the ref-count-hits-zero-then-immediately-
    // reacquired race above has many chances to fire) must still leave the
    // manager's internal map completely empty once every TableLock has been
    // dropped -- a leaked entry is exactly what the old buggy
    // remove_if_unused (removing an entry a concurrent lock_for() had
    // already resurrected) could produce.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn heavy_concurrent_churn_leaves_no_entries_behind() {
        let manager = TableLockManager::default();
        let mut handles = Vec::new();
        for i in 0..32 {
            let manager = manager.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    let key = format!("table-{}", i % 4);
                    let table_lock = manager.lock_for(&key);
                    let _guard = table_lock.lock().await;
                    tokio::task::yield_now().await;
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        assert!(
            manager.locks.is_empty(),
            "expected every LockEntry to be cleaned up once all TableLocks were dropped, found {} \
             remaining",
            manager.locks.len()
        );
    }
}
