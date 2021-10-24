use std::fs::File;
use std::io::*;
use std::path::{Path, PathBuf};

use crate::*;

/// A VFS which is backed by a given root directory.
///
/// This handles the rather tricky path cases around Windows and Linux differences, and makes it so that you can and
/// should use keys like `/b/c` (behavior with `\` is undefined).  Additionally, it makes a best effort to disallow a
/// user to use relative paths to escape the root directory, primarily as a measure to detect bugs.
#[derive(Debug)]
pub struct FilesystemVfs {
    root_path: PathBuf,
}

fn conv_path(path: impl AsRef<Path>) -> Result<relative_path::RelativePathBuf> {
    relative_path::RelativePathBuf::from_path(path)
        .map_err(|_| Error::new(ErrorKind::Other, "Invalid path"))
}

impl FilesystemVfs {
    pub fn new(root_path: &Path) -> std::io::Result<FilesystemVfs> {
        Ok(FilesystemVfs {
            root_path: root_path.to_path_buf(),
        })
    }

    /// Run the file opening logic on the VFS, so that this can be reused for normal file access at the same time.
    pub fn open_file(&self, path: &Path) -> std::io::Result<File> {
        // On Windows, canonicalize is currently very broken when relative path segments appear in the middle of a
        // path, and stdlib doesn't help us out. Go via `RelativePathBuf` to clean it up.
        let absolute = conv_path(path)?.to_logical_path(&self.root_path);
        if !absolute.starts_with(&self.root_path) {
            return Err(Error::new(
                ErrorKind::Other,
                "path is outside the vfs root directory",
            ));
        }
        File::open(absolute)
    }
}

impl Vfs for FilesystemVfs {
    type Reader = File;

    fn open(&self, key: &str) -> std::io::Result<File> {
        self.open_file(Path::new(key))
    }
}

impl VfsReader for File {
    fn get_size(&self) -> Result<u64> {
        let meta = self.metadata()?;
        Ok(meta.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StringDecoder;

    impl Decoder for StringDecoder {
        type Output = String;
        type Error = Error;

        fn decode<R: Read>(&self, mut reader: R) -> Result<String> {
            let mut out = String::new();
            reader.read_to_string(&mut out)?;
            Ok(out)
        }

        fn estimate_cost(&self, item: &String) -> Result<u64> {
            Ok(item.len() as u64)
        }
    }

    #[test]
    fn test_filesystem_vfs() {
        let cache_config = AssetCacheConfig {
            max_single_object_bytes_cost: 100,
            max_bytes_cost: 1000,
            max_decoded_cost: 1000,
            max_single_object_decoded_cost: 1000,
        };

        let tmp_dir = tempfile::tempdir().unwrap();

        // Create a directory under the temporary directory so that we can test relative paths.
        let mut vfs_path = tmp_dir.path().to_path_buf();
        vfs_path.push("actual_dir");
        std::fs::create_dir(&vfs_path).unwrap();
        let vfs = FilesystemVfs::new(&vfs_path).unwrap();
        let cache =
            AssetCache::<FilesystemVfs, StringDecoder>::new(vfs, StringDecoder, cache_config);

        // Now let's write some files.
        std::fs::write(&vfs_path.join("a"), "aaaa").unwrap();
        std::fs::write(vfs_path.join("b"), "bbbb").unwrap();
        std::fs::write(vfs_path.join("c"), "cccc").unwrap();
        // Now, we want to write something outside the vfs.
        std::fs::write(vfs_path.parent().unwrap().join("d"), "dddd").unwrap();

        // Should be able to get a b and c.
        assert_eq!(&*cache.get("a").unwrap(), "aaaa");
        assert_eq!(&*cache.get("b").unwrap(), "bbbb");
        assert_eq!(&*cache.get("c").unwrap(), "cccc");

        // d should return a specific error.
        if let Err(AssetCacheError::<Error>::Vfs(e)) = cache.get("../d") {
            if e.kind() != ErrorKind::Other {
                panic!(
                    "Should get an other error for paths outside the vfs root: {:?}",
                    e
                );
            }
        } else {
            panic!("Should error when getting files outside the vfs directory");
        }
    }
}
