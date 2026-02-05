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

    fn remove_if_unused(&self, key: &str, entry: &Arc<LockEntry>) {
        if entry.ref_count.load(Ordering::Acquire) != 0 {
            return;
        }

        let existing = self.locks.get(key);
        if let Some(existing) = existing {
            if Arc::ptr_eq(&existing, entry) {
                drop(existing);
                self.locks.remove(key);
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
