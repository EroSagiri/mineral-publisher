//! The asset target boundary: portable asset facts, the port a runtime
//! implements to touch object storage, and the portable publication sequence.
//!
//! Only asset *identity and readiness* are modeled here. Object storage products,
//! buckets, credentials, and vendor metadata stay in the runtime adapter: the
//! engine knows a frozen object key, the frozen publication facts of the bytes
//! behind it, and whether a target's observed facts satisfy them.

mod model;
mod observation;
mod ports;
mod publish;
mod stream;

pub use model::{
    AssetByteIdentity, AssetContentError, AssetTargetConflict, AssetTargetFacts, AssetTargetState,
    AssetVerification, VerifiedAssetContent,
};
pub use observation::{
    AssetObservationId, AssetObservationIdError, AssetObservationIdGenerator,
    AssetObservationStore, AssetTargetObservation, AssetTargetObservationError,
};
pub use ports::AssetTarget;
pub use publish::{AssetPublication, AssetPublicationError, AssetPublicationOutcome};
pub use stream::{
    BufferedBlobSource, ImmutableBlobSource, IncrementalBlobVerifier, VerifiedBytesSource,
};
