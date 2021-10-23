//! The [Vfs] trait is responsible for converting string keys to [Read] implementations.
//!
//! The cache caches the bytes representation from whatever the [Vfs] returns, then uses a [Decoder] on it when needed
//! to get the actual object.
use std::io::Read;

///Estimate the cost of a decoded item.  This is usually in bytes.
///
/// The caches in this crate will cache up to a specified total cost, then begin
/// evicting entries which are least recently used.
///
/// As an example, for files, this is the size of the file.
pub trait EstimateCost {
    fn estimate_cost(&self) -> u64;
}

/// "open" a "file" and return a [VfsReader] over it.
pub trait Vfs: Send + Sync + 'static {
    type Reader: VfsReader;
    type Error: std::error::Error;

    /// Open a file.
    fn open(&self, key: &str) -> Result<Self::Reader, Self::Error>;
}

/// A reader returned from the VFS.
pub trait VfsReader: Read + Send + Sync + 'static {
    type Error: std::error::Error;

    /// Will always be called by the cache when this object is no longer needed, possibly communicating the error to the user.
    fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// If possible, return the size of this object once read.
    ///
    /// Only objects which can return their size are eligible for caching their encoded representations in memory.
    fn get_size(&self) -> Result<Option<u64>, Self::Error>;
}
