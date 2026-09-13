use std::error::Error;

use crate::workflow::{AssetObjectKey, PublishedAsset};

use super::{AssetTargetState, ImmutableBlobSource};

/// The only way the engine touches asset storage.
///
/// Every method either reports an observed fact or performs the one side effect
/// it names. Whether an object is acceptable is the engine's decision and is
/// deliberately absent here: a runtime that implements this port may be a
/// filesystem directory today and an object store later, and neither is trusted
/// to decide whether a delivery is correct.
///
/// Two rules bind every implementation:
///
/// * The frozen object key and the frozen content type come from the value passed
///   in. An implementation must not derive a path from the logical asset path,
///   re-derive the key, guess a media type from a file name, or regenerate a
///   public URL.
/// * An object store's ETag is not a SHA-256. A target that cannot hash the bytes
///   it serves reports [`super::AssetByteIdentity::Unavailable`] or
///   [`super::AssetByteIdentity::Attested`], never an ETag dressed up as a
///   verified identity.
pub trait AssetTarget {
    type Error: Error + 'static;

    /// Reports what the target currently holds under one frozen object key.
    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error>;

    /// Places one asset's frozen representation at its frozen key.
    ///
    /// The asset carries every frozen fact (key, media type, size, identity) and
    /// `source` yields the bytes in bounded chunks, so an implementation never
    /// consults current configuration and never has to materialize the object.
    ///
    /// Two rules bind every implementation:
    ///
    /// * It must read `source` through [`ImmutableBlobSource::read_chunk`] and may
    ///   not require the whole object at once.
    /// * It must fold every chunk into an [`super::IncrementalBlobVerifier`] and
    ///   refuse to commit the object unless that verifier accepts the frozen
    ///   facts. A content-addressed key can only ever mean one thing, so an
    ///   implementation must fail closed rather than overwrite (or commit
    ///   unverified bytes over) an object it finds holding different content.
    ///
    /// Reporting success is still not evidence that the object is readable: the
    /// engine re-inspects and re-verifies before anything may depend on it.
    fn publish(
        &self,
        asset: &PublishedAsset,
        source: &mut dyn ImmutableBlobSource,
    ) -> Result<(), Self::Error>;
}
