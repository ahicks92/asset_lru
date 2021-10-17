//! An internal Lru cell, which holds an item and records when it was last used.
use std::sync::Arc;
use std::time::Instant;

struct LruEntry<T> {
    item: Arc<T>,
    last_used: Instant,
}

impl<T> LruEntry<T> {
    pub(crate) fn new(item: T) -> LruEntry<T> {
        LruEntry {
            item: Arc::new(item),
            last_used: Instant::now(),
        }
    }

    /// read the item, making it used and returning a reference to the contents.
    pub fn get(&mut self) -> Arc<T> {
        self.last_used = Instant::now();
        self.item.clone()
    }
}
