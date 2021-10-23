//! a [CostBasedLru] is an Lru cache which uses the cost of the items in the cache to decide when to evict.
//!
//! This is implemented as a vec-backed linked listwhere the items are allocated on the heap behind `Arc`, plus an
//! auxiliary hash-based index.
//!
//! The keys may not die immediately on eviction; only the value should be large.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use ahash::RandomState;

struct OccupiedEntry<K, V> {
    key: Arc<K>,
    item: Arc<V>,
    prev: Option<usize>,
    next: Option<usize>,
    cost: u64,
}

struct EmptyEntry {
    next_empty: Option<usize>,
}

enum CacheEntry<K, V> {
    /// This entry is empty, possibly with a pointer at the next empty entry.
    Empty(EmptyEntry),
    /// This entry is occupied, and doubley linked to the previous and next entry.
    Occupied(OccupiedEntry<K, V>),
}

impl<K, V> CacheEntry<K, V> {
    fn as_occupied_mut(&mut self) -> &mut OccupiedEntry<K, V> {
        match self {
            Self::Occupied(ref mut x) => x,
            _ => panic!("Entry should be occupied"),
        }
    }

    fn as_occupied(&self) -> &OccupiedEntry<K, V> {
        match self {
            Self::Occupied(ref x) => x,
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

pub struct CostBasedLru<K: std::hash::Hash + Eq, V> {
    entries: Vec<CacheEntry<K, V>>,
    /// Points at the index of the key.
    index: HashMap<Arc<K>, usize, RandomState>,
    // At what cost do we start evicting?
    max_cost: u64,
    entries_head: Option<usize>,
    entries_tail: Option<usize>,
    empty_head: Option<usize>,
    /// Current cost of the items in the cache.
    current_cost: u64,
}

impl<K: Hash + Eq + std::fmt::Debug, V: std::fmt::Debug> CostBasedLru<K, V> {
    pub fn new(max_cost: u64) -> CostBasedLru<K, V> {
        CostBasedLru {
            entries: Default::default(),
            index: Default::default(),
            max_cost,
            entries_head: None,
            entries_tail: None,
            empty_head: None,
            current_cost: 0,
        }
    }

    /// Entirely unlink an occupied index from the list.
    /// Used as a precursor step to lots of things such as patching up the head.
    fn unlink_index(&mut self, index: usize) {
        // Easiest to handle the tail first.
        if Some(index) == self.entries_tail {
            self.entries_tail = self.entries[index].as_occupied().prev;
        }

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
        if let Some(i) = self.entries_head {
            self.entries[i].as_occupied_mut().prev = Some(index);
        }
        self.entries_head = Some(index);

        // If this is the only entry, then unlinking it broke the tail.
        if self.entries_tail.is_none() {
            self.entries_tail = Some(index);
        }
    }

    pub fn get(&mut self, key: &K) -> Option<Arc<V>> {
        let ind = *self.index.get(key)?;
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
            CacheEntry::Occupied(OccupiedEntry {
                key, item, cost, ..
            }) => {
                self.index.remove(&key);
                self.current_cost -= cost;
                item
            }
            _ => panic!("Should have been occupied"),
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        let ind = self.index.remove(key)?;
        let old = self.become_empty(ind);
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
    pub fn insert(&mut self, key: K, value: V, cost: u64) -> Option<Arc<V>> {
        let key_arc = Arc::new(key);
        let ret = self.remove(&*key_arc);
        let ind = self.find_empty();
        let old_head = self.entries_head;

        self.entries[ind] = CacheEntry::Occupied(OccupiedEntry {
            key: key_arc.clone(),
            item: Arc::new(value),
            prev: None,
            next: self.entries_head,
            cost,
        });
        self.entries_head = Some(ind);
        self.index.insert(key_arc, ind);
        self.current_cost += cost;

        // Link up the prev of the old head.
        if let Some(h) = old_head {
            self.entries[h].as_occupied_mut().prev = self.entries_head;
        }

        // If there's no tail this was the first insert and we need one.
        if self.entries_tail.is_none() {
            self.entries_tail = Some(ind);
        }

        self.maybe_evict();
        ret
    }

    /// Run a cache eviction if required.
    fn maybe_evict(&mut self) {
        while self.current_cost > self.max_cost {
            let cur = match self.entries_tail {
                Some(t) => t,
                None => panic!("Not enough entries to explain cost"),
            };

            self.become_empty(cur);
        }
    }

    /// Iterator visiting entries in most-recently-used order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        let mut ind = self.entries_head;
        std::iter::from_fn(move || {
            let next = ind?;
            let ret = self.entries[next].as_occupied();
            ind = ret.next;
            Some((&*ret.key, &*ret.item))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use lru::LruCache;
    use proptest::prelude::*;

    /// Simple helper to build proptest strategies so that we can test the one-based base case against [LruCache].
    #[derive(Copy, Clone, Debug, Ord, Eq, PartialOrd, PartialEq)]
    enum CacheCommand {
        Put(u64, u64),
        Get(u64),
        Delete(u64),
    }

    fn cache_command_strat(
        max_key: std::ops::Range<u64>,
        max_value: std::ops::Range<u64>,
    ) -> prop::strategy::BoxedStrategy<CacheCommand> {
        proptest::prop_oneof![
            max_key.clone().prop_map(CacheCommand::Get),
            (max_key.clone(), max_value).prop_map(|(x, y)| CacheCommand::Put(x, y)),
            max_key.prop_map(CacheCommand::Delete),
        ]
        .boxed()
    }

    // Run some tests against bounded lru caches.  When we set max_cost to the capacity and the cost of
    // all inputted keys as 1, we get something exactly equivalent to `[LruCache].
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 1000,
            max_shrink_iters: 100000,
            ..Default::default()
        })]
        #[test]
        fn test_against_lru_cache_bounded(
            bound in 1..1000u64,
            commands in prop::collection::vec(cache_command_strat(0..100, 0..10000), 0..10000)
        ) {
            let mut known_good = LruCache::<u64, u64>::new(bound as usize);
            let mut ours = CostBasedLru::<u64, u64>::new(bound as u64);

            for c in commands {
                use CacheCommand::*;

                match c {
                    Get(k) => {
                        let left: Option<u64> = known_good.get(&k).cloned();
                        let right: Option<u64> = ours.get(&k).as_deref().cloned();
                        prop_assert_eq!(left, right);
                    },
                    Put(k, v) => prop_assert_eq!(known_good.put(k, v), ours.insert(k, v, 1).as_deref().cloned()),
                    Delete(k) => prop_assert_eq!(known_good.pop(&k), ours.remove(&k).as_deref().cloned()),
                }

                //let good_state = known_good.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>();
                //let our_state = ours.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>();
                //prop_assert_eq!(&good_state, &our_state);
                //prop_assert_eq!(good_state.len() as u64, ours.current_cost);
            }
        }
    }

    // We know everything else works, including complex linked lists for eviction, but let's still check what happens
    // without a cost of zero.
    #[test]
    fn test_eviction() {
        let mut cache = CostBasedLru::<u64, u64>::new(10);
        cache.insert(1, 1, 1);
        cache.insert(2, 2, 2);
        cache.insert(3, 3, 3);
        cache.insert(4, 4, 4);
        cache.insert(5, 5, 5);

        let state = cache
            .iter()
            .map(|x| (*x.0, *x.1))
            .collect::<Vec<(u64, u64)>>();
        assert_eq!(state, vec![(5, 5), (4, 4)]);
    }
}
