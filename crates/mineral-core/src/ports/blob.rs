use std::{error::Error, fmt, io, path::PathBuf};

use crate::domain::Sha256;

/// Content-addressed blob access required by the portable engine.
///
/// One implementation backs the native filesystem CAS; the Cloudflare adapter
/// provides an R2-backed implementation later. `read` must return exactly the
/// bytes whose SHA-256 equals `identity`, and `store` must never replace an
/// existing blob with different bytes.
pub trait BlobStore {
    fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError>;
    fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError>;
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
