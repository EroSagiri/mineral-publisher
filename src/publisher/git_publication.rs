use std::{error::Error, fmt, path::Path, time::SystemTime};

use uuid::Uuid;

use crate::{
    storage::LocalContentStore,
    workflow::{PublicProjection, PublishPlan, PublishPlanError},
};

use super::{
    GitCommitMetadata, GitCommitObjectCreator, GitCommitObjectError, GitCurrentTargetAdapter,
    GitCurrentTargetError, GitProjectionMaterializationError, GitProjectionMaterializer,
    GitRemoteObservationError, GitRemoteObserver, GitRepositoryIdentity,
    GitRepositoryIdentityError, PublicationTarget, PublicationWorkflow, PublicationWorkflowError,
    PublicationWorkflowResult, PublishRun, PublishRunError, PublishRunId, PublishRunStore,
    RemoteObservationId, RemoteObservationIdGenerator, RemoteObservationStore, RemoteRefState,
};

/// Allocates immutable publication-attempt identities at the application boundary.
pub trait PublishRunIdGenerator {
    type Error: Error;

    fn next_id(&mut self) -> Result<PublishRunId, Self::Error>;
}

/// A deterministic caller-owned allocator for tests.
#[derive(Clone, Debug)]
pub struct SequentialPublishRunIdGenerator {
    next: Option<u64>,
}

impl SequentialPublishRunIdGenerator {
    pub fn new(first: PublishRunId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl PublishRunIdGenerator for SequentialPublishRunIdGenerator {
    type Error = SequentialPublishRunIdGeneratorError;

    fn next_id(&mut self) -> Result<PublishRunId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialPublishRunIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        PublishRunId::new(value).map_err(|_| SequentialPublishRunIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialPublishRunIdGeneratorError {
    Exhausted,
}

impl fmt::Display for SequentialPublishRunIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("publish run ID sequence is exhausted")
    }
}

impl Error for SequentialPublishRunIdGeneratorError {}

/// Production identity allocator backed by UUID v4 entropy.
///
/// The current durable schema uses positive `u64` IDs, so this generator maps
/// a UUID v4 to its first non-zero 63 bits. That preserves a collision-resistant
/// process-restart-safe source without changing the existing SQLite ID format.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidPublishRunIdGenerator;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UuidPublishRunIdGeneratorError {
    InvalidGeneratedId,
}

impl fmt::Display for UuidPublishRunIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UUID-based publish run ID was invalid")
    }
}

impl Error for UuidPublishRunIdGeneratorError {}

impl PublishRunIdGenerator for UuidPublishRunIdGenerator {
    type Error = UuidPublishRunIdGeneratorError;

    fn next_id(&mut self) -> Result<PublishRunId, Self::Error> {
        PublishRunId::new(positive_uuid_id())
            .map_err(|_| UuidPublishRunIdGeneratorError::InvalidGeneratedId)
    }
}

/// Production remote-observation allocator using independent UUID v4 entropy.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidRemoteObservationIdGenerator;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UuidRemoteObservationIdGeneratorError {
    InvalidGeneratedId,
}

impl fmt::Display for UuidRemoteObservationIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UUID-based remote observation ID was invalid")
    }
}

impl Error for UuidRemoteObservationIdGeneratorError {}

impl RemoteObservationIdGenerator for UuidRemoteObservationIdGenerator {
    type Error = UuidRemoteObservationIdGeneratorError;

    fn next_id(&mut self) -> Result<RemoteObservationId, Self::Error> {
        RemoteObservationId::new(positive_uuid_id())
            .map_err(|_| UuidRemoteObservationIdGeneratorError::InvalidGeneratedId)
    }
}

fn positive_uuid_id() -> u64 {
    let bytes = Uuid::new_v4().into_bytes();
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&bytes[..8]);
    let value = u64::from_be_bytes(prefix) & i64::MAX as u64;
    value.max(1)
}

/// Thin application boundary for preparing one public projection and executing
/// the existing recovery-safe publication workflow.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitPublicationApplication;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPublicationApplicationResult {
    publish_run_id: PublishRunId,
    projection_sha256: crate::domain::Sha256,
    publish_plan_sha256: crate::domain::Sha256,
    workflow: PublicationWorkflowResult,
}

impl GitPublicationApplicationResult {
    pub fn publish_run_id(&self) -> PublishRunId {
        self.publish_run_id
    }

    pub fn projection_sha256(&self) -> crate::domain::Sha256 {
        self.projection_sha256
    }

    pub fn publish_plan_sha256(&self) -> crate::domain::Sha256 {
        self.publish_plan_sha256
    }

    pub fn workflow(&self) -> &PublicationWorkflowResult {
        &self.workflow
    }
}

#[derive(Debug)]
pub enum GitPublicationApplicationError<P: Error, O: Error, R: Error, I: Error> {
    RepositoryIdentity(GitRepositoryIdentityError),
    PreparationObservation(GitRemoteObservationError),
    TargetMissing,
    RemoteBaseUnavailableLocally { commit_oid: String },
    CurrentTarget(GitCurrentTargetError),
    ObservedBaseMismatch { observed: String, resolved: String },
    PublishPlan(PublishPlanError),
    Materialization(GitProjectionMaterializationError),
    PlanReviewedTreeMismatch,
    Commit(GitCommitObjectError),
    PublishRunId(R),
    PublishRun(PublishRunError),
    PublishRunPersistence(P),
    Workflow(PublicationWorkflowError<P, O, I>),
}

impl<P: Error, O: Error, R: Error, I: Error> fmt::Display
    for GitPublicationApplicationError<P, O, R, I>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryIdentity(error) => {
                write!(formatter, "could not identify Git repository: {error}")
            }
            Self::PreparationObservation(error) => write!(
                formatter,
                "could not observe remote publication base: {error}"
            ),
            Self::TargetMissing => formatter.write_str("publication target ref is missing"),
            Self::RemoteBaseUnavailableLocally { commit_oid } => write!(
                formatter,
                "remote publication base is unavailable locally: {commit_oid}"
            ),
            Self::CurrentTarget(error) => {
                write!(formatter, "could not read current Git target: {error}")
            }
            Self::ObservedBaseMismatch { .. } => {
                formatter.write_str("resolved Git base differs from observed remote base")
            }
            Self::PublishPlan(error) => write!(formatter, "could not derive publish plan: {error}"),
            Self::Materialization(error) => write!(
                formatter,
                "could not materialize reviewed Git tree: {error}"
            ),
            Self::PlanReviewedTreeMismatch => {
                formatter.write_str("publish plan and reviewed Git tree disagree about noop state")
            }
            Self::Commit(error) => write!(formatter, "could not create Git commit object: {error}"),
            Self::PublishRunId(_) => formatter.write_str("could not allocate publish run ID"),
            Self::PublishRun(error) => {
                write!(formatter, "could not construct publish run: {error}")
            }
            Self::PublishRunPersistence(_) => {
                formatter.write_str("could not persist publish run before publication")
            }
            Self::Workflow(error) => error.fmt(formatter),
        }
    }
}

impl<P: Error + 'static, O: Error + 'static, R: Error + 'static, I: Error + 'static> Error
    for GitPublicationApplicationError<P, O, R, I>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RepositoryIdentity(error) => Some(error),
            Self::PreparationObservation(error) => Some(error),
            Self::CurrentTarget(error) => Some(error),
            Self::PublishPlan(error) => Some(error),
            Self::Materialization(error) => Some(error),
            Self::Commit(error) => Some(error),
            Self::PublishRunId(error) => Some(error),
            Self::PublishRun(error) => Some(error),
            Self::PublishRunPersistence(error) => Some(error),
            Self::Workflow(error) => Some(error),
            Self::TargetMissing
            | Self::RemoteBaseUnavailableLocally { .. }
            | Self::ObservedBaseMismatch { .. }
            | Self::PlanReviewedTreeMismatch => None,
        }
    }
}

impl GitPublicationApplication {
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn prepare_and_publish<P, O, R, I>(
        projection: &PublicProjection,
        repository: impl AsRef<Path>,
        target: PublicationTarget,
        commit_metadata: &GitCommitMetadata,
        content_store: &LocalContentStore,
        publish_run_store: &P,
        observation_store: &O,
        publish_run_ids: &mut R,
        observation_ids: &mut I,
    ) -> Result<
        GitPublicationApplicationResult,
        GitPublicationApplicationError<P::Error, O::Error, R::Error, I::Error>,
    >
    where
        P: PublishRunStore,
        O: RemoteObservationStore,
        R: PublishRunIdGenerator,
        I: RemoteObservationIdGenerator,
    {
        let repository = GitRepositoryIdentity::new(repository)
            .map_err(GitPublicationApplicationError::RepositoryIdentity)?;
        let observed = GitRemoteObserver::observe_target(repository.path(), &target)
            .map_err(GitPublicationApplicationError::PreparationObservation)?;
        let RemoteRefState::Present { commit_oid } = observed else {
            return Err(GitPublicationApplicationError::TargetMissing);
        };
        let observed_base = commit_oid.as_str().to_owned();
        let current = GitCurrentTargetAdapter::read(
            repository.path(),
            &observed_base,
            projection.managed_root().clone(),
        )
        .map_err(|error| match error {
            GitCurrentTargetError::RevisionNotFound { .. } => {
                GitPublicationApplicationError::RemoteBaseUnavailableLocally {
                    commit_oid: observed_base.clone(),
                }
            }
            error => GitPublicationApplicationError::CurrentTarget(error),
        })?;
        if current.base_commit() != observed_base {
            return Err(GitPublicationApplicationError::ObservedBaseMismatch {
                observed: observed_base,
                resolved: current.base_commit().to_owned(),
            });
        }
        let plan = PublishPlan::build(current.state(), projection)
            .map_err(GitPublicationApplicationError::PublishPlan)?;
        let reviewed = GitProjectionMaterializer::materialize(
            repository.path(),
            current.base_commit(),
            projection,
            content_store,
        )
        .map_err(GitPublicationApplicationError::Materialization)?;
        if plan.operations().is_empty() != reviewed.is_noop() {
            return Err(GitPublicationApplicationError::PlanReviewedTreeMismatch);
        }
        let commit = GitCommitObjectCreator::create(repository.path(), &reviewed, commit_metadata)
            .map_err(GitPublicationApplicationError::Commit)?;
        let publish_run_id = publish_run_ids
            .next_id()
            .map_err(GitPublicationApplicationError::PublishRunId)?;
        let publish_run = PublishRun::from_git_commit_result(
            publish_run_id,
            repository,
            target,
            &commit,
            SystemTime::now(),
        )
        .map_err(GitPublicationApplicationError::PublishRun)?;
        publish_run_store
            .save(&publish_run)
            .map_err(GitPublicationApplicationError::PublishRunPersistence)?;
        let workflow = PublicationWorkflow::execute(
            publish_run_id,
            publish_run_store,
            observation_store,
            observation_ids,
        )
        .map_err(GitPublicationApplicationError::Workflow)?;
        Ok(GitPublicationApplicationResult {
            publish_run_id,
            projection_sha256: projection.projection_sha256(),
            publish_plan_sha256: plan.plan_sha256(),
            workflow,
        })
    }

    #[allow(clippy::type_complexity)]
    pub fn resume<P, O, I>(
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
        PublicationWorkflow::execute(
            publish_run_id,
            publish_run_store,
            observation_store,
            observation_ids,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        convert::Infallible,
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    use crate::{
        domain::{ContentPath, Snapshot, SnapshotFile, SnapshotId, SourceId},
        publisher::SequentialRemoteObservationIdGenerator,
        storage::{SqlitePublishRunStore, SqliteRemoteObservationStore},
        workflow::{FinalPublicationSet, ManagedRoot, PublicProjection},
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestRepository {
        root: PathBuf,
        local: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-git-publication-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let local = root.join("local");
            let remote = root.join("remote.git");
            fs::create_dir_all(&root).unwrap();
            git(&root, &["init", "--bare", remote.to_str().unwrap()]);
            git(&root, &["init", local.to_str().unwrap()]);
            git(&local, &["config", "user.name", "Mineral Publisher Test"]);
            git(&local, &["config", "user.email", "test@example.invalid"]);
            fs::create_dir_all(local.join("content")).unwrap();
            fs::write(local.join("content/old.md"), b"old").unwrap();
            git(&local, &["add", "content/old.md"]);
            git(&local, &["commit", "-m", "base"]);
            git(&local, &["branch", "-M", "main"]);
            git(
                &local,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(&local, &["push", "-u", "origin", "main"]);
            Self { root, local }
        }

        fn remote_oid(&self) -> String {
            git_stdout(&self.local, &["ls-remote", "origin", "refs/heads/main"])
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
        }

        fn projection(&self, store: &LocalContentStore) -> PublicProjection {
            let identity = store.store(b"new public content").unwrap();
            let snapshot = Snapshot::new(
                SnapshotId::new(1).unwrap(),
                SystemTime::UNIX_EPOCH,
                SourceId::new("test").unwrap(),
                vec![SnapshotFile::new(
                    ContentPath::new("note.md").unwrap(),
                    18,
                    identity,
                    None,
                )],
            )
            .unwrap();
            let set = FinalPublicationSet::from_parts_for_test(
                snapshot.id(),
                vec![ContentPath::new("note.md").unwrap()],
                vec![],
            );
            PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap()
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
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn metadata() -> GitCommitMetadata {
        GitCommitMetadata::with_timestamp(
            "Mineral Publisher",
            "publisher@example.invalid",
            "Publish Mineral content",
            SystemTime::UNIX_EPOCH,
        )
        .unwrap()
    }

    fn target() -> PublicationTarget {
        PublicationTarget::new("origin", "refs/heads/main").unwrap()
    }

    #[derive(Debug)]
    struct SaveFailure;
    impl fmt::Display for SaveFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("save failed")
        }
    }
    impl Error for SaveFailure {}

    struct RejectingPublishRunStore;
    impl PublishRunStore for RejectingPublishRunStore {
        type Error = SaveFailure;
        fn save(&self, _: &PublishRun) -> Result<(), Self::Error> {
            Err(SaveFailure)
        }
        fn get(&self, _: PublishRunId) -> Result<Option<PublishRun>, Self::Error> {
            Ok(None)
        }
        fn list(&self) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(Vec::new())
        }
        fn list_for_target(&self, _: &PublicationTarget) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(Vec::new())
        }
    }

    struct AdvancingPublishRunStore {
        inner: SqlitePublishRunStore,
        repository: PathBuf,
    }

    impl PublishRunStore for AdvancingPublishRunStore {
        type Error = crate::storage::SqlitePublishRunStoreError;

        fn save(&self, run: &PublishRun) -> Result<(), Self::Error> {
            self.inner.save(run)?;
            fs::write(
                self.repository.join("content/foreign.md"),
                b"foreign advance",
            )
            .unwrap();
            git(&self.repository, &["add", "content/foreign.md"]);
            git(&self.repository, &["commit", "-m", "foreign advance"]);
            git(&self.repository, &["push", "origin", "main"]);
            Ok(())
        }

        fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error> {
            self.inner.get(id)
        }

        fn list(&self) -> Result<Vec<PublishRun>, Self::Error> {
            self.inner.list()
        }

        fn list_for_target(
            &self,
            target: &PublicationTarget,
        ) -> Result<Vec<PublishRun>, Self::Error> {
            self.inner.list_for_target(target)
        }
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
        fn list_for_target(&self, _: &PublicationTarget) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn uuid_generators_are_nonzero_and_do_not_repeat_in_a_small_sample() {
        let mut publish_ids = UuidPublishRunIdGenerator;
        let mut observation_ids = UuidRemoteObservationIdGenerator;
        let publish = (0..1024)
            .map(|_| publish_ids.next_id().unwrap().get())
            .collect::<std::collections::BTreeSet<_>>();
        let observations = (0..1024)
            .map(|_| observation_ids.next_id().unwrap().get())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(publish.len(), 1024);
        assert_eq!(observations.len(), 1024);
    }

    #[test]
    fn missing_remote_target_stops_before_preparation_or_persistence() {
        let repository = TestRepository::new();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let projection = repository.projection(&store);
        let runs = MemoryPublishRunStore::default();
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &repository.local,
            PublicationTarget::new("origin", "refs/heads/missing").unwrap(),
            &metadata(),
            &store,
            &runs,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        );

        assert!(matches!(
            result,
            Err(GitPublicationApplicationError::TargetMissing)
        ));
        assert_eq!(
            repository.remote_oid(),
            git_stdout(&repository.local, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn persistence_failure_prevents_any_remote_push() {
        let repository = TestRepository::new();
        let before = repository.remote_oid();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let projection = repository.projection(&store);
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &repository.local,
            target(),
            &metadata(),
            &store,
            &RejectingPublishRunStore,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        );

        assert!(matches!(
            result,
            Err(GitPublicationApplicationError::PublishRunPersistence(_))
        ));
        assert_eq!(repository.remote_oid(), before);
    }

    #[test]
    fn remote_advance_after_preparation_is_reconciled_without_an_overwrite() {
        let repository = TestRepository::new();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let projection = repository.projection(&store);
        let runs = AdvancingPublishRunStore {
            inner: SqlitePublishRunStore::open(repository.root.join("runs.sqlite")).unwrap(),
            repository: repository.local.clone(),
        };
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &repository.local,
            target(),
            &metadata(),
            &store,
            &runs,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        )
        .unwrap();

        assert!(matches!(
            result.workflow(),
            PublicationWorkflowResult::RemoteChanged { .. }
        ));
        assert_eq!(
            repository.remote_oid(),
            git_stdout(&repository.local, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            git_stdout(
                &repository.local,
                &["show", "--format=", "--name-only", "HEAD"]
            ),
            "content/foreign.md"
        );
    }

    #[test]
    fn durable_attempt_publishes_and_resume_observes_already_published_without_a_second_push() {
        let repository = TestRepository::new();
        fs::write(repository.local.join("unrelated-dirty-file"), b"ignored").unwrap();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let projection = repository.projection(&store);
        let run_db = repository.root.join("runs.sqlite");
        let observation_db = repository.root.join("observations.sqlite");
        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let observations = SqliteRemoteObservationStore::open(&observation_db).unwrap();
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &repository.local,
            target(),
            &metadata(),
            &store,
            &runs,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        )
        .unwrap();
        assert!(matches!(
            result.workflow(),
            PublicationWorkflowResult::Published { .. }
        ));
        let published = repository.remote_oid();
        assert_ne!(
            published,
            git_stdout(&repository.local, &["rev-parse", "HEAD"])
        );
        drop(observations);
        drop(runs);

        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let observations = SqliteRemoteObservationStore::open(&observation_db).unwrap();
        let mut recovery_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(3).unwrap());
        let recovered = GitPublicationApplication::resume(
            result.publish_run_id(),
            &runs,
            &observations,
            &mut recovery_ids,
        )
        .unwrap();
        assert!(matches!(
            recovered,
            PublicationWorkflowResult::AlreadyPublished { .. }
        ));
        assert_eq!(repository.remote_oid(), published);
    }
}
