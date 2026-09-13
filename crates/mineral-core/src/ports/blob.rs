use std::{error::Error, fmt, io, path::PathBuf};

use crate::{
    domain::Sha256,
    publication::asset::{BufferedBlobSource, ImmutableBlobSource},
};

/// Content-addressed blob access required by the portable engine.
///
/// One implementation backs the native filesystem CAS; the Cloudflare adapter
/// provides an R2-backed implementation later. `read` must return exactly the
/// bytes whose SHA-256 equals `identity`, and `store` must never replace an
/// existing blob with different bytes.
pub trait BlobStore {
    fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError>;
    fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError>;

    /// Opens a bounded, streaming reader over one immutable blob.
    ///
    /// The default implementation reads the blob completely and hands it out in
    /// chunks: that keeps every store honest about the *shape* of the contract,
    /// but it still holds the whole object in memory. A runtime that publishes
    /// large media must override this with a real stream, so that an asset is
    /// never fully materialized on the way to the object store.
    fn open(
        &self,
        identity: Sha256,
    ) -> Result<Box<dyn ImmutableBlobSource + '_>, ContentStoreError> {
        Ok(Box::new(BufferedBlobSource::new(
            identity,
            self.read(identity)?,
        )))
    }

    /// Opens an incremental writer for one new blob.
    ///
    /// The identity of the blob is not known when the writer is opened: it is the
    /// SHA-256 of exactly the bytes that were accepted, reported by
    /// [`BlobWriter::finish`]. This is what lets a runtime ingest an object it can
    /// never hold in memory — a source file read directly from a remote store —
    /// while keeping the content-addressed contract intact.
    ///
    /// The default implementation buffers the whole blob in memory. That keeps every
    /// store honest about the shape of the contract, but it does not bound the
    /// working set, so a runtime that ingests objects of unknown or unbounded size
    /// must override it with a real incremental writer.
    fn writer(&self) -> Result<Box<dyn BlobWriter + '_>, ContentStoreError> {
        Ok(Box::new(BufferedBlobWriter {
            store: self,
            buffer: Vec::new(),
        }))
    }

    /// Reports the size of a stored blob without reading it.
    ///
    /// `None` means the blob is absent. The size is evidence that a blob the engine
    /// bound to a durable fact is still the blob it recorded; the bytes themselves
    /// are verified when something reads them, and a store whose cheap probe cannot
    /// answer must read the blob rather than guess.
    fn probe(&self, identity: Sha256) -> Result<Option<u64>, ContentStoreError> {
        match self.read(identity) {
            Ok(bytes) => Ok(Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX))),
            Err(ContentStoreError::Missing(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// A blob that one incremental ingest actually stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredBlob {
    identity: Sha256,
    size: u64,
}

impl StoredBlob {
    pub fn new(identity: Sha256, size: u64) -> Self {
        Self { identity, size }
    }

    /// The SHA-256 of exactly the bytes the writer accepted.
    pub fn identity(self) -> Sha256 {
        self.identity
    }

    /// How many bytes the writer accepted.
    pub fn size(self) -> u64 {
        self.size
    }
}

/// One incremental, atomically finalized ingest of a single blob.
///
/// A writer that is dropped without [`finish`](BlobWriter::finish) must leave no
/// readable blob behind: a content-addressed name may never point at bytes nobody
/// finished writing.
pub trait BlobWriter {
    /// Accepts one chunk.
    ///
    /// An implementation may buffer internally, but it must not require or assume
    /// that the whole blob is ever available at once.
    fn write(&mut self, chunk: &[u8]) -> Result<(), ContentStoreError>;

    /// Finalizes the blob and reports the identity and size of exactly the bytes
    /// that were written.
    fn finish(self: Box<Self>) -> Result<StoredBlob, ContentStoreError>;
}

/// The default writer: it works for any store, at the cost of holding the blob.
struct BufferedBlobWriter<'a, B: BlobStore + ?Sized> {
    store: &'a B,
    buffer: Vec<u8>,
}

impl<B: BlobStore + ?Sized> BlobWriter for BufferedBlobWriter<'_, B> {
    fn write(&mut self, chunk: &[u8]) -> Result<(), ContentStoreError> {
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<StoredBlob, ContentStoreError> {
        let size = u64::try_from(self.buffer.len()).unwrap_or(u64::MAX);
        let identity = self.store.store(&self.buffer)?;
        Ok(StoredBlob::new(identity, size))
    }
}

/// A borrowed blob source is a blob source.
///
/// A runtime that owns its store by value — the Git repository adapter does, so the
/// engine can never hand it repository details or large media — is built from a
/// store the composition root still owns, which is exactly a borrow of one.
impl<B: BlobStore> BlobStore for &B {
    fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
        (**self).read(identity)
    }

    fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
        (**self).store(content)
    }

    fn open(
        &self,
        identity: Sha256,
    ) -> Result<Box<dyn ImmutableBlobSource + '_>, ContentStoreError> {
        (**self).open(identity)
    }

    fn writer(&self) -> Result<Box<dyn BlobWriter + '_>, ContentStoreError> {
        (**self).writer()
    }

    fn probe(&self, identity: Sha256) -> Result<Option<u64>, ContentStoreError> {
        (**self).probe(identity)
    }
}

/// Why a blob could not be stored under, or read back by, its content identity.
#[derive(Debug)]
pub enum ContentStoreError {
    Missing(Sha256),
    Corrupt {
        expected: Sha256,
        actual: Sha256,
    },
    HashCollision(Sha256),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl ContentStoreError {
    /// Builds an I/O failure with its operation and path context.
    ///
    /// Adapters use this so their failures stay uniform across platforms.
    pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for ContentStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(identity) => write!(formatter, "content blob does not exist: {identity}"),
            Self::Corrupt { expected, actual } => write!(
                formatter,
                "content blob failed integrity verification: expected {expected}, got {actual}"
            ),
            Self::HashCollision(identity) => write!(
                formatter,
                "existing content differs despite matching SHA-256 identity: {identity}"
            ),
            Self::Io {
                operation, path, ..
            } => write!(formatter, "could not {operation}: {}", path.display()),
        }
    }
}

impl Error for ContentStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// The smallest honest store: it keeps blobs in memory and nothing else.
    #[derive(Default)]
    struct MemoryStore {
        blobs: std::cell::RefCell<HashMap<Sha256, Vec<u8>>>,
    }

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.blobs
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            let identity = Sha256::digest(content);
            self.blobs
                .borrow_mut()
                .entry(identity)
                .or_insert_with(|| content.to_vec());
            Ok(identity)
        }
    }

    #[test]
    fn the_default_writer_stores_exactly_the_bytes_it_accepted() {
        let store = MemoryStore::default();
        let mut writer = store.writer().unwrap();

        writer.write(b"he").unwrap();
        writer.write(b"llo").unwrap();
        let stored = writer.finish().unwrap();

        assert_eq!(stored.identity(), Sha256::digest(b"hello"));
        assert_eq!(stored.size(), 5);
        assert_eq!(store.read(stored.identity()).unwrap(), b"hello");
    }

    #[test]
    fn probing_reports_absence_and_size_without_interpreting_a_filename() {
        let store = MemoryStore::default();
        let identity = store.store(b"abc").unwrap();

        assert_eq!(store.probe(identity).unwrap(), Some(3));
        assert_eq!(store.probe(Sha256::digest(b"missing")).unwrap(), None);
    }
}
