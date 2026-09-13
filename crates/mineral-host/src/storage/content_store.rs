use std::{
    fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::domain::Sha256;
use mineral_core::ports::{BlobStore, ContentStoreError};
use mineral_core::publication::asset::ImmutableBlobSource;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// A local filesystem content-addressed store keyed by the SHA-256 of each blob.
#[derive(Clone, Debug)]
pub struct LocalContentStore {
    root: PathBuf,
}

impl LocalContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stores `content` under its SHA-256 identity without replacing an existing blob.
    pub fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
        let identity = Sha256::digest(content);
        fs::create_dir_all(&self.root)
            .map_err(|source| ContentStoreError::io("create content store", &self.root, source))?;

        let blob_path = self.blob_path(identity);
        match fs::read(&blob_path) {
            Ok(existing) => {
                Self::verify_existing(identity, content, &existing)?;
                return Ok(identity);
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ContentStoreError::io(
                    "read existing blob",
                    blob_path,
                    source,
                ));
            }
        }

        let temp_path = self.temp_path(identity);
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
                .map_err(|source| {
                    ContentStoreError::io("create temporary blob", &temp_path, source)
                })?;
            file.write_all(content).map_err(|source| {
                ContentStoreError::io("write temporary blob", &temp_path, source)
            })?;
            file.sync_all().map_err(|source| {
                ContentStoreError::io("sync temporary blob", &temp_path, source)
            })?;
            Ok::<(), ContentStoreError>(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        match fs::hard_link(&temp_path, &blob_path) {
            Ok(()) => {
                let _ = fs::remove_file(&temp_path);
                Ok(identity)
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temp_path);
                let existing = fs::read(&blob_path).map_err(|source| {
                    ContentStoreError::io("read concurrently stored blob", &blob_path, source)
                })?;
                Self::verify_existing(identity, content, &existing)?;
                Ok(identity)
            }
            Err(source) => {
                let _ = fs::remove_file(&temp_path);
                Err(ContentStoreError::io("publish blob", blob_path, source))
            }
        }
    }

    /// Reads a blob and verifies that its bytes still match the requested identity.
    pub fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
        let path = self.blob_path(identity);
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(ContentStoreError::Missing(identity));
            }
            Err(source) => return Err(ContentStoreError::io("read blob", path, source)),
        };
        let actual = Sha256::digest(&content);
        if actual != identity {
            return Err(ContentStoreError::Corrupt {
                expected: identity,
                actual,
            });
        }
        Ok(content)
    }

    /// Opens a bounded, streaming reader over one blob.
    ///
    /// The reader holds an open file and one caller-provided buffer, so an asset of
    /// any size moves through this process without ever being materialized in
    /// memory. Integrity is not assumed from the file name: the engine verifies the
    /// stream it reads against the frozen published facts, so a blob that changed
    /// under us is refused rather than uploaded.
    pub fn open_blob(&self, identity: Sha256) -> Result<LocalBlobSource, ContentStoreError> {
        LocalBlobSource::open(&self.blob_path(identity), identity)
    }

    fn blob_path(&self, identity: Sha256) -> PathBuf {
        self.root.join(identity.to_string())
    }

    fn temp_path(&self, identity: Sha256) -> PathBuf {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(
            ".{}-{}-{sequence}.tmp",
            identity,
            std::process::id()
        ))
    }

    fn verify_existing(
        identity: Sha256,
        expected_content: &[u8],
        existing: &[u8],
    ) -> Result<(), ContentStoreError> {
        let actual = Sha256::digest(existing);
        if actual != identity {
            return Err(ContentStoreError::Corrupt {
                expected: identity,
                actual,
            });
        }
        if existing != expected_content {
            return Err(ContentStoreError::HashCollision(identity));
        }
        Ok(())
    }
}

impl BlobStore for LocalContentStore {
    fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
        LocalContentStore::read(self, identity)
    }

    fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
        LocalContentStore::store(self, content)
    }

    fn open(
        &self,
        identity: Sha256,
    ) -> Result<Box<dyn ImmutableBlobSource + '_>, ContentStoreError> {
        Ok(Box::new(self.open_blob(identity)?))
    }
}

/// A bounded streaming reader over one local content-addressed blob.
///
/// It owns an open file and reads at most what the caller's buffer holds, so the
/// process working set is one buffer regardless of the object's size. It makes no
/// claim about the bytes: the engine verifies them.
pub struct LocalBlobSource {
    identity: Sha256,
    path: PathBuf,
    file: fs::File,
}

impl LocalBlobSource {
    fn open(path: &Path, identity: Sha256) -> Result<Self, ContentStoreError> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(ContentStoreError::Missing(identity));
            }
            Err(source) => {
                return Err(ContentStoreError::io("open blob", path, source));
            }
        };
        Ok(Self {
            identity,
            path: path.to_path_buf(),
            file,
        })
    }
}

impl ImmutableBlobSource for LocalBlobSource {
    fn identity(&self) -> Sha256 {
        self.identity
    }

    fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, ContentStoreError> {
        loop {
            match std::io::Read::read(&mut self.file, buffer) {
                Ok(read) => return Ok(read),
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(ContentStoreError::io("read blob", &self.path, source));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-content-store-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stores_and_reads_content_by_sha256() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());

        let identity = store.store(b"hello").unwrap();

        assert_eq!(store.read(identity).unwrap(), b"hello");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn storing_identical_content_again_is_idempotent() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());

        let first = store.store(b"hello").unwrap();
        let second = store.store(b"hello").unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn missing_blob_is_an_explicit_error() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let identity = Sha256::digest(b"missing");

        assert!(matches!(
            store.read(identity),
            Err(ContentStoreError::Missing(missing)) if missing == identity
        ));
    }

    #[test]
    fn a_blob_can_be_streamed_in_bounded_pieces() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let body = b"a published blob that is longer than one small read";
        let identity = store.store(body).unwrap();

        let mut source = store.open_blob(identity).unwrap();
        assert_eq!(source.identity(), identity);

        let mut collected = Vec::new();
        let mut buffer = [0_u8; 7];
        let mut reads = 0;
        loop {
            let read = source.read_chunk(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            assert!(read <= buffer.len());
            reads += 1;
            collected.extend_from_slice(&buffer[..read]);
        }

        assert_eq!(collected, body);
        assert!(reads > 1, "a seven-byte buffer must need several reads");
        // The stream reads the very file the identity names, with no full copy.
        assert_eq!(
            fs::metadata(directory.path().join(identity.to_string()))
                .unwrap()
                .len(),
            body.len() as u64
        );
    }

    #[test]
    fn opening_a_missing_blob_is_an_explicit_error() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let identity = Sha256::digest(b"missing");

        assert!(matches!(
            store.open_blob(identity),
            Err(ContentStoreError::Missing(missing)) if missing == identity
        ));
    }

    #[test]
    fn corrupted_blob_is_not_returned_as_valid_content() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let identity = store.store(b"original").unwrap();
        fs::write(directory.path().join(identity.to_string()), b"corrupt").unwrap();

        assert!(matches!(
            store.read(identity),
            Err(ContentStoreError::Corrupt { expected, .. }) if expected == identity
        ));
        assert!(matches!(
            store.store(b"original"),
            Err(ContentStoreError::Corrupt { expected, .. }) if expected == identity
        ));
    }
}
