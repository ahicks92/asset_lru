//! A set of LRU caching mechanisms intended for smallish numbers of large objects.
//!
//! Sometimes, you have something like a compressed JSON file on disk, or maybe a lossy audio file, which is 10 times
//! smaller or more than the decoded representation after decompression/decoding.  Reading this from disk and running
//! the decoding process over and over is costly, but at the same time a simple map of keys to cached values will just
//! grow forever and isn't very smart because it can't optimize the case where we have enough memory to keep the bytes
//! from disk around as well.  This crate provides a solution to that problem via two types and some traits:
//!
//! [CostBasedLru] is a standard Lru cache which supports giving each item a cost.  When the cost is exceeded, the cache
//! will evict until the cost is below a threshold.  This is the basic low-level building block, and is exposed because
//! it's useful in other contexts.  This is the simplest piece to use: you just throw items at it.
//!
//! The higher level piece is [AssetCache], which returns `Arc`s wrapping a decoded item read from a [Vfs], with a
//! complex caching strategy:
//!
//! - First, we can cache the bytes we read from disk in an [CostBasedLru] if the object from disk is below a
//!   configurable threshold, evicting from the backing LRU as necessary.
//! - Next, we can cache the decoded object if the object's size is under a configurable threshold, in the same way.
//! - After that, if we don't cache the object at all, we can keep it around as a weak reference so that we can return
//!   the same one as long as something outside this crate is keeping it alive.
//! - And finally, for really important items, you can call `cache_always` to entirely short-circuit the mechanism and
//!   keep them around forever outside the book-keeping mechanisms.
//!
//! To use this crate, implement the [Vfs] and [Decoder] traits, then construct a [AssetCache] with your chosen
//! [AssetCacheConfig].
mod asset_cache;
mod cost_based_lru;
mod traits;

pub use asset_cache::*;
pub use cost_based_lru::*;
pub use traits::*;
