//! a [CostBasedLru] is an Lru cache which uses the cost of the items in the cache to decide when to evict.
//!
//! This is implemented as a vec-backed linked listwhere the items are allocated on the heap behind `Arc`, plus an
//! auxiliary hash-based index.
//!
//! The keys may not die immediately on eviction; only the value should be large.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use crate::*;

struct OccupiedEntry<T> {
    item: Arc<T>,
    prev: Option<usize>,
    next: Option<usize>,
    cost: u64,
}

struct EmptyEntry {
    next_empty: Option<usize>,
}

enum CacheEntry<T> {
    /// This entry is empty, possibly with a pointer at the next empty entry.
    Empty(EmptyEntry),
    /// This entry is occupied, and doubley linked to the previous and next entry.
    Occupied(OccupiedEntry<T>),
}

impl<T> CacheEntry<T> {
    fn as_occupied_mut(&mut self) -> &mut OccupiedEntry<T> {
        match self {
            Self::Occupied(ref mut x) => x,
            _ => panic!("Entry should be occupied"),
        }
    }

    fn as_empty_mut(&mut self) -> &mut EmptyEntry {
        match self {
            CacheEntry::Empty(ref mut x) => x,
            _ => panic!("Entry should be empty"),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::Empty(_))
    }
}

pub struct CostBasedLru<K: std::hash::Hash + Eq, V: EstimateCost> {
    entries: Vec<CacheEntry<V>>,
    /// Points at the index of the key.
    index: HashMap<K, usize>,
    // At what cost do we start evicting?
    max_cost: u64,
    entries_head: Option<usize>,
    empty_head: Option<usize>,
    /// Current cost of the items in the cache.
    current_cost: u64,
}

impl<K: Hash + Eq, V: EstimateCost> CostBasedLru<K, V> {
    pub fn new(max_cost: u64) -> CostBasedLru<K, V> {
        CostBasedLru {
            entries: Default::default(),
            index: Default::default(),
            max_cost,
            entries_head: None,
            empty_head: None,
            current_cost: 0,
        }
    }

    /// Entirely unlink an occupied index from the list.
    /// Used as a precursor step to lots of things such as patching up the head.
    fn unlink_index(&mut self, index: usize) {
        if Some(index) == self.entries_head {
            // unlinking the head is special.
            self.entries_head = self.entries[index].as_occupied_mut().next;
            if let Some(n) = self.entries_head {
                self.entries[n].as_occupied_mut().prev = None;
            }

            return;
        }

        // Otherwise we just do a standard linked list unlink.
        let old_prev = self.entries[index]
            .as_occupied_mut()
            .prev
            .expect("Isn't the head");
        let old_next = self.entries[index].as_occupied_mut().next;
        self.entries[old_prev].as_occupied_mut().next = old_next;
        if let Some(n) = old_next {
            self.entries[n].as_occupied_mut().prev = Some(old_prev);
        }
    }

    /// Given the index of an occupied entry, make it the most recent item.
    fn make_most_recent(&mut self, index: usize) {
        self.unlink_index(index);
        self.entries[index].as_occupied_mut().next = self.entries_head;
        self.entries_head = Some(index);
    }

    pub fn get(&mut self, key: &K) -> Option<Arc<V>> {
        let ind = *self.index.get(key)?;
        if ind >= self.entries.len() {
            return None;
        }

        self.make_most_recent(ind);
        Some(self.entries[ind].as_occupied_mut().item.clone())
    }

    /// Make a specific index of the map become empty.
    fn become_empty(&mut self, index: usize) -> Arc<V> {
        self.unlink_index(index);
        let mut old = CacheEntry::Empty(EmptyEntry {
            next_empty: self.empty_head,
        });
        std::mem::swap(&mut old, &mut self.entries[index]);
        self.empty_head = Some(index);
        match old {
            CacheEntry::Occupied(OccupiedEntry { item, .. }) => item,
            _ => panic!("Should have been occupied"),
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        let ind = self.index.remove(key)?;
        if ind >= self.entries.len() {
            return None;
        }
        if let CacheEntry::Empty(_) = self.entries[ind] {
            return None;
        }

        let old_cost = self.entries[ind].as_occupied_mut().cost;
        let old = self.become_empty(ind);
        self.current_cost
            .checked_sub(old_cost)
            .expect("Should never underflow");

        Some(old)
    }

    /// Find an available empty index, or make one if necessary.
    fn find_empty(&mut self) -> usize {
        if let Some(e) = self.empty_head {
            self.empty_head = self.entries[e].as_empty_mut().next_empty;
            return e;
        }

        self.entries
            .push(CacheEntry::Empty(EmptyEntry { next_empty: None }));
        self.entries.len() - 1
    }

    /// Add an entry to the cache.  Return the old entry if this key was already present.
    pub fn insert(&mut self, key: K, value: V) -> Option<Arc<V>> {
        let ret = self.remove(&key);
        let ind = self.find_empty();
        let cost = value.estimate_cost();
        self.entries[ind] = CacheEntry::Occupied(OccupiedEntry {
            item: Arc::new(value),
            prev: None,
            next: self.entries_head,
            cost,
        });
        self.entries_head = Some(ind);
        self.current_cost
            .checked_add(cost)
            .expect("Should never overflow");
        self.maybe_evict();
        ret
    }

    /// Run a cache eviction if required.
    fn maybe_evict(&mut self) {
        if self.current_cost <= self.max_cost {
            return;
        }

        // Otherwise, we iterate until we find the first item which goes over the current cost...
        let mut start_evicting_at = match self.entries_head {
            Some(x) => x,
            // Might as well support caches with zero max cost.
            None => return,
        };

        let mut cost = self.entries[start_evicting_at].as_occupied_mut().cost;
        while cost < self.max_cost {
            if let Some(n) = self.entries[start_evicting_at].as_occupied_mut().next {
                start_evicting_at = n;
                cost += self.entries[n].as_occupied_mut().cost;
            } else {
                panic!("We're over-cost, but don't have enough entries. cache is corrupt");
            }

            let mut evict = Some(start_evicting_at);
            while let Some(e) = evict {
                evict = self.entries[start_evicting_at].as_occupied_mut().next;
                // We could optimize this because it goes through the unlinking steps and so on.  There shouldn't be a
                // point though.
                self.become_empty(e);
            }
        }
    }
}
