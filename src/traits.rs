//! The [Vfs] trait is responsible for converting string keys to [Read] implementations.
//!
//! The cache caches the bytes representation from whatever the [Vfs] returns, then uses a [Decoder] on it when needed
//! to get the actual object.
use std::io::{Error, Read, Seek};

/// "open" a "file" and return a [VfsReader] over it.
///
/// This is the first step of the decoding process, and is used to get from a string key to a reader over some bytes to
/// pass to the [Decoder].
pub trait Vfs: Send + Sync + 'static {
    type Reader: VfsReader;

    /// Open a file.
    fn open(&self, key: &str) -> Result<Self::Reader, Error>;
}

/// A reader returned from the VFS.
///
/// Readers should handle closing in their drop implementations.
pub trait VfsReader: Read + Seek + Send + Sync + 'static {
    /// Return the size of this object once read.
    ///
    /// This function should try to be as inexpensive as possible.
    fn get_size(&self) -> Result<u64, Error>;
}

/// A `Decoder` knows how to get from a reader to a decoded representation in memory.
///
/// The output type must be sync in order to enable the cache to store elements behind `Arc`.
///
/// This crate does not insert a [std::io::BufReader] for you.  You should do so yourself as needed.
pub trait Decoder {
    type Output: Send + Sync;
    type Error: std::error::Error;

    fn decode<R: Read>(&self, reader: R) -> Result<Self::Output, Self::Error>;

    /// Estimate the cost of a decoded item, usually the in-memory size.
    fn estimate_cost(&self, item: &Self::Output) -> Result<u64, Self::Error>;

    /// Sometimes it is possible for the cache to directly provide bytes.  Implement this optional method to take
    /// advantage of that case.
    ///
    /// By default this just forwards to the `read` function.  Useful because some decoders are faster if they can be
    /// fed a slice of bytes, for example serde_json.
    fn decode_bytes(&self, mut bytes: &[u8]) -> Result<Self::Output, Self::Error> {
        self.decode(&mut bytes)
    }
}

impl<T: Vfs> Vfs for std::sync::Arc<T> {
    type Reader = T::Reader;

    fn open(&self, key: &str) -> Result<Self::Reader, Error> {
        (**self).open(key)
    }
}
