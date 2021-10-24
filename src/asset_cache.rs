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
pub enum CacheError<DecoderError> {
    Vfs(IoError),
    Decoder(DecoderError),
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
    ) -> Result<Arc<DecoderImpl::Output>, CacheError<DecoderImpl::Error>> {
        // First, if we can find the item, return it immediately.
        if let Some(x) = self.search_for_item(key) {
            return Ok(x);
        }

        // If we can get the size of the item, and it is less than the single object limit, we cache a vec of bytes.
        // Otherwise, we feed the reader into the decoder directly.

        let mut bytes_reader = self.vfs.open(key).map_err(CacheError::Vfs)?;
        let size = bytes_reader.get_size().map_err(CacheError::Vfs)?;
        let decoded = if size <= self.config.max_single_object_bytes_cost {
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
                    guard.insert(key.to_string().into(), dest, size);
                    guard.get(key).expect("We just inserted this")
                };
                self.decoder
                    .decode(&mut &will_use[..])
                    .map_err(CacheError::Decoder)?
            }
        } else {
            // The object was too big, or we couldn't get the size; in this case, we feed the vfs directly to the
            // decoder.
            self.decoder
                .decode(bytes_reader)
                .map_err(CacheError::Decoder)?
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
    ) -> Result<Arc<DecoderImpl::Output>, CacheError<DecoderImpl::Error>> {
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
        // be a guard.  Any mistakes in the above rather complicated chain to set this up will be caught at compile
        // time.
        let _guard: std::sync::MutexGuard<()> = mutex.lock().unwrap();

        self.find_or_decode_postchecked(key)
    }

    /// Get an item from the cache, decoding if the item isn't present.
    pub fn get(
        &self,
        key: &str,
    ) -> Result<Arc<DecoderImpl::Output>, CacheError<DecoderImpl::Error>> {
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
        self.decoded_cache.lock().unwrap().remove(key);
        self.weak_refs.write().unwrap().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A VFS wrapping a `HashMap` for testing.
    struct HashMapVfs(Mutex<HashMap<String, Vec<u8>>>);

    impl Vfs for Arc<HashMapVfs> {
        type Reader = std::io::Cursor<Vec<u8>>;

        fn open(&self, key: &str) -> Result<Self::Reader, IoError> {
            let ret = self
                .0
                .lock()
                .unwrap()
                .get(key)
                .ok_or_else(|| {
                    IoError::new(std::io::ErrorKind::NotFound, "Entry not found".to_string())
                })?
                .clone();
            Ok(std::io::Cursor::new(ret))
        }
    }

    impl VfsReader for std::io::Cursor<Vec<u8>> {
        fn get_size(&self) -> Result<u64, IoError> {
            Ok(self.get_ref().len() as u64)
        }
    }

    // Add a helper to put things into the vfs.
    impl HashMapVfs {
        fn new() -> HashMapVfs {
            HashMapVfs(Mutex::new(Default::default()))
        }

        pub fn insert(&self, key: &str, value: Vec<u8>) -> Option<Vec<u8>> {
            self.0.lock().unwrap().insert(key.to_string(), value)
        }

        fn remove(&self, key: &str) -> Option<Vec<u8>> {
            self.0.lock().unwrap().remove(key)
        }
    }

    struct HashMapDecoder;

    impl Decoder for HashMapDecoder {
        type Error = IoError;
        type Output = String;

        fn decode<R: Read>(&self, mut reader: R) -> Result<String, IoError> {
            let mut out = String::new();
            reader.read_to_string(&mut out)?;
            Ok(out)
        }

        fn estimate_cost(&self, item: &String) -> Result<u64, IoError> {
            Ok(item.len() as u64)
        }
    }

    fn build_cache() -> (Arc<HashMapVfs>, AssetCache<Arc<HashMapVfs>, HashMapDecoder>) {
        let cfg = AssetCacheConfigBuilder::default()
            .max_bytes_cost(50)
            .max_single_object_bytes_cost(10)
            .max_decoded_cost(60)
            .max_single_object_decoded_cost(12)
            .build()
            .expect("Should build");
        let vfs = Arc::new(HashMapVfs::new());
        (vfs.clone(), AssetCache::new(vfs, HashMapDecoder, cfg))
    }

    // Test some basic common cache operations.
    #[test]
    fn basic_ops() {
        let (vfs, cache) = build_cache();
        vfs.insert("a", "abc".into());
        vfs.insert("b", "def".into());

        assert_eq!(&*cache.get("a").unwrap(), "abc");
        assert_eq!(&*cache.get("b").unwrap(), "def");

        // We should find these keys.
        cache.search_for_item("a").expect("Should find the key");
        cache.search_for_item("b").expect("Should find the item");

        cache.remove("b");
        assert!(cache.search_for_item("b").is_none());
        cache.search_for_item("a").expect("Key should be found");
    }

    #[test]
    fn test_single_object_limits() {
        let (vfs, cache) = build_cache();

        const SMALL: &str = "small";
        const NO_CACHE_BYTES: &str = "no_cache_bytes";
        const MAX_BYTES: &str = "max_bytes";
        const MAX_DECODED: &str = "max_decoded";
        const NO_CACHE: &str = "no_cache";

        vfs.insert(SMALL, "abc".into());
        vfs.insert(MAX_BYTES, "abcdefghij".into());
        // Big enough that decoding it won't cache the bytes.
        vfs.insert(NO_CACHE_BYTES, "abcdefghijk".into());
        // Largest object we'll cache.
        vfs.insert(MAX_DECODED, "abcdefghijkl".into());
        // Big enough that we don't cache it.
        vfs.insert(NO_CACHE, "abcdefghijklm".into());

        // Load up the cache.
        for i in &[SMALL, MAX_BYTES, MAX_DECODED, NO_CACHE, NO_CACHE_BYTES] {
            cache.get(i).expect("Should decode fine");
        }

        // All but NO_CACHE should be findable.
        assert_eq!(&*cache.search_for_item(SMALL).unwrap(), "abc");
        assert_eq!(&*cache.search_for_item(MAX_BYTES).unwrap(), "abcdefghij");
        assert_eq!(
            &*cache.search_for_item(MAX_DECODED).unwrap(),
            "abcdefghijkl"
        );
        assert_eq!(
            &*cache.search_for_item(NO_CACHE_BYTES).unwrap(),
            "abcdefghijk"
        );
        assert!(cache.search_for_item(NO_CACHE).is_none());
    }

    /// If we cache objects which are otherwise too large for the cache, or if an object is purged, we can still get the
    /// objects via our internal cache of weak references.
    #[test]
    fn test_weak_recovery() {
        let (vfs, cache) = build_cache();

        // insert a bunch of keys, holding onto the arcs.
        let mut arcs = vec![];
        for i in 0..100 {
            let key = format!("{}", i);
            let val = format!("{}", i);
            vfs.insert(&key, val.into());
            arcs.push(cache.get(&key).unwrap());
        }

        // Let's verify that key "1" isn't in any of the places we expect it to be.
        assert!(cache.bytes_cache.lock().unwrap().get("1").is_none());
        assert!(cache.decoded_cache.lock().unwrap().get("1").is_none());
        // But it should be in the weak map.
        assert!(cache.weak_refs.read().unwrap().get("1").is_some());

        // And looking for it should find it.
        assert_eq!(&*cache.get("1").unwrap(), "1");

        // If we drop our arcs, we can't find it anymore.
        arcs.clear();
        assert!(cache.search_for_item("1").is_none());

        // If we put a really big item in, then it doesn't cache. But holding onto the arc will let us get it back
        // anyway.
        vfs.insert("big", "abcdefghijklmnopqrstuvwxyz".into());
        let sref = cache.get("big");
        assert!(cache.bytes_cache.lock().unwrap().get("big").is_none());
        assert!(cache.decoded_cache.lock().unwrap().get("big").is_none());
        assert_eq!(&*cache.get("big").unwrap(), "abcdefghijklmnopqrstuvwxyz");
        // But droping sref makes it go away.
        std::mem::drop(sref);
        assert!(cache.search_for_item("big").is_none());
    }
}
