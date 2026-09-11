use std::{error::Error, fmt, time::SystemTime};

use super::{
    GitCompareAndPushExecutor, GitPushCommandOutcome, GitPushExecutionError,
    GitPushExecutionResult, GitRemoteObservationError, GitRemoteObserver, PublishReconciliation,
    PublishReconciliationError, PublishRunId, PublishRunStore, RemoteObservationId,
    RemoteObservationStore, RemoteRefObservation,
};

/// Allocates immutable remote-observation identities at the publication boundary.
pub trait RemoteObservationIdGenerator {
    type Error: Error;

    fn next_id(&mut self) -> Result<RemoteObservationId, Self::Error>;
}

/// A small caller-owned allocator, suitable for tests and single-process execution.
#[derive(Clone, Debug)]
pub struct SequentialRemoteObservationIdGenerator {
    next: Option<u64>,
}

impl SequentialRemoteObservationIdGenerator {
    pub fn new(first: RemoteObservationId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl RemoteObservationIdGenerator for SequentialRemoteObservationIdGenerator {
    type Error = SequentialRemoteObservationIdGeneratorError;

    fn next_id(&mut self) -> Result<RemoteObservationId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialRemoteObservationIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        RemoteObservationId::new(value)
            .map_err(|_| SequentialRemoteObservationIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialRemoteObservationIdGeneratorError {
    Exhausted,
}

impl fmt::Display for SequentialRemoteObservationIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote observation ID sequence is exhausted")
    }
}

impl Error for SequentialRemoteObservationIdGeneratorError {}

/// The explicit outcome of one recovery-safe publication execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationWorkflowResult {
    NoopSatisfied {
        observation: RemoteRefObservation,
    },
    AlreadyPublished {
        observation: RemoteRefObservation,
    },
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
        command_outcome: Option<GitPushCommandOutcome>,
    },
    TargetMissing {
        observation: RemoteRefObservation,
        command_outcome: Option<GitPushCommandOutcome>,
    },
    Indeterminate {
        command_outcome: GitPushCommandOutcome,
    },
}

impl PublicationWorkflowResult {
    pub fn is_satisfied(&self) -> bool {
        matches!(
            self,
            Self::NoopSatisfied { .. } | Self::AlreadyPublished { .. } | Self::Published { .. }
        )
    }
}

/// Errors that prevented the workflow from obtaining a durable, meaningful outcome.
#[derive(Debug)]
pub enum PublicationWorkflowError<P: Error, O: Error, I: Error> {
    PublishRunLoad(P),
    PublishRunNotFound(PublishRunId),
    ObservationId(I),
    PreObservation(GitRemoteObservationError),
    PreObservationPersistence(O),
    Reconciliation(PublishReconciliationError),
    Push(GitPushExecutionError<O>),
}

impl<P: Error, O: Error, I: Error> fmt::Display for PublicationWorkflowError<P, O, I> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublishRunLoad(_) => formatter.write_str("could not load publish run"),
            Self::PublishRunNotFound(id) => {
                write!(formatter, "publish run {} was not found", id.get())
            }
            Self::ObservationId(_) => {
                formatter.write_str("could not allocate remote observation ID")
            }
            Self::PreObservation(error) => {
                write!(formatter, "could not observe remote before push: {error}")
            }
            Self::PreObservationPersistence(_) => {
                formatter.write_str("pre-push remote observation could not be persisted")
            }
            Self::Reconciliation(error) => error.fmt(formatter),
            Self::Push(error) => error.fmt(formatter),
        }
    }
}

impl<P: Error + 'static, O: Error + 'static, I: Error + 'static> Error
    for PublicationWorkflowError<P, O, I>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PublishRunLoad(error) => Some(error),
            Self::ObservationId(error) => Some(error),
            Self::PreObservation(error) => Some(error),
            Self::PreObservationPersistence(error) => Some(error),
            Self::Reconciliation(error) => Some(error),
            Self::Push(error) => Some(error),
            Self::PublishRunNotFound(_) => None,
        }
    }
}

/// Orchestrates durable intent, observation, reconciliation, and at most one CAS push.
#[derive(Clone, Copy, Debug, Default)]
pub struct PublicationWorkflow;

impl PublicationWorkflow {
    #[allow(clippy::type_complexity)]
    pub fn execute<P, O, I>(
        publish_run_id: PublishRunId,
        publish_run_store: &P,
        observation_store: &O,
        observation_ids: &mut I,
    ) -> Result<PublicationWorkflowResult, PublicationWorkflowError<P::Error, O::Error, I::Error>>
    where
        P: PublishRunStore,
        O: RemoteObservationStore,
        I: RemoteObservationIdGenerator,
    {
        let publish_run = publish_run_store
            .get(publish_run_id)
            .map_err(PublicationWorkflowError::PublishRunLoad)?
            .ok_or(PublicationWorkflowError::PublishRunNotFound(publish_run_id))?;
        let pre_observation_id = observation_ids
            .next_id()
            .map_err(PublicationWorkflowError::ObservationId)?;
        let pre_observation =
            GitRemoteObserver::observe(pre_observation_id, &publish_run, SystemTime::now())
                .map_err(PublicationWorkflowError::PreObservation)?;
        observation_store
            .save(&pre_observation)
            .map_err(PublicationWorkflowError::PreObservationPersistence)?;

        match PublishReconciliation::derive(&publish_run, &pre_observation)
            .map_err(PublicationWorkflowError::Reconciliation)?
        {
            PublishReconciliation::NoopSatisfied => Ok(Self::noop_satisfied(pre_observation)),
            PublishReconciliation::AlreadyPublished => Ok(Self::already_published(pre_observation)),
            PublishReconciliation::RemoteChanged { .. } => {
                Ok(Self::remote_changed(pre_observation, None))
            }
            PublishReconciliation::TargetMissing => Ok(Self::target_missing(pre_observation, None)),
            PublishReconciliation::ReadyToPush(ready) => {
                let post_observation_id = observation_ids
                    .next_id()
                    .map_err(PublicationWorkflowError::ObservationId)?;
                let result = GitCompareAndPushExecutor::execute(
                    &publish_run,
                    &ready,
                    post_observation_id,
                    SystemTime::now(),
                    observation_store,
                )
                .map_err(PublicationWorkflowError::Push)?;
                Ok(Self::from_push_result(result))
            }
        }
    }

    fn noop_satisfied(observation: RemoteRefObservation) -> PublicationWorkflowResult {
        PublicationWorkflowResult::NoopSatisfied { observation }
    }
    fn already_published(observation: RemoteRefObservation) -> PublicationWorkflowResult {
        PublicationWorkflowResult::AlreadyPublished { observation }
    }
    fn remote_changed(
        observation: RemoteRefObservation,
        command_outcome: Option<GitPushCommandOutcome>,
    ) -> PublicationWorkflowResult {
        PublicationWorkflowResult::RemoteChanged {
            observation,
            command_outcome,
        }
    }
    fn target_missing(
        observation: RemoteRefObservation,
        command_outcome: Option<GitPushCommandOutcome>,
    ) -> PublicationWorkflowResult {
        PublicationWorkflowResult::TargetMissing {
            observation,
            command_outcome,
        }
    }

    fn from_push_result(result: GitPushExecutionResult) -> PublicationWorkflowResult {
        match result {
            GitPushExecutionResult::Published {
                observation,
                command_outcome,
            } => PublicationWorkflowResult::Published {
                observation,
                command_outcome,
            },
            GitPushExecutionResult::PushFailedButRemoteUnchanged {
                observation,
                status,
            } => PublicationWorkflowResult::PushFailedButRemoteUnchanged {
                observation,
                status,
            },
            GitPushExecutionResult::RemoteUnchangedAfterSuccessfulPush { observation } => {
                PublicationWorkflowResult::RemoteUnchangedAfterSuccessfulPush { observation }
            }
            GitPushExecutionResult::RemoteChanged {
                observation,
                command_outcome,
                ..
            } => Self::remote_changed(observation, Some(command_outcome)),
            GitPushExecutionResult::TargetMissing {
                observation,
                command_outcome,
            } => Self::target_missing(observation, Some(command_outcome)),
            GitPushExecutionResult::Indeterminate {
                command_outcome, ..
            } => PublicationWorkflowResult::Indeterminate { command_outcome },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Sha256, SnapshotId},
        publisher::{
            GitRepositoryIdentity, PublicationTarget, PublishRun, PublishRunPublication,
            RemoteRefState,
        },
        storage::{SqlitePublishRunStore, SqliteRemoteObservationStore},
        workflow::ManagedRoot,
    };
    use std::{
        collections::BTreeMap,
        convert::Infallible,
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestRepository {
        root: PathBuf,
        local: PathBuf,
        a: String,
        c: String,
        tree_c: String,
    }

    impl TestRepository {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-publication-workflow-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let local = root.join("local");
            let remote = root.join("remote.git");
            fs::create_dir(&root).unwrap();
            git(&root, &["init", "--bare", remote.to_str().unwrap()]);
            git(&root, &["init", local.to_str().unwrap()]);
            git(&local, &["config", "user.name", "Mineral Publisher Test"]);
            git(&local, &["config", "user.email", "test@example.invalid"]);
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
            fs::write(local.join("file.txt"), b"c").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "c"]);
            let c = git_stdout(&local, &["rev-parse", "HEAD"]);
            let tree_c = git_stdout(&local, &["rev-parse", "HEAD^{tree}"]);
            Self {
                root,
                local,
                a,
                c,
                tree_c,
            }
        }

        fn run(&self, id: u64, publication: PublishRunPublication) -> PublishRun {
            PublishRun::rehydrate(
                PublishRunId::new(id).unwrap(),
                SnapshotId::new(1).unwrap(),
                Sha256::new([1; 32]),
                ManagedRoot::new("content").unwrap(),
                GitRepositoryIdentity::new(&self.local).unwrap(),
                PublicationTarget::new("origin", "refs/heads/main").unwrap(),
                self.a.clone(),
                self.tree_c.clone(),
                publication,
                1,
            )
            .unwrap()
        }

        fn remote_oid(&self) -> String {
            git_stdout(&self.local, &["ls-remote", "origin", "refs/heads/main"])
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[derive(Default)]
    struct MemoryPublishRunStore(BTreeMap<PublishRunId, PublishRun>);
    impl PublishRunStore for MemoryPublishRunStore {
        type Error = Infallible;
        fn save(&self, _: &PublishRun) -> Result<(), Self::Error> {
            unreachable!()
        }
        fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error> {
            Ok(self.0.get(&id).cloned())
        }
        fn list(&self) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(self.0.values().cloned().collect())
        }
        fn list_for_target(
            &self,
            _: &crate::publisher::PublicationTarget,
        ) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug)]
    struct RejectingStoreError;
    impl fmt::Display for RejectingStoreError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("rejected")
        }
    }
    impl Error for RejectingStoreError {}
    struct RejectingObservationStore;
    impl RemoteObservationStore for RejectingObservationStore {
        type Error = RejectingStoreError;
        fn save(&self, _: &RemoteRefObservation) -> Result<(), Self::Error> {
            Err(RejectingStoreError)
        }
        fn get(&self, _: RemoteObservationId) -> Result<Option<RemoteRefObservation>, Self::Error> {
            Ok(None)
        }
        fn list_for_publish_run(
            &self,
            _: PublishRunId,
        ) -> Result<Vec<RemoteRefObservation>, Self::Error> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn noop_is_observed_and_satisfied_without_a_push() {
        let repository = TestRepository::new();
        let run = repository.run(1, PublishRunPublication::Noop);
        let publish_store = MemoryPublishRunStore(BTreeMap::from([(run.id(), run)]));
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("observations.sqlite"))
                .unwrap();
        let mut ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = PublicationWorkflow::execute(
            PublishRunId::new(1).unwrap(),
            &publish_store,
            &observations,
            &mut ids,
        )
        .unwrap();

        assert!(matches!(
            result,
            PublicationWorkflowResult::NoopSatisfied { .. }
        ));
        assert_eq!(repository.remote_oid(), repository.a);
        assert_eq!(
            observations
                .list_for_publish_run(PublishRunId::new(1).unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn published_run_recovers_after_sqlite_restart_without_another_push() {
        let repository = TestRepository::new();
        let run = repository.run(
            1,
            PublishRunPublication::CommitReady {
                commit_oid: repository.c.clone(),
            },
        );
        let publish_db = repository.root.join("publish.sqlite");
        let observation_db = repository.root.join("observations.sqlite");
        let publish_store = SqlitePublishRunStore::open(&publish_db).unwrap();
        publish_store.save(&run).unwrap();
        let observations = SqliteRemoteObservationStore::open(&observation_db).unwrap();
        let mut first_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());
        let first =
            PublicationWorkflow::execute(run.id(), &publish_store, &observations, &mut first_ids)
                .unwrap();
        assert!(matches!(first, PublicationWorkflowResult::Published { .. }));
        assert_eq!(repository.remote_oid(), repository.c);
        drop(observations);
        drop(publish_store);

        let publish_store = SqlitePublishRunStore::open(&publish_db).unwrap();
        let observations = SqliteRemoteObservationStore::open(&observation_db).unwrap();
        let mut recovered_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(3).unwrap());
        let recovered = PublicationWorkflow::execute(
            run.id(),
            &publish_store,
            &observations,
            &mut recovered_ids,
        )
        .unwrap();
        assert!(matches!(
            recovered,
            PublicationWorkflowResult::AlreadyPublished { .. }
        ));
        assert_eq!(repository.remote_oid(), repository.c);
        assert_eq!(
            observations.list_for_publish_run(run.id()).unwrap().len(),
            3
        );
    }

    #[test]
    fn persisted_pre_observation_is_required_before_any_push() {
        let repository = TestRepository::new();
        let run = repository.run(
            1,
            PublishRunPublication::CommitReady {
                commit_oid: repository.c.clone(),
            },
        );
        let publish_store = MemoryPublishRunStore(BTreeMap::from([(run.id(), run)]));
        let mut ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let error = PublicationWorkflow::execute(
            PublishRunId::new(1).unwrap(),
            &publish_store,
            &RejectingObservationStore,
            &mut ids,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            PublicationWorkflowError::PreObservationPersistence(_)
        ));
        assert_eq!(repository.remote_oid(), repository.a);
    }

    #[test]
    fn missing_run_is_a_typed_error_without_remote_access() {
        let store = MemoryPublishRunStore::default();
        let observations = RejectingObservationStore;
        let mut ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());
        let error = PublicationWorkflow::execute(
            PublishRunId::new(1).unwrap(),
            &store,
            &observations,
            &mut ids,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PublicationWorkflowError::PublishRunNotFound(_)
        ));
    }

    #[test]
    fn result_satisfaction_only_means_durable_completion() {
        let repository = TestRepository::new();
        let run = repository.run(1, PublishRunPublication::Noop);
        let observation = RemoteRefObservation::new(
            RemoteObservationId::new(1).unwrap(),
            &run,
            RemoteRefState::Missing,
            SystemTime::now(),
        )
        .unwrap();
        assert!(
            !PublicationWorkflowResult::TargetMissing {
                observation,
                command_outcome: None
            }
            .is_satisfied()
        );
    }
}
