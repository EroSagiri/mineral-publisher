use std::{error::Error, fmt, process::Command, time::SystemTime};

use super::{
    GitCommitOid, GitRemoteObservationError, GitRemoteObserver, PublishReconciliation,
    PublishReconciliationError, PublishRun, PublishRunPublication, ReadyToPush,
    RemoteObservationId, RemoteObservationStore, RemoteRefObservation,
};

/// The process-level outcome of the push command. It is diagnostic context, not remote truth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitPushCommandOutcome {
    Succeeded,
    Failed { status: Option<i32> },
}

/// Final execution state derived from a fresh remote observation after one push attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitPushExecutionResult {
    Published {
        observation: RemoteRefObservation,
        command_outcome: GitPushCommandOutcome,
    },
    PushFailedButRemoteUnchanged {
        observation: RemoteRefObservation,
        status: Option<i32>,
    },
    RemoteUnchangedAfterSuccessfulPush {
        observation: RemoteRefObservation,
    },
    RemoteChanged {
        observation: RemoteRefObservation,
        expected_base_oid: GitCommitOid,
        observed_oid: GitCommitOid,
        command_outcome: GitPushCommandOutcome,
    },
    TargetMissing {
        observation: RemoteRefObservation,
        command_outcome: GitPushCommandOutcome,
    },
    Indeterminate {
        command_outcome: GitPushCommandOutcome,
        observation_error: GitRemoteObservationError,
    },
}

/// Executes exactly one compare-and-swap push, then treats a new remote observation as truth.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitCompareAndPushExecutor;

impl GitCompareAndPushExecutor {
    pub fn execute<S: RemoteObservationStore>(
        publish_run: &PublishRun,
        ready: &ReadyToPush,
        post_push_observation_id: RemoteObservationId,
        observed_at: SystemTime,
        observation_store: &S,
    ) -> Result<GitPushExecutionResult, GitPushExecutionError<S::Error>> {
        Self::validate(publish_run, ready, post_push_observation_id)?;
        verify_desired_commit(publish_run, ready)?;

        Self::execute_with(
            publish_run,
            ready,
            observation_store,
            || run_exact_cas_push(publish_run, ready),
            || GitRemoteObserver::observe(post_push_observation_id, publish_run, observed_at),
        )
    }

    fn validate<E: Error>(
        publish_run: &PublishRun,
        ready: &ReadyToPush,
        post_push_observation_id: RemoteObservationId,
    ) -> Result<(), GitPushExecutionError<E>> {
        if ready.publish_run_id() != publish_run.id() || ready.target() != publish_run.target() {
            return Err(GitPushExecutionError::ReadyToPushBindingMismatch);
        }
        if ready.expected_remote_oid().as_str() != publish_run.base_commit() {
            return Err(GitPushExecutionError::ExpectedRemoteMismatch);
        }
        if ready.source_observation_id() == post_push_observation_id {
            return Err(GitPushExecutionError::PostObservationIdReused);
        }
        match publish_run.publication() {
            PublishRunPublication::CommitReady { commit_oid }
                if commit_oid == ready.desired_commit_oid().as_str() =>
            {
                Ok(())
            }
            PublishRunPublication::CommitReady { .. } => {
                Err(GitPushExecutionError::DesiredCommitMismatch)
            }
            PublishRunPublication::Noop => Err(GitPushExecutionError::NoopCannotBePushed),
        }
    }

    fn execute_with<S, P, O>(
        publish_run: &PublishRun,
        _ready: &ReadyToPush,
        observation_store: &S,
        push: P,
        observe: O,
    ) -> Result<GitPushExecutionResult, GitPushExecutionError<S::Error>>
    where
        S: RemoteObservationStore,
        P: FnOnce() -> GitPushCommandOutcome,
        O: FnOnce() -> Result<RemoteRefObservation, GitRemoteObservationError>,
    {
        let command_outcome = push();
        let observation = match observe() {
            Ok(observation) => observation,
            Err(observation_error) => {
                return Ok(GitPushExecutionResult::Indeterminate {
                    command_outcome,
                    observation_error,
                });
            }
        };
        observation_store
            .save(&observation)
            .map_err(GitPushExecutionError::ObservationPersistence)?;

        match PublishReconciliation::derive(publish_run, &observation)
            .map_err(GitPushExecutionError::Reconciliation)?
        {
            PublishReconciliation::AlreadyPublished => Ok(GitPushExecutionResult::Published {
                observation,
                command_outcome,
            }),
            PublishReconciliation::ReadyToPush(_) => match command_outcome {
                GitPushCommandOutcome::Failed { status } => {
                    Ok(GitPushExecutionResult::PushFailedButRemoteUnchanged {
                        observation,
                        status,
                    })
                }
                GitPushCommandOutcome::Succeeded => {
                    Ok(GitPushExecutionResult::RemoteUnchangedAfterSuccessfulPush { observation })
                }
            },
            PublishReconciliation::RemoteChanged {
                expected_base_oid,
                observed_oid,
            } => Ok(GitPushExecutionResult::RemoteChanged {
                observation,
                expected_base_oid,
                observed_oid,
                command_outcome,
            }),
            PublishReconciliation::TargetMissing => Ok(GitPushExecutionResult::TargetMissing {
                observation,
                command_outcome,
            }),
            PublishReconciliation::NoopSatisfied => {
                Err(GitPushExecutionError::UnexpectedNoopReconciliation)
            }
        }
    }
}

fn verify_desired_commit<E: Error>(
    publish_run: &PublishRun,
    ready: &ReadyToPush,
) -> Result<(), GitPushExecutionError<E>> {
    let repository = publish_run.repository().path();
    let repository_check = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map_err(|_| GitPushExecutionError::GitUnavailable)?;
    if !repository_check.status.success() {
        return Err(GitPushExecutionError::RepositoryUnavailable);
    }

    let commit_expression = format!("{}^{{commit}}", ready.desired_commit_oid().as_str());
    let object_check = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "-e", commit_expression.as_str()])
        .output()
        .map_err(|_| GitPushExecutionError::GitUnavailable)?;
    if !object_check.status.success() {
        return Err(GitPushExecutionError::DesiredCommitUnavailable);
    }

    let tree_expression = format!("{}^{{tree}}", ready.desired_commit_oid().as_str());
    let tree = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", tree_expression.as_str()])
        .output()
        .map_err(|_| GitPushExecutionError::GitUnavailable)?;
    if !tree.status.success()
        || std::str::from_utf8(&tree.stdout)
            .map(str::trim)
            .ok()
            .is_none_or(|tree_oid| tree_oid != publish_run.reviewed_tree())
    {
        return Err(GitPushExecutionError::DesiredCommitTreeMismatch);
    }
    Ok(())
}

fn run_exact_cas_push(publish_run: &PublishRun, ready: &ReadyToPush) -> GitPushCommandOutcome {
    let target = publish_run.target();
    let lease = format!(
        "--force-with-lease={}:{}",
        target.destination_ref(),
        ready.expected_remote_oid().as_str()
    );
    let refspec = format!(
        "{}:{}",
        ready.desired_commit_oid().as_str(),
        target.destination_ref()
    );
    match Command::new("git")
        .current_dir(publish_run.repository().path())
        .arg("push")
        .arg(lease)
        .arg("--")
        .arg(target.remote_name())
        .arg(refspec)
        .output()
    {
        Ok(output) if output.status.success() => GitPushCommandOutcome::Succeeded,
        Ok(output) => GitPushCommandOutcome::Failed {
            status: output.status.code(),
        },
        Err(_) => GitPushCommandOutcome::Failed { status: None },
    }
}

#[derive(Debug)]
pub enum GitPushExecutionError<E: Error> {
    ReadyToPushBindingMismatch,
    ExpectedRemoteMismatch,
    PostObservationIdReused,
    DesiredCommitMismatch,
    NoopCannotBePushed,
    RepositoryUnavailable,
    GitUnavailable,
    DesiredCommitUnavailable,
    DesiredCommitTreeMismatch,
    ObservationPersistence(E),
    Reconciliation(PublishReconciliationError),
    UnexpectedNoopReconciliation,
}

impl<E: Error> fmt::Display for GitPushExecutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadyToPushBindingMismatch => {
                formatter.write_str("ReadyToPush belongs to another publish run or target")
            }
            Self::ExpectedRemoteMismatch => {
                formatter.write_str("ReadyToPush expected OID does not match publish run base")
            }
            Self::PostObservationIdReused => {
                formatter.write_str("post-push observation must use a new observation ID")
            }
            Self::DesiredCommitMismatch => {
                formatter.write_str("ReadyToPush desired OID does not match publish run commit")
            }
            Self::NoopCannotBePushed => formatter.write_str("a noop publish run cannot be pushed"),
            Self::RepositoryUnavailable => {
                formatter.write_str("Git repository is unavailable or invalid")
            }
            Self::GitUnavailable => formatter.write_str("Git command could not be executed"),
            Self::DesiredCommitUnavailable => {
                formatter.write_str("desired commit object is unavailable")
            }
            Self::DesiredCommitTreeMismatch => {
                formatter.write_str("desired commit tree does not match the reviewed tree")
            }
            Self::ObservationPersistence(_) => {
                formatter.write_str("post-push remote observation could not be persisted")
            }
            Self::Reconciliation(error) => error.fmt(formatter),
            Self::UnexpectedNoopReconciliation => {
                formatter.write_str("commit push unexpectedly reconciled as noop")
            }
        }
    }
}

impl<E: Error + 'static> Error for GitPushExecutionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ObservationPersistence(error) => Some(error),
            Self::Reconciliation(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Sha256, SnapshotId},
        publisher::{GitRepositoryIdentity, PublicationTarget, PublishRunId, RemoteRefState},
        workflow::ManagedRoot,
    };
    use std::{
        cell::RefCell,
        convert::Infallible,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::UNIX_EPOCH,
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    #[derive(Default)]
    struct MemoryObservationStore(RefCell<Vec<RemoteRefObservation>>);

    impl RemoteObservationStore for MemoryObservationStore {
        type Error = Infallible;

        fn save(&self, observation: &RemoteRefObservation) -> Result<(), Self::Error> {
            self.0.borrow_mut().push(observation.clone());
            Ok(())
        }

        fn get(
            &self,
            id: RemoteObservationId,
        ) -> Result<Option<RemoteRefObservation>, Self::Error> {
            Ok(self.0.borrow().iter().find(|item| item.id() == id).cloned())
        }

        fn list_for_publish_run(
            &self,
            publish_run_id: PublishRunId,
        ) -> Result<Vec<RemoteRefObservation>, Self::Error> {
            Ok(self
                .0
                .borrow()
                .iter()
                .filter(|item| item.publish_run_id() == publish_run_id)
                .cloned()
                .collect())
        }
    }

    struct TestRepository {
        root: PathBuf,
        local: PathBuf,
        actor: PathBuf,
        x: String,
        a: String,
        c: String,
        tree_c: String,
    }

    impl TestRepository {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-push-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let local = root.join("local");
            let actor = root.join("actor");
            let remote = root.join("remote.git");
            fs::create_dir(&root).unwrap();
            git(&root, &["init", "--bare", remote.to_str().unwrap()]);
            git(&root, &["init", local.to_str().unwrap()]);
            configure_identity(&local);

            fs::write(local.join("file.txt"), b"x").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "x"]);
            let x = git_stdout(&local, &["rev-parse", "HEAD"]);
            fs::write(local.join("file.txt"), b"a").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "a"]);
            let a = git_stdout(&local, &["rev-parse", "HEAD"]);
            git(&local, &["branch", "-M", "main"]);
            git(
                &local,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(&local, &["push", "-u", "origin", "main"]);
            git(
                &root,
                &["clone", remote.to_str().unwrap(), actor.to_str().unwrap()],
            );
            git(&actor, &["checkout", "-b", "main", "origin/main"]);
            configure_identity(&actor);

            fs::write(local.join("file.txt"), b"c").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "c"]);
            let c = git_stdout(&local, &["rev-parse", "HEAD"]);
            let tree_c = git_stdout(&local, &["rev-parse", "HEAD^{tree}"]);
            Self {
                root,
                local,
                actor,
                x,
                a,
                c,
                tree_c,
            }
        }

        fn run(&self, id: u64) -> PublishRun {
            PublishRun::rehydrate(
                PublishRunId::new(id).unwrap(),
                SnapshotId::new(1).unwrap(),
                Sha256::new([1; 32]),
                ManagedRoot::new("content").unwrap(),
                GitRepositoryIdentity::new(&self.local).unwrap(),
                PublicationTarget::new("origin", "refs/heads/main").unwrap(),
                self.a.clone(),
                self.tree_c.clone(),
                PublishRunPublication::CommitReady {
                    commit_oid: self.c.clone(),
                },
                1,
            )
            .unwrap()
        }

        fn ready(&self, run: &PublishRun) -> ReadyToPush {
            let observation =
                GitRemoteObserver::observe(RemoteObservationId::new(1).unwrap(), run, UNIX_EPOCH)
                    .unwrap();
            match PublishReconciliation::derive(run, &observation).unwrap() {
                PublishReconciliation::ReadyToPush(ready) => ready,
                other => panic!("expected ready, got {other:?}"),
            }
        }

        fn remote_oid(&self) -> String {
            git_stdout(&self.local, &["ls-remote", "origin", "refs/heads/main"])
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
        }

        fn actor_commit_and_push(&self, contents: &[u8], message: &str) -> String {
            fs::write(self.actor.join("file.txt"), contents).unwrap();
            git(&self.actor, &["add", "file.txt"]);
            git(&self.actor, &["commit", "-m", message]);
            let oid = git_stdout(&self.actor, &["rev-parse", "HEAD"]);
            git(&self.actor, &["push", "origin", "HEAD:refs/heads/main"]);
            oid
        }

        fn observe(&self, run: &PublishRun, id: u64) -> RemoteRefObservation {
            GitRemoteObserver::observe(RemoteObservationId::new(id).unwrap(), run, UNIX_EPOCH)
                .unwrap()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn configure_identity(repository: &Path) {
        git(
            repository,
            &["config", "user.name", "Mineral Publisher Test"],
        );
        git(
            repository,
            &["config", "user.email", "test@example.invalid"],
        );
    }

    fn git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn exact_cas_push_is_confirmed_by_a_persisted_post_observation() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);
        let store = MemoryObservationStore::default();

        let result = GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &store,
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::Published {
                command_outcome: GitPushCommandOutcome::Succeeded,
                ..
            }
        ));
        assert_eq!(repository.remote_oid(), repository.c);
        assert_eq!(store.0.borrow().len(), 1);
        assert_eq!(
            store.0.borrow()[0].id(),
            RemoteObservationId::new(2).unwrap()
        );
    }

    #[test]
    fn concurrent_advance_is_not_overwritten() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);
        let b = repository.actor_commit_and_push(b"b", "b");

        let result = GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::RemoteChanged {
                command_outcome: GitPushCommandOutcome::Failed { .. },
                ..
            }
        ));
        assert_eq!(repository.remote_oid(), b);
    }

    #[test]
    fn exact_lease_rejects_remote_rewind_even_when_desired_would_fast_forward() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);
        git(
            &repository.actor,
            &["reset", "--hard", repository.x.as_str()],
        );
        git(
            &repository.actor,
            &["push", "--force", "origin", "HEAD:refs/heads/main"],
        );

        let result = GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::RemoteChanged {
                command_outcome: GitPushCommandOutcome::Failed { .. },
                ..
            }
        ));
        assert_eq!(repository.remote_oid(), repository.x);
    }

    #[test]
    fn stale_tracking_ref_does_not_control_the_explicit_lease() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);
        git(
            &repository.local,
            &[
                "update-ref",
                "refs/remotes/origin/main",
                repository.x.as_str(),
            ],
        );

        let result = GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap();

        assert!(matches!(result, GitPushExecutionResult::Published { .. }));
        assert_eq!(repository.remote_oid(), repository.c);
    }

    #[test]
    fn push_does_not_modify_worktree_index_head_or_local_branch() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);
        fs::write(repository.local.join("file.txt"), b"dirty").unwrap();
        fs::write(repository.local.join("staged.txt"), b"staged").unwrap();
        git(&repository.local, &["add", "staged.txt"]);
        let before_status = git_stdout(&repository.local, &["status", "--porcelain=v1"]);
        let before_head = git_stdout(&repository.local, &["rev-parse", "HEAD"]);
        let before_branch = git_stdout(&repository.local, &["rev-parse", "refs/heads/main"]);

        GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap();

        assert_eq!(
            git_stdout(&repository.local, &["status", "--porcelain=v1"]),
            before_status
        );
        assert_eq!(
            git_stdout(&repository.local, &["rev-parse", "HEAD"]),
            before_head
        );
        assert_eq!(
            git_stdout(&repository.local, &["rev-parse", "refs/heads/main"]),
            before_branch
        );
    }

    #[test]
    fn ready_capability_from_another_run_is_rejected_before_push() {
        let repository = TestRepository::new();
        let first = repository.run(1);
        let ready = repository.ready(&first);
        let second = repository.run(2);

        let error = GitCompareAndPushExecutor::execute(
            &second,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GitPushExecutionError::ReadyToPushBindingMismatch
        ));
        assert_eq!(repository.remote_oid(), repository.a);
    }

    #[test]
    fn post_observation_must_not_reuse_the_pre_push_id() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);

        let error = GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            ready.source_observation_id(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GitPushExecutionError::PostObservationIdReused
        ));
        assert_eq!(repository.remote_oid(), repository.a);
    }

    #[test]
    fn failed_command_can_recover_as_published_from_remote_truth() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);
        let store = MemoryObservationStore::default();

        let result = GitCompareAndPushExecutor::execute_with(
            &run,
            &ready,
            &store,
            || {
                git(
                    &repository.local,
                    &[
                        "push",
                        "origin",
                        &format!("{}:refs/heads/main", repository.c),
                    ],
                );
                GitPushCommandOutcome::Failed { status: None }
            },
            || Ok(repository.observe(&run, 2)),
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::Published {
                command_outcome: GitPushCommandOutcome::Failed { status: None },
                ..
            }
        ));
    }

    #[test]
    fn failed_command_with_remote_still_at_base_is_not_retried() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);

        let result = GitCompareAndPushExecutor::execute_with(
            &run,
            &ready,
            &MemoryObservationStore::default(),
            || GitPushCommandOutcome::Failed { status: Some(1) },
            || Ok(repository.observe(&run, 2)),
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::PushFailedButRemoteUnchanged {
                status: Some(1),
                ..
            }
        ));
        assert_eq!(repository.remote_oid(), repository.a);
    }

    #[test]
    fn failed_command_and_failed_observation_is_indeterminate() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);

        let result = GitCompareAndPushExecutor::execute_with(
            &run,
            &ready,
            &MemoryObservationStore::default(),
            || GitPushCommandOutcome::Failed { status: None },
            || Err(GitRemoteObservationError::RemoteQueryFailed { status: Some(1) }),
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::Indeterminate { .. }
        ));
    }

    #[test]
    fn post_push_change_is_reported_instead_of_published() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let ready = repository.ready(&run);

        let result = GitCompareAndPushExecutor::execute_with(
            &run,
            &ready,
            &MemoryObservationStore::default(),
            || {
                git(
                    &repository.local,
                    &[
                        "push",
                        "origin",
                        &format!("{}:refs/heads/main", repository.c),
                    ],
                );
                git(&repository.actor, &["fetch", "origin"]);
                git(&repository.actor, &["reset", "--hard", "origin/main"]);
                repository.actor_commit_and_push(b"d", "d");
                GitPushCommandOutcome::Succeeded
            },
            || Ok(repository.observe(&run, 2)),
        )
        .unwrap();

        assert!(matches!(
            result,
            GitPushExecutionResult::RemoteChanged { .. }
        ));
    }

    #[test]
    fn desired_commit_must_still_exist_and_match_the_reviewed_tree() {
        let repository = TestRepository::new();
        let mut wrong_tree_run = repository.run(1);
        wrong_tree_run = PublishRun::rehydrate(
            wrong_tree_run.id(),
            wrong_tree_run.snapshot_id(),
            wrong_tree_run.projection_sha256(),
            wrong_tree_run.managed_root().clone(),
            wrong_tree_run.repository().clone(),
            wrong_tree_run.target().clone(),
            wrong_tree_run.base_commit().to_owned(),
            repository.x.clone(),
            wrong_tree_run.publication().clone(),
            wrong_tree_run.created_at_unix_ms(),
        )
        .unwrap();
        let ready = repository.ready(&wrong_tree_run);

        let error = GitCompareAndPushExecutor::execute(
            &wrong_tree_run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GitPushExecutionError::DesiredCommitTreeMismatch
        ));
        assert_eq!(repository.remote_oid(), repository.a);
    }

    #[test]
    fn missing_desired_commit_fails_before_remote_side_effect() {
        let repository = TestRepository::new();
        let run = PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            Sha256::new([1; 32]),
            ManagedRoot::new("content").unwrap(),
            GitRepositoryIdentity::new(&repository.local).unwrap(),
            PublicationTarget::new("origin", "refs/heads/main").unwrap(),
            repository.a.clone(),
            repository.tree_c.clone(),
            PublishRunPublication::CommitReady {
                commit_oid: "d".repeat(40),
            },
            1,
        )
        .unwrap();
        let ready = repository.ready(&run);

        let error = GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &MemoryObservationStore::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GitPushExecutionError::DesiredCommitUnavailable
        ));
        assert_eq!(repository.remote_oid(), repository.a);
    }

    #[test]
    fn post_observation_is_a_new_immutable_audit_fact() {
        let repository = TestRepository::new();
        let run = repository.run(1);
        let pre = repository.observe(&run, 1);
        let ready = match PublishReconciliation::derive(&run, &pre).unwrap() {
            PublishReconciliation::ReadyToPush(ready) => ready,
            other => panic!("expected ready, got {other:?}"),
        };
        let store = MemoryObservationStore::default();
        store.save(&pre).unwrap();

        GitCompareAndPushExecutor::execute(
            &run,
            &ready,
            RemoteObservationId::new(2).unwrap(),
            UNIX_EPOCH,
            &store,
        )
        .unwrap();

        let observations = store.list_for_publish_run(run.id()).unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].id(), RemoteObservationId::new(1).unwrap());
        assert_eq!(
            observations[0].observed(),
            &RemoteRefState::Present {
                commit_oid: GitCommitOid::new(repository.a.clone()).unwrap()
            }
        );
        assert_eq!(observations[1].id(), RemoteObservationId::new(2).unwrap());
        assert_eq!(
            observations[1].observed(),
            &RemoteRefState::Present {
                commit_oid: GitCommitOid::new(repository.c.clone()).unwrap()
            }
        );
    }
}
