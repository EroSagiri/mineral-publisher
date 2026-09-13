use std::error::Error;

use crate::{
    asset::AssetTargetState,
    workflow::{AssetObjectKey, PublishedAsset},
};

/// One non-overwriting, streaming destination for one object.
///
/// The engine's runtime driver writes a published blob into a writer chunk by
/// chunk and only calls [`finish`](ObjectWriter::finish) after the engine's own
/// verification rule has accepted the bytes it sent. Dropping a writer without
/// finishing it must abort the write: a content-addressed key may never receive
/// bytes nobody verified, so "commit" has to be a separate, explicit step.
pub trait ObjectWriter {
    type Error: Error + 'static;

    /// Sends one chunk. An implementation may buffer internally, but it must not
    /// require or assume that the whole object is ever available at once.
    fn write(&mut self, chunk: &[u8]) -> Result<(), Self::Error>;

    /// Commits the object.
    ///
    /// Reporting success is not evidence that the object is readable or correct:
    /// the engine re-inspects the target and judges the facts it observes.
    fn finish(self: Box<Self>) -> Result<(), Self::Error>;
}

/// The runtime side of asset storage: an object store, seen as facts plus a
/// streaming writer.
///
/// A transport reports what it holds and performs the one write it is asked for.
/// It never decides whether an asset is ready, never derives an object key or a
/// media type from anything but the frozen asset it is handed, and never turns a
/// transport digest (an ETag, a checksum header) into a byte identity: only bytes
/// the transport itself hashed may be reported as
/// [`crate::asset::AssetByteIdentity::Verified`].
///
/// Implementations exist for the native filesystem today; an object-storage
/// runtime implements the same trait over its own API.
pub trait ObjectStoreTransport {
    type Error: Error + 'static;

    /// Reports the facts this store has for one object.
    ///
    /// The byte identity must come from the object's own bytes. A store that can
    /// only offer a claim reports
    /// [`crate::asset::AssetByteIdentity::Attested`] or `Unavailable`, and the
    /// engine will refuse to authorize a publication on it.
    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error>;

    /// Opens a streaming writer for one frozen object.
    ///
    /// The object key, the media type and the expected size all come from `asset`;
    /// nothing is re-derived. An object already present under that key is never
    /// overwritten: the writer either becomes a no-op that requires an exact
    /// match, or the call fails.
    fn open_writer(
        &self,
        asset: &PublishedAsset,
    ) -> Result<Box<dyn ObjectWriter<Error = Self::Error> + '_>, Self::Error>;
}

/// A borrowed transport is a transport.
impl<T: ObjectStoreTransport> ObjectStoreTransport for &T {
    type Error = T::Error;

    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error> {
        (**self).inspect(object_key)
    }

    fn open_writer(
        &self,
        asset: &PublishedAsset,
    ) -> Result<Box<dyn ObjectWriter<Error = Self::Error> + '_>, Self::Error> {
        (**self).open_writer(asset)
    }
}
