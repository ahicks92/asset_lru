//! The [AssetCache] drives a [Vfs], producing an output type from an input type with 2 levels of caching.
//!
//! Typically, the [Vfs] is a file-backed implementation, and the output type is something like a decoded image or audio
//! buffer.
//!
//! The cache caches at two points:
//!
//! - First, at the level of reading bytes into an in-memory buffer.
//! - Second, the actual decoded objects themselves.
//!
//! Any asset which is so critical that it must never be unloaded may be pinned with [AssetCache::cache_always], at
//! which point it may only be removed with [AssetCache::remove_key].
use std::io::{Error as IoError, Read};
use std::sync::{Arc, Mutex, RwLock};

use crate::*;

type CacheHashMap<V> = std::collections::HashMap<String, V, ahash::RandomState>;

#[derive(Debug, derive_builder::Builder)]
pub struct AssetCacheConfig {
    /// Maximum cost of the bytes cache in bytes.
    pub max_bytes_cost: u64,
    /// Maximum cost of the decoded cache in bytes.
    pub max_decoded_cost: u64,
    /// Point at which we will stop caching bytes.
    pub max_single_object_bytes_cost: u64,
    /// Point at which we will avoid caching decoded objects.
    pub max_single_object_decoded_cost: u64,
}

pub struct AssetCache<VfsImpl: Vfs, DecoderImpl: Decoder> {
    config: AssetCacheConfig,
    pinned_entries: RwLock<CacheHashMap<Arc<DecoderImpl::Output>>>,
    bytes_cache: Mutex<CostBasedLru<str, Vec<u8>>>,
    decoded_cache: Mutex<CostBasedLru<str, DecoderImpl::Output>>,
    /// Mutexes that stop multiple threads trying to decode the same content.
    decoding_guards: Mutex<CacheHashMap<Arc<Mutex<()>>>>,
    /// After eviction, we can still give the item back if something external kept it around; do so unless the user explicitly deleted it.
    weak_refs: RwLock<CacheHashMap<std::sync::Weak<DecoderImpl::Output>>>,
    vfs: VfsImpl,
    decoder: DecoderImpl,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError<DecoderImpl: Decoder> {
    Vfs(IoError),
    Decoder(DecoderImpl::Error),
}

impl<VfsImpl: Vfs, DecoderImpl: Decoder> AssetCache<VfsImpl, DecoderImpl> {
    pub fn new(
        vfs: VfsImpl,
        decoder: DecoderImpl,
        config: AssetCacheConfig,
    ) -> AssetCache<VfsImpl, DecoderImpl> {
        AssetCache {
            decoder,
            vfs,
            bytes_cache: Mutex::new(CostBasedLru::new(config.max_bytes_cost)),
            decoded_cache: Mutex::new(CostBasedLru::new(config.max_decoded_cost)),
            decoding_guards: Default::default(),
            pinned_entries: RwLock::new(Default::default()),
            weak_refs: RwLock::new(Default::default()),
            config,
        }
    }

    /// Find an item in the cache, returning `None` if it isn't currently cached.
    fn search_for_item(&self, key: &str) -> Option<Arc<DecoderImpl::Output>> {
        {
            let guard = self.pinned_entries.read().unwrap();
            if let Some(x) = guard.get(key) {
                return Some((*x).clone());
            }
        }

        {
            let mut guard = self.decoded_cache.lock().unwrap();
            if let Some(x) = guard.get(key) {
                return Some(x);
            }
        }

        // The unlikely pessimistic case is that this item is in the weak references; let's try to get it out.
        self.weak_refs
            .read()
            .unwrap()
            .get(key)
            .and_then(|x| x.upgrade())
    }

    /// Decode an item for the cache, assuming we definitely know it isn't present and are holding the guard necessary
    /// to stop other threads from attempting to do so in parallel.
    ///
    /// This is hard to break up into smaller functions, unfortunately.
    fn find_or_decode_postchecked(
        &self,
        key: &str,
    ) -> Result<Arc<DecoderImpl::Output>, CacheError<DecoderImpl>> {
        // First, if we can find the item, return it immediately.
        if let Some(x) = self.search_for_item(key) {
            return Ok(x);
        }

        // If we can get the size of the item, and it is less than the single object limit, we cache a vec of bytes.
        // Otherwise, we feed the reader into the decoder directly.
        let mut bytes_reader = self.vfs.open(key).map_err(CacheError::Vfs)?;
        let decoded = match bytes_reader.get_size().map_err(CacheError::Vfs)? {
            Some(s) if s <= self.config.max_single_object_bytes_cost => {
                let maybe_cached_bytes = self.bytes_cache.lock().unwrap().get(key);
                if let Some(x) = maybe_cached_bytes {
                    self.decoder
                        .decode(&mut &x[..])
                        .map_err(CacheError::Decoder)?
                } else {
                    // Read to a vec, insert that vec, then read from the vec.
                    let mut dest = vec![];
                    bytes_reader
                        .read_to_end(&mut dest)
                        .map_err(CacheError::Vfs)?;
                    let will_use = {
                        let mut guard = self.bytes_cache.lock().unwrap();
                        guard.insert(key.to_string().into(), dest, s);
                        guard.get(key).expect("We just inserted this")
                    };
                    self.decoder
                        .decode(&mut &will_use[..])
                        .map_err(CacheError::Decoder)?
                }
            }
            _ => {
                // The object was too big, or we couldn't get the size; in this case, we feed the vfs directly to the
                // decoder.
                self.decoder
                    .decode(bytes_reader)
                    .map_err(CacheError::Decoder)?
            }
        };

        let cost = self
            .decoder
            .estimate_cost(&decoded)
            .map_err(CacheError::Decoder)?;
        let res = if cost <= self.config.max_single_object_decoded_cost {
            let mut guard = self.decoded_cache.lock().unwrap();
            guard.insert(key.to_string().into(), decoded, cost);
            guard.get(key).expect("Just inserted")
        } else {
            Arc::new(decoded)
        };

        let weak = Arc::downgrade(&res);
        self.weak_refs
            .write()
            .unwrap()
            .insert(key.to_string(), weak);
        Ok(res)
    }

    /// Find or decode an item from the cache.
    fn find_or_decode(
        &self,
        key: &str,
    ) -> Result<Arc<DecoderImpl::Output>, CacheError<DecoderImpl>> {
        if let Some(x) = self.search_for_item(key) {
            return Ok(x);
        }

        // Stop any other threads from trying to decode this item, and make them wait on this thread to finish.
        let mutex = {
            let mut guard_inner = self.decoding_guards.lock().unwrap();
            let tmp = guard_inner
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())));
            (*tmp).clone()
        };
        // The type here is important: it makes sure that we actually lock the mutex, by making this variable definitely
        // b e a guard.  Any mistakes in the above rather complicated chain to set this up will be caught at compile
        // time.
        let _guard: std::sync::MutexGuard<()> = mutex.lock().unwrap();

        self.find_or_decode_postchecked(key)
    }

    /// Get an item from the cache, decoding if the item isn't present.
    pub fn get(&self, key: &str) -> Result<Arc<DecoderImpl::Output>, CacheError<DecoderImpl>> {
        self.find_or_decode(key)
    }

    /// Pin an item, so that it is always present in the cache.
    pub fn cache_always(&self, key: String, value: Arc<DecoderImpl::Output>) {
        let weak = Arc::downgrade(&value);
        self.pinned_entries
            .write()
            .unwrap()
            .insert(key.clone(), value);
        self.weak_refs.write().unwrap().insert(key, weak);
    }

    /// Remove an item from the cache.
    pub fn remove(&self, key: &str) {
        self.pinned_entries.write().unwrap().remove(key);
        self.bytes_cache.lock().unwrap().remove(key);
        self.decoding_guards.lock().unwrap().remove(key);
        self.weak_refs.write().unwrap().remove(key);
    }
}
