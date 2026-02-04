use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct TableLockManager {
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl TableLockManager {
    pub fn lock_for(&self, table_uri: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(table_uri.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}
