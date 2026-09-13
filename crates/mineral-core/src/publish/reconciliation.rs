use std::{error::Error, fmt};

use crate::publication::git::{GitCommitOid, GitRefTarget, RemoteRefState};

use super::{PublishRun, PublishRunId, RemoteObservationId, RemoteRefObservation};

/// A capability produced only by reconciliation for one exact publication intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyToPush {
    publish_run_id: PublishRunId,
    target: GitRefTarget,
    source_observation_id: RemoteObservationId,
    expected_remote_oid: GitCommitOid,
    desired_commit_oid: GitCommitOid,
}

impl ReadyToPush {
    pub fn publish_run_id(&self) -> PublishRunId {
        self.publish_run_id
    }

    pub fn target(&self) -> &GitRefTarget {
        &self.target
    }

    pub fn source_observation_id(&self) -> RemoteObservationId {
        self.source_observation_id
    }

    pub fn expected_remote_oid(&self) -> &GitCommitOid {
        &self.expected_remote_oid
    }

    pub fn desired_commit_oid(&self) -> &GitCommitOid {
        &self.desired_commit_oid
    }
}

/// A deterministic decision derived from an immutable intent and one remote fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishReconciliation {
    NoopSatisfied,
    AlreadyPublished,
    ReadyToPush(ReadyToPush),
    RemoteChanged {
        expected_base_oid: GitCommitOid,
        observed_oid: GitCommitOid,
    },
    TargetMissing,
}

impl PublishReconciliation {
    pub fn derive(
        publish_run: &PublishRun,
        observation: &RemoteRefObservation,
    ) -> Result<Self, PublishReconciliationError> {
        if observation.publish_run_id() != publish_run.id() {
            return Err(PublishReconciliationError::PublishRunMismatch);
        }
        if observation.target() != publish_run.target() {
            return Err(PublishReconciliationError::TargetMismatch);
        }
        let base = GitCommitOid::new(publish_run.base_commit())
            .expect("PublishRun validates its base commit identity");
        let RemoteRefState::Present { commit_oid: remote } = observation.observed() else {
            return Ok(Self::TargetMissing);
        };
        match publish_run.desired_commit() {
            None if remote == &base => Ok(Self::NoopSatisfied),
            None => Ok(Self::RemoteChanged {
                expected_base_oid: base,
                observed_oid: remote.clone(),
            }),
            Some(desired) => {
                if remote == desired {
                    Ok(Self::AlreadyPublished)
                } else if remote == &base {
                    Ok(Self::ReadyToPush(ReadyToPush {
                        publish_run_id: publish_run.id(),
                        target: publish_run.target().clone(),
                        source_observation_id: observation.id(),
                        expected_remote_oid: base,
                        desired_commit_oid: desired.clone(),
                    }))
                } else {
                    Ok(Self::RemoteChanged {
                        expected_base_oid: base,
                        observed_oid: remote.clone(),
                    })
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishReconciliationError {
    PublishRunMismatch,
    TargetMismatch,
}

impl fmt::Display for PublishReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublishRunMismatch => {
                formatter.write_str("remote observation belongs to another publish run")
            }
            Self::TargetMismatch => {
                formatter.write_str("remote observation target does not match publish run target")
            }
        }
    }
}

impl Error for PublishReconciliationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Sha256, SnapshotId, TimestampMillis},
        publish::{PublishTargetId, RepositoryLocator},
        workflow::ManagedRoot,
    };

    fn oid(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn run(desired_commit: Option<GitCommitOid>) -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            Some(Sha256::new([1; 32])),
            None,
            None,
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            "a".repeat(40),
            "b".repeat(40),
            desired_commit,
            None,
            1,
        )
        .unwrap()
    }

    fn observation(run: &PublishRun, state: RemoteRefState) -> RemoteRefObservation {
        RemoteRefObservation::new(
            RemoteObservationId::new(1).unwrap(),
            run,
            state,
            TimestampMillis::UNIX_EPOCH,
        )
    }

    fn present(value: char) -> RemoteRefState {
        RemoteRefState::Present {
            commit_oid: oid(value),
        }
    }

    #[test]
    fn commit_ready_reconciles_by_exact_identity() {
        let run = run(Some(oid('c')));
        let published_observation = observation(&run, present('c'));
        assert_eq!(
            PublishReconciliation::derive(&run, &published_observation).unwrap(),
            PublishReconciliation::AlreadyPublished
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &published_observation),
            PublishReconciliation::derive(&run, &published_observation)
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &observation(&run, present('a'))).unwrap(),
            PublishReconciliation::ReadyToPush(ReadyToPush {
                publish_run_id: run.id(),
                target: run.target().clone(),
                source_observation_id: RemoteObservationId::new(1).unwrap(),
                expected_remote_oid: oid('a'),
                desired_commit_oid: oid('c'),
            })
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &observation(&run, present('d'))).unwrap(),
            PublishReconciliation::RemoteChanged {
                expected_base_oid: oid('a'),
                observed_oid: oid('d'),
            }
        );
    }

    #[test]
    fn noop_requires_the_exact_base() {
        let run = run(None);
        assert_eq!(
            PublishReconciliation::derive(&run, &observation(&run, present('a'))).unwrap(),
            PublishReconciliation::NoopSatisfied
        );
        assert!(matches!(
            PublishReconciliation::derive(&run, &observation(&run, present('d'))).unwrap(),
            PublishReconciliation::RemoteChanged { .. }
        ));
    }

    #[test]
    fn missing_target_fails_closed_for_both_publication_kinds() {
        for desired_commit in [None, Some(oid('c'))] {
            let run = run(desired_commit);
            assert_eq!(
                PublishReconciliation::derive(&run, &observation(&run, RemoteRefState::Missing))
                    .unwrap(),
                PublishReconciliation::TargetMissing
            );
        }
    }

    #[test]
    fn reconciliation_rejects_a_different_publication_context() {
        let run = run(None);
        let different_run = RemoteRefObservation::rehydrate(
            RemoteObservationId::new(1).unwrap(),
            PublishRunId::new(2).unwrap(),
            run.target().clone(),
            present('a'),
            0,
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &different_run),
            Err(PublishReconciliationError::PublishRunMismatch)
        );
        let different_target = RemoteRefObservation::rehydrate(
            RemoteObservationId::new(1).unwrap(),
            run.id(),
            GitRefTarget::new("origin", "refs/heads/other").unwrap(),
            present('a'),
            0,
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &different_target),
            Err(PublishReconciliationError::TargetMismatch)
        );
    }

    #[test]
    fn ready_to_push_is_bound_to_the_observation_it_came_from() {
        let run = run(Some(oid('c')));
        let source = RemoteRefObservation::new(
            RemoteObservationId::new(7).unwrap(),
            &run,
            present('a'),
            TimestampMillis::from_unix_millis(5),
        );
        let PublishReconciliation::ReadyToPush(ready) =
            PublishReconciliation::derive(&run, &source).unwrap()
        else {
            panic!("expected a ready-to-push decision");
        };

        assert_eq!(ready.publish_run_id(), run.id());
        assert_eq!(ready.target(), run.target());
        assert_eq!(ready.source_observation_id(), source.id());
        assert_eq!(ready.expected_remote_oid(), &oid('a'));
        assert_eq!(ready.desired_commit_oid(), &oid('c'));
    }
}
