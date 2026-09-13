//! Native asset-target adapters.
//!
//! The engine's asset boundary is [`mineral_core::publication::asset`]: a frozen
//! object key, frozen publication facts, and a port that reports what a target
//! holds. This module supplies the runtime side of that boundary:
//!
//! * [`ObjectStoreTransport`] — the storage seam, implemented by the native
//!   filesystem store today and by an object-storage runtime later;
//! * [`StreamingAssetTarget`] — one driver that streams a published blob in
//!   bounded chunks and commits it only after the engine's verification rule has
//!   accepted the bytes it sent;
//! * the observation allocators the composition root binds.
//!
//! A Cloudflare or object-storage runtime implements the same transport trait;
//! nothing in the engine changes when it does.

mod filesystem;
pub mod r2;
mod streaming;
mod transport;

use std::{error::Error, fmt, path::Path, path::PathBuf};

use crate::workflow::{AssetObjectKey, PublishedAsset};
pub use filesystem::{FilesystemObjectStore, FilesystemObjectStoreError};
pub use mineral_core::publication::asset::{
    AssetByteIdentity, AssetContentError, AssetObservationId, AssetObservationIdError,
    AssetObservationIdGenerator, AssetObservationStore, AssetPublication, AssetPublicationError,
    AssetPublicationOutcome, AssetTarget, AssetTargetConflict, AssetTargetFacts,
    AssetTargetObservation, AssetTargetObservationError, AssetTargetState, AssetVerification,
    BufferedBlobSource, ImmutableBlobSource, IncrementalBlobVerifier, VerifiedAssetContent,
    VerifiedBytesSource,
};
pub use r2::{R2ObjectStore, R2ObjectStoreConfig, R2ObjectStoreError, R2SecretKey};
pub use streaming::{DEFAULT_CHUNK_SIZE, StreamingAssetTarget, StreamingAssetTargetError};
pub use transport::ObjectStoreTransport;
pub use transport::ObjectWriter;

/// The native asset target: a streaming driver over a filesystem object store.
pub type FilesystemAssetTarget = StreamingAssetTarget<FilesystemObjectStore>;

impl StreamingAssetTarget<FilesystemObjectStore> {
    /// Opens the native target over one object-store root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_transport(FilesystemObjectStore::new(root))
    }

    pub fn root(&self) -> &Path {
        self.transport().root()
    }
}

/// A deterministic caller-owned asset-observation allocator for tests.
#[derive(Clone, Debug)]
pub struct SequentialAssetObservationIdGenerator {
    next: Option<u64>,
}

impl SequentialAssetObservationIdGenerator {
    pub fn new(first: AssetObservationId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl AssetObservationIdGenerator for SequentialAssetObservationIdGenerator {
    type Error = SequentialAssetObservationIdGeneratorError;

    fn next_id(&mut self) -> Result<AssetObservationId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialAssetObservationIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        AssetObservationId::new(value)
            .map_err(|_| SequentialAssetObservationIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialAssetObservationIdGeneratorError {
    Exhausted,
}

impl std::fmt::Display for SequentialAssetObservationIdGeneratorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("asset observation ID sequence is exhausted")
    }
}

impl Error for SequentialAssetObservationIdGeneratorError {}

/// Production asset-observation allocator backed by UUID v4 entropy.
///
/// The durable schema uses positive `u64` ids, so a UUID v4 is mapped to its
/// first non-zero 63 bits, exactly like the publish-run and remote-observation
/// allocators.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidAssetObservationIdGenerator;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UuidAssetObservationIdGeneratorError {
    InvalidGeneratedId,
}

impl std::fmt::Display for UuidAssetObservationIdGeneratorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UUID-based asset observation ID was invalid")
    }
}

impl Error for UuidAssetObservationIdGeneratorError {}

impl AssetObservationIdGenerator for UuidAssetObservationIdGenerator {
    type Error = UuidAssetObservationIdGeneratorError;

    fn next_id(&mut self) -> Result<AssetObservationId, Self::Error> {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&bytes[..8]);
        let value = (u64::from_be_bytes(prefix) & i64::MAX as u64).max(1);
        AssetObservationId::new(value)
            .map_err(|_| UuidAssetObservationIdGeneratorError::InvalidGeneratedId)
    }
}

/// The asset target a workspace publishes to.
///
/// The engine accepts any [`AssetTarget`], so which store a delivery lands in is
/// a composition-root decision. This enum keeps that decision in one place so the
/// rest of the host names a single target type, and so a workspace that names no
/// target — or two — is refused before any publication begins.
pub enum ConfiguredAssetTarget {
    /// The native store: objects under a directory on this machine.
    Filesystem(FilesystemAssetTarget),
    /// An S3-compatible bucket reached over HTTPS.
    R2(StreamingAssetTarget<R2ObjectStore>),
}

impl ConfiguredAssetTarget {
    pub fn filesystem(root: impl Into<PathBuf>) -> Self {
        Self::Filesystem(FilesystemAssetTarget::new(root))
    }

    pub fn r2(store: R2ObjectStore) -> Self {
        Self::R2(StreamingAssetTarget::with_transport(store))
    }

    /// Where this target puts objects, for a publication report. It names the
    /// location only: no credential is reachable from it.
    pub fn description(&self) -> String {
        match self {
            Self::Filesystem(target) => format!("filesystem:{}", target.root().display()),
            Self::R2(target) => format!("r2:{}", target.transport().describe()),
        }
    }
}

impl AssetTarget for ConfiguredAssetTarget {
    type Error = ConfiguredAssetTargetError;

    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error> {
        match self {
            Self::Filesystem(target) => target
                .inspect(object_key)
                .map_err(ConfiguredAssetTargetError::Filesystem),
            Self::R2(target) => target
                .inspect(object_key)
                .map_err(ConfiguredAssetTargetError::R2),
        }
    }

    fn publish(
        &self,
        asset: &PublishedAsset,
        source: &mut dyn ImmutableBlobSource,
    ) -> Result<(), Self::Error> {
        match self {
            Self::Filesystem(target) => target
                .publish(asset, source)
                .map_err(ConfiguredAssetTargetError::Filesystem),
            Self::R2(target) => target
                .publish(asset, source)
                .map_err(ConfiguredAssetTargetError::R2),
        }
    }
}

/// Why the configured target could not report or place an object.
///
/// The two stores have different failure vocabularies, and this keeps them apart:
/// a caller can still tell a filesystem conflict from a bucket conflict.
#[derive(Debug)]
pub enum ConfiguredAssetTargetError {
    Filesystem(StreamingAssetTargetError<FilesystemObjectStoreError>),
    R2(StreamingAssetTargetError<R2ObjectStoreError>),
}

impl fmt::Display for ConfiguredAssetTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filesystem(error) => write!(formatter, "filesystem asset target failed: {error}"),
            Self::R2(error) => write!(formatter, "object storage asset target failed: {error}"),
        }
    }
}

impl Error for ConfiguredAssetTargetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Filesystem(error) => Some(error),
            Self::R2(error) => Some(error),
        }
    }
}
