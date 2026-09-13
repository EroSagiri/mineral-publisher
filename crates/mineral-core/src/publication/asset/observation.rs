use std::{error::Error, fmt};

use crate::{
    domain::{ContentPath, Sha256, TimestampMillis},
    publish::{PublishRun, PublishRunId},
    workflow::{AssetObjectKey, PublishedAsset},
};

use super::AssetTargetState;

/// Stable identity for one immutable observation of an asset target.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AssetObservationId(u64);

impl AssetObservationId {
    pub fn new(value: u64) -> Result<Self, AssetObservationIdError> {
        if value == 0 {
            return Err(AssetObservationIdError::Zero);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetObservationIdError {
    Zero,
}

impl fmt::Display for AssetObservationIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset observation ID must be positive")
    }
}

impl Error for AssetObservationIdError {}

/// An immutable, time-stamped asset-target fact scoped to one publication intent.
///
/// This is the audit record that authorizes (or refuses) the next side effect, so
/// it binds the run, the exact delivery projection it belongs to, and the exact
/// frozen asset it was judged against. It carries no runtime handle: an object
/// key and observed facts are portable, a bucket or an ETag is not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetTargetObservation {
    id: AssetObservationId,
    publish_run_id: PublishRunId,
    delivery_projection_sha256: Sha256,
    logical_path: ContentPath,
    object_key: AssetObjectKey,
    observed: AssetTargetState,
    observed_at_unix_ms: u64,
}

impl AssetTargetObservation {
    /// Builds the audit fact for one inspected asset of one exact delivery.
    pub fn new(
        id: AssetObservationId,
        publish_run: &PublishRun,
        delivery_projection_sha256: Sha256,
        asset: &PublishedAsset,
        observed: AssetTargetState,
        observed_at: TimestampMillis,
    ) -> Result<Self, AssetTargetObservationError> {
        Self::from_parts(
            id,
            publish_run.id(),
            delivery_projection_sha256,
            asset.logical_path().clone(),
            asset.object_key().clone(),
            observed,
            observed_at.as_unix_millis(),
        )
    }

    /// Runtime-side constructor for a persisted observation.
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate(
        id: AssetObservationId,
        publish_run_id: PublishRunId,
        delivery_projection_sha256: Sha256,
        logical_path: ContentPath,
        object_key: AssetObjectKey,
        observed: AssetTargetState,
        observed_at_unix_ms: u64,
    ) -> Result<Self, AssetTargetObservationError> {
        Self::from_parts(
            id,
            publish_run_id,
            delivery_projection_sha256,
            logical_path,
            object_key,
            observed,
            observed_at_unix_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        id: AssetObservationId,
        publish_run_id: PublishRunId,
        delivery_projection_sha256: Sha256,
        logical_path: ContentPath,
        object_key: AssetObjectKey,
        observed: AssetTargetState,
        observed_at_unix_ms: u64,
    ) -> Result<Self, AssetTargetObservationError> {
        // An observation about one object key cannot carry facts about another:
        // that would let a mismatched row launder itself into a consistent one on
        // the way out of a store.
        if let AssetTargetState::Present(facts) = &observed
            && facts.object_key() != &object_key
        {
            return Err(AssetTargetObservationError::ObservedObjectKeyMismatch {
                object_key,
                observed_object_key: facts.object_key().clone(),
            });
        }
        Ok(Self {
            id,
            publish_run_id,
            delivery_projection_sha256,
            logical_path,
            object_key,
            observed,
            observed_at_unix_ms,
        })
    }

    pub fn id(&self) -> AssetObservationId {
        self.id
    }
    pub fn publish_run_id(&self) -> PublishRunId {
        self.publish_run_id
    }
    /// The exact delivery projection this fact was observed for.
    pub fn delivery_projection_sha256(&self) -> Sha256 {
        self.delivery_projection_sha256
    }
    pub fn logical_path(&self) -> &ContentPath {
        &self.logical_path
    }
    pub fn object_key(&self) -> &AssetObjectKey {
        &self.object_key
    }
    pub fn observed(&self) -> &AssetTargetState {
        &self.observed
    }
    pub fn observed_at(&self) -> TimestampMillis {
        TimestampMillis::from_unix_millis(self.observed_at_unix_ms)
    }
    pub fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }
}

/// Why one observed fact set cannot be recorded as an asset-target observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetTargetObservationError {
    /// The observed facts describe another object than the one being observed.
    ObservedObjectKeyMismatch {
        object_key: AssetObjectKey,
        observed_object_key: AssetObjectKey,
    },
}

impl fmt::Display for AssetTargetObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservedObjectKeyMismatch { object_key, .. } => write!(
                formatter,
                "observed facts describe another object than the frozen key {object_key}"
            ),
        }
    }
}

impl Error for AssetTargetObservationError {}

/// Append-only persistence for asset-target observations.
///
/// The store is an audit trail, not a cache: an attempt always re-observes the
/// target before it acts, so a historical row can never authorize a side effect
/// on its own.
pub trait AssetObservationStore {
    type Error: Error;

    fn save(&self, observation: &AssetTargetObservation) -> Result<(), Self::Error>;
    fn get(&self, id: AssetObservationId) -> Result<Option<AssetTargetObservation>, Self::Error>;
    fn list_for_publish_run(
        &self,
        publish_run_id: PublishRunId,
    ) -> Result<Vec<AssetTargetObservation>, Self::Error>;
}

/// Allocates immutable asset-observation identities at the publication boundary.
pub trait AssetObservationIdGenerator {
    type Error: Error;

    fn next_id(&mut self) -> Result<AssetObservationId, Self::Error>;
}
