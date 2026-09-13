use std::{error::Error, fmt};

use crate::{
    domain::TimestampMillis,
    publication::git::{GitRefTarget, RemoteRefState},
};

use super::{PublishRun, PublishRunId};

/// Stable identity for one immutable observation of a publication target.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RemoteObservationId(u64);

impl RemoteObservationId {
    pub fn new(value: u64) -> Result<Self, RemoteObservationIdError> {
        if value == 0 {
            return Err(RemoteObservationIdError::Zero);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteObservationIdError {
    Zero,
}

impl fmt::Display for RemoteObservationIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote observation ID must be positive")
    }
}

impl Error for RemoteObservationIdError {}

/// An immutable, time-stamped remote fact scoped to one publication intent.
///
/// A runtime reports the bare ref state it observed; the engine assembles this
/// audit record around it and freezes the time it was taken.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRefObservation {
    id: RemoteObservationId,
    publish_run_id: PublishRunId,
    target: GitRefTarget,
    observed: RemoteRefState,
    observed_at_unix_ms: u64,
}

impl RemoteRefObservation {
    /// Builds an audit fact from an observed ref state. The observation time is
    /// already frozen, so this cannot fail on an unusable clock reading.
    pub fn new(
        id: RemoteObservationId,
        publish_run: &PublishRun,
        observed: RemoteRefState,
        observed_at: TimestampMillis,
    ) -> Self {
        Self::from_parts(
            id,
            publish_run.id(),
            publish_run.target().clone(),
            observed,
            observed_at.as_unix_millis(),
        )
    }

    /// Runtime-side constructor for a persisted observation.
    pub fn rehydrate(
        id: RemoteObservationId,
        publish_run_id: PublishRunId,
        target: GitRefTarget,
        observed: RemoteRefState,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self::from_parts(id, publish_run_id, target, observed, observed_at_unix_ms)
    }

    fn from_parts(
        id: RemoteObservationId,
        publish_run_id: PublishRunId,
        target: GitRefTarget,
        observed: RemoteRefState,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self {
            id,
            publish_run_id,
            target,
            observed,
            observed_at_unix_ms,
        }
    }

    pub fn id(&self) -> RemoteObservationId {
        self.id
    }
    pub fn publish_run_id(&self) -> PublishRunId {
        self.publish_run_id
    }
    pub fn target(&self) -> &GitRefTarget {
        &self.target
    }
    pub fn observed(&self) -> &RemoteRefState {
        &self.observed
    }
    pub fn observed_at(&self) -> TimestampMillis {
        TimestampMillis::from_unix_millis(self.observed_at_unix_ms)
    }
    pub fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }
}

pub trait RemoteObservationStore {
    type Error: Error;

    fn save(&self, observation: &RemoteRefObservation) -> Result<(), Self::Error>;
    fn get(&self, id: RemoteObservationId) -> Result<Option<RemoteRefObservation>, Self::Error>;
    fn list_for_publish_run(
        &self,
        publish_run_id: PublishRunId,
    ) -> Result<Vec<RemoteRefObservation>, Self::Error>;
}

/// Allocates immutable remote-observation identities at the publication boundary.
///
/// An observation is an append-only audit fact, so its identity must never be
/// reused; the engine decides when one is needed and the runtime decides how one
/// is minted.
pub trait RemoteObservationIdGenerator {
    type Error: Error;

    fn next_id(&mut self) -> Result<RemoteObservationId, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Sha256, SnapshotId},
        publication::git::GitCommitOid,
        publish::{PublishTargetId, RepositoryLocator},
        workflow::ManagedRoot,
    };

    fn run() -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(3).unwrap(),
            SnapshotId::new(1).unwrap(),
            Sha256::new([1; 32]),
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            "a".repeat(40),
            "b".repeat(40),
            None,
            None,
            1,
        )
        .unwrap()
    }

    #[test]
    fn observation_binds_the_run_target_and_frozen_time() {
        let run = run();
        let observation = RemoteRefObservation::new(
            RemoteObservationId::new(9).unwrap(),
            &run,
            RemoteRefState::Present {
                commit_oid: GitCommitOid::new("c".repeat(40)).unwrap(),
            },
            TimestampMillis::from_unix_millis(1_500),
        );

        assert_eq!(observation.id(), RemoteObservationId::new(9).unwrap());
        assert_eq!(observation.publish_run_id(), run.id());
        assert_eq!(observation.target(), run.target());
        assert_eq!(observation.observed_at().as_unix_millis(), 1_500);
        assert_eq!(observation.observed_at_unix_ms(), 1_500);
    }

    #[test]
    fn rehydrated_observation_keeps_the_persisted_fact() {
        let observation = RemoteRefObservation::rehydrate(
            RemoteObservationId::new(2).unwrap(),
            PublishRunId::new(3).unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            RemoteRefState::Missing,
            42,
        );

        assert_eq!(observation.observed(), &RemoteRefState::Missing);
        assert_eq!(observation.observed_at_unix_ms(), 42);
        assert_eq!(
            RemoteRefObservation::new(
                observation.id(),
                &run(),
                RemoteRefState::Missing,
                TimestampMillis::from_unix_millis(42),
            ),
            observation
        );
    }

    #[test]
    fn observation_ids_must_be_positive() {
        assert_eq!(
            RemoteObservationId::new(0),
            Err(RemoteObservationIdError::Zero)
        );
    }
}
