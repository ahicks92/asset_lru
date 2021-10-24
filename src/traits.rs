//! The [Vfs] trait is responsible for converting string keys to [Read] implementations.
//!
//! The cache caches the bytes representation from whatever the [Vfs] returns, then uses a [Decoder] on it when needed
//! to get the actual object.
use std::io::{Error, Read};

/// "open" a "file" and return a [VfsReader] over it.
pub trait Vfs: Send + Sync + 'static {
    type Reader: VfsReader;

    /// Open a file.
    fn open(&self, key: &str) -> Result<Self::Reader, Error>;
}

/// A reader returned from the VFS.
///
/// Readers should handle closing in their drop implementations.
pub trait VfsReader: Read + Send + Sync + 'static {
    /// Return the size of this object once read.    
    fn get_size(&self) -> Result<u64, Error>;
}

/// A `Decoder` knows how to get from a reader to a decoded representation in memory.
///
/// The output type must be sync in order to enable the cache to store elements behind `Arc`.
pub trait Decoder {
    type Output: Send + Sync;
    type Error: std::error::Error;

    fn decode<R: Read>(&self, reader: R) -> Result<Self::Output, Self::Error>;

    /// Estimate the cost of a decoded item, usually the in-memory size.
    fn estimate_cost(&self, item: &Self::Output) -> Result<u64, Self::Error>;
}
