//! The [Vfs] trait is responsible for converting stirng keys to [Read]
//! implementations.
use std::io::Read;

pub trait Vfs: Send + Sync + 'static {
    type Reader: VfsReader;
    type Error: std::error::Error;

    /// Open a file.
    fn open(&self, key: &str) -> Result<Self::Reader, Self::Error>;
}

pub trait VfsReader: Read + Send + Sync + 'static {
    type Error: std::error::Error;

    /// Will always be called by the cache when this object is no longer needed.
    fn close(self) -> Result<(), Self::Error>;

    /// If possible, return the size of this object once read.
    ///
    /// Only objects which can return their size are eligible for caching their encoded representations in memory.
    fn get_size(&self) -> Result<Option<u64>, Self::Error>;
}
