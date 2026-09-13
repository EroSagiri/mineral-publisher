//! Adapter conformance tests for the portable publish reconciliation engine.
//!
//! Reconciliation itself is pure. These tests keep the original verification
//! that built an intent from a real canonical repository locator (which requires
//! the native filesystem), so they live in the host crate alongside the other
//! adapter conformance tests. The pure decision matrix is covered by the
//! portable tests in `mineral-core/src/publish/reconciliation.rs`.

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        domain::{Sha256, SnapshotId, TimestampMillis},
        publisher::{
            GitCommitOid, GitRefTarget, GitRepositoryIdentity, PublishReconciliation,
            PublishReconciliationError, PublishRun, PublishRunId, PublishTargetId,
            RemoteObservationId, RemoteRefObservation, RemoteRefState,
        },
        workflow::ManagedRoot,
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn run(desired_commit: Option<GitCommitOid>) -> PublishRun {
        let path = std::env::temp_dir().join(format!(
            "mineral-publisher-reconciliation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let run = PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            Some(Sha256::new([1; 32])),
            None,
            None,
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            GitRepositoryIdentity::new(&path).unwrap().locator().clone(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            "a".repeat(40),
            "b".repeat(40),
            desired_commit,
            None,
            1,
        )
        .unwrap();
        fs::remove_dir(path).unwrap();
        run
    }

    fn oid(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
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
        let PublishReconciliation::ReadyToPush(ready) =
            PublishReconciliation::derive(&run, &observation(&run, present('a'))).unwrap()
        else {
            panic!("expected a ready-to-push decision");
        };
        assert_eq!(ready.publish_run_id(), run.id());
        assert_eq!(ready.target(), run.target());
        assert_eq!(
            ready.source_observation_id(),
            RemoteObservationId::new(1).unwrap()
        );
        assert_eq!(ready.expected_remote_oid(), &oid('a'));
        assert_eq!(ready.desired_commit_oid(), &oid('c'));
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
}
