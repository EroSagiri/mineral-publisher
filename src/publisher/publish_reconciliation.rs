use std::{error::Error, fmt};

use super::{
    GitCommitOid, PublishRun, PublishRunPublication, RemoteRefObservation, RemoteRefState,
};

/// A deterministic decision derived from an immutable intent and one remote fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishReconciliation {
    NoopSatisfied,
    AlreadyPublished,
    ReadyToPush {
        expected_remote_oid: GitCommitOid,
        desired_commit_oid: GitCommitOid,
    },
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
        match publish_run.publication() {
            PublishRunPublication::Noop if remote == &base => Ok(Self::NoopSatisfied),
            PublishRunPublication::Noop => Ok(Self::RemoteChanged {
                expected_base_oid: base,
                observed_oid: remote.clone(),
            }),
            PublishRunPublication::CommitReady { commit_oid } => {
                let desired = GitCommitOid::new(commit_oid.as_str())
                    .expect("PublishRun validates its desired commit identity");
                if remote == &desired {
                    Ok(Self::AlreadyPublished)
                } else if remote == &base {
                    Ok(Self::ReadyToPush {
                        expected_remote_oid: base,
                        desired_commit_oid: desired,
                    })
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
        domain::{Sha256, SnapshotId},
        publisher::{
            GitRepositoryIdentity, PublicationTarget, PublishRunId, RemoteObservationId,
            RemoteRefObservation,
        },
        workflow::ManagedRoot,
    };
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::UNIX_EPOCH,
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn run(publication: PublishRunPublication) -> PublishRun {
        let path = std::env::temp_dir().join(format!(
            "mineral-publisher-reconciliation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let run = PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            Sha256::new([1; 32]),
            ManagedRoot::new("content").unwrap(),
            GitRepositoryIdentity::new(&path).unwrap(),
            PublicationTarget::new("origin", "refs/heads/main").unwrap(),
            "a".repeat(40),
            "b".repeat(40),
            publication,
            1,
        )
        .unwrap();
        fs::remove_dir(path).unwrap();
        run
    }

    fn observation(run: &PublishRun, state: RemoteRefState) -> RemoteRefObservation {
        RemoteRefObservation::new(RemoteObservationId::new(1).unwrap(), run, state, UNIX_EPOCH)
            .unwrap()
    }

    fn present(value: char) -> RemoteRefState {
        RemoteRefState::Present {
            commit_oid: GitCommitOid::new(value.to_string().repeat(40)).unwrap(),
        }
    }

    #[test]
    fn commit_ready_reconciles_by_exact_identity() {
        let run = run(PublishRunPublication::CommitReady {
            commit_oid: "c".repeat(40),
        });
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
            PublishReconciliation::ReadyToPush {
                expected_remote_oid: GitCommitOid::new("a".repeat(40)).unwrap(),
                desired_commit_oid: GitCommitOid::new("c".repeat(40)).unwrap(),
            }
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &observation(&run, present('d'))).unwrap(),
            PublishReconciliation::RemoteChanged {
                expected_base_oid: GitCommitOid::new("a".repeat(40)).unwrap(),
                observed_oid: GitCommitOid::new("d".repeat(40)).unwrap(),
            }
        );
    }

    #[test]
    fn noop_requires_the_exact_base() {
        let run = run(PublishRunPublication::Noop);
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
        for publication in [
            PublishRunPublication::Noop,
            PublishRunPublication::CommitReady {
                commit_oid: "c".repeat(40),
            },
        ] {
            let run = run(publication);
            assert_eq!(
                PublishReconciliation::derive(&run, &observation(&run, RemoteRefState::Missing))
                    .unwrap(),
                PublishReconciliation::TargetMissing
            );
        }
    }

    #[test]
    fn reconciliation_rejects_a_different_publication_context() {
        let run = run(PublishRunPublication::Noop);
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
            PublicationTarget::new("origin", "refs/heads/other").unwrap(),
            present('a'),
            0,
        );
        assert_eq!(
            PublishReconciliation::derive(&run, &different_target),
            Err(PublishReconciliationError::TargetMismatch)
        );
    }
}
