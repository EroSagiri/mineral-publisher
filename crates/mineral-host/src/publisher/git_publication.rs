use std::{error::Error, fmt, path::Path, time::SystemTime};

use uuid::Uuid;

use crate::{
    asset::{AssetObservationIdGenerator, AssetObservationStore, AssetTarget},
    domain::{Snapshot, TimestampMillis},
    ports::BlobStore,
    runtime::SystemClock,
    workflow::{
        AssetDeliveryConfig, DeliveryProjectionBuilder, DeliveryProjectionError,
        DeliveryProjectionStore, PublicProjection,
    },
};

use super::{
    DeliveryProjectionBinding, DeliveryPublicationExecuteError, DeliveryPublicationExecution,
    DeliveryPublicationExecutor, GitCommitMetadata, GitPublicationPrepareError,
    GitPublicationPrepareRequest, GitPublicationPreparer, GitRefTarget, GitRemoteAdapter,
    GitRemoteError, GitRemoteObserver, GitRepositoryAdapter, GitRepositoryAdapterError,
    GitRepositoryIdentity, GitRepositoryIdentityError, PublishRunId, PublishRunStore,
    PublishTargetId, RemoteObservationId, RemoteObservationIdGenerator, RemoteObservationStore,
    RemoteRefState,
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

/// A small caller-owned remote-observation allocator, suitable for tests and
/// single-process execution.
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
    delivery_sha256: crate::domain::Sha256,
    publish_plan_sha256: crate::domain::Sha256,
    workflow: DeliveryPublicationExecution,
}

impl GitPublicationApplicationResult {
    pub fn publish_run_id(&self) -> PublishRunId {
        self.publish_run_id
    }

    /// The logical public projection this attempt delivered.
    pub fn projection_sha256(&self) -> crate::domain::Sha256 {
        self.projection_sha256
    }

    /// The complete delivery decision: which documents, with which rewritten
    /// bytes, and where every referenced asset will physically live.
    pub fn delivery_sha256(&self) -> crate::domain::Sha256 {
        self.delivery_sha256
    }

    pub fn publish_plan_sha256(&self) -> crate::domain::Sha256 {
        self.publish_plan_sha256
    }

    /// The complete delivery verdict: Git facts plus the asset facts that had to
    /// hold before the Git target was allowed to become public.
    pub fn workflow(&self) -> &DeliveryPublicationExecution {
        &self.workflow
    }
}

#[derive(Debug)]
pub enum GitPublicationApplicationError<
    P: Error,
    D: Error,
    A: Error,
    S: Error,
    G: Error,
    O: Error,
    R: Error,
    I: Error,
> {
    RepositoryIdentity(GitRepositoryIdentityError),
    PreparationObservation(GitRemoteError),
    TargetMissing,
    RepositoryAdapter(GitRepositoryAdapterError),
    SystemClockUnavailable,
    PublishRunId(R),
    /// The delivery stage refused to derive a publishable text tree.
    Delivery(DeliveryProjectionError),
    /// The portable prepare stage refused the facts the runtime supplied.
    Prepare(GitPublicationPrepareError<GitRepositoryAdapterError>),
    /// The delivery intent could not be made durable before anything else happened.
    DeliveryPersistence(D),
    PublishRunPersistence(P),
    /// Recovery could not read the intent it was asked to continue.
    PublishRunLoad(P),
    /// Recovery was asked to continue an intent that does not exist.
    PublishRunMissing(PublishRunId),
    /// Binding the native adapters failed.
    RemoteAdapter(GitRemoteError),
    Workflow(
        DeliveryPublicationExecuteError<
            P,
            D,
            A,
            S,
            G,
            O,
            I,
            GitRepositoryAdapterError,
            GitRemoteError,
        >,
    ),
}

impl<P: Error, D: Error, A: Error, S: Error, G: Error, O: Error, R: Error, I: Error> fmt::Display
    for GitPublicationApplicationError<P, D, A, S, G, O, R, I>
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
            Self::RepositoryAdapter(error) => {
                write!(
                    formatter,
                    "could not open the Git repository adapter: {error}"
                )
            }
            Self::RemoteAdapter(error) => {
                write!(formatter, "could not open the Git remote adapter: {error}")
            }
            Self::SystemClockUnavailable => {
                formatter.write_str("system clock reading cannot be frozen as a commit timestamp")
            }
            Self::PublishRunId(_) => formatter.write_str("could not allocate publish run ID"),
            Self::Delivery(error) => {
                write!(
                    formatter,
                    "could not build the delivery projection: {error}"
                )
            }
            Self::Prepare(error) => {
                write!(
                    formatter,
                    "could not prepare the publication intent: {error}"
                )
            }
            Self::DeliveryPersistence(_) => {
                formatter.write_str("could not persist the delivery projection before publication")
            }
            Self::PublishRunPersistence(_) => {
                formatter.write_str("could not persist publish run before publication")
            }
            Self::PublishRunLoad(_) => formatter.write_str("could not load publish run to resume"),
            Self::PublishRunMissing(id) => {
                write!(formatter, "publish run {} was not found", id.get())
            }
            Self::Workflow(error) => error.fmt(formatter),
        }
    }
}

impl<
    P: Error + 'static,
    D: Error + 'static,
    A: Error + 'static,
    S: Error + 'static,
    G: Error + 'static,
    O: Error + 'static,
    R: Error + 'static,
    I: Error + 'static,
> Error for GitPublicationApplicationError<P, D, A, S, G, O, R, I>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RepositoryIdentity(error) => Some(error),
            Self::PreparationObservation(error) => Some(error),
            Self::RepositoryAdapter(error) => Some(error),
            Self::RemoteAdapter(error) => Some(error),
            Self::PublishRunId(error) => Some(error),
            Self::Delivery(error) => Some(error),
            Self::Prepare(error) => Some(error),
            Self::DeliveryPersistence(error) => Some(error),
            Self::PublishRunPersistence(error) => Some(error),
            Self::PublishRunLoad(error) => Some(error),
            Self::Workflow(error) => Some(error),
            Self::TargetMissing | Self::SystemClockUnavailable | Self::PublishRunMissing(_) => None,
        }
    }
}

impl GitPublicationApplication {
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn prepare_and_publish<P, D, T, S, G, O, R, I, B>(
        projection: &PublicProjection,
        snapshot: &Snapshot,
        delivery_config: &AssetDeliveryConfig,
        repository: impl AsRef<Path>,
        target_id: PublishTargetId,
        target: GitRefTarget,
        commit_metadata: &GitCommitMetadata,
        content_store: &B,
        publish_run_store: &P,
        delivery_projections: &D,
        asset_target: &T,
        asset_observations: &S,
        asset_observation_ids: &mut G,
        observation_store: &O,
        publish_run_ids: &mut R,
        observation_ids: &mut I,
    ) -> Result<
        GitPublicationApplicationResult,
        GitPublicationApplicationError<
            P::Error,
            D::Error,
            T::Error,
            S::Error,
            G::Error,
            O::Error,
            R::Error,
            I::Error,
        >,
    >
    where
        P: PublishRunStore,
        D: DeliveryProjectionStore,
        T: AssetTarget,
        S: AssetObservationStore,
        G: AssetObservationIdGenerator,
        O: RemoteObservationStore,
        R: PublishRunIdGenerator,
        I: RemoteObservationIdGenerator,
        B: BlobStore,
    {
        let repository = GitRepositoryIdentity::new(repository)
            .map_err(GitPublicationApplicationError::RepositoryIdentity)?;
        // The remote observation stays here: the portable prepare stage is told
        // which base the target holds instead of reaching for a remote itself.
        let observed = GitRemoteObserver::observe_target(repository.path(), &target)
            .map_err(GitPublicationApplicationError::PreparationObservation)?;
        let RemoteRefState::Present { commit_oid } = observed else {
            return Err(GitPublicationApplicationError::TargetMissing);
        };
        // The delivery split happens before any Git fact is read: the runtime is
        // handed the final text side, so binary assets and reference rewriting are
        // decided in the engine rather than by the Git adapter.
        let delivery =
            DeliveryProjectionBuilder::build(projection, snapshot, delivery_config, content_store)
                .map_err(GitPublicationApplicationError::Delivery)?;
        // Durable before any Git object is written and long before any remote is
        // touched: recovery may only ever rematerialize the projection the intent
        // bound, so that projection has to exist first. An orphan projection whose
        // run never gets saved is harmless — it is content-addressed and describes
        // an intent nothing acted on.
        delivery_projections
            .save(&delivery)
            .map_err(GitPublicationApplicationError::DeliveryPersistence)?;
        let delivery_binding = DeliveryProjectionBinding::from_projection(&delivery);
        let git_repository =
            GitRepositoryAdapter::from_locator(repository.locator(), content_store)
                .map_err(GitPublicationApplicationError::RepositoryAdapter)?;
        // One attempt instant, frozen before anything that depends on it. The
        // commit object and the publication intent share exactly this value, which
        // is what lets the persisted specification rebuild the commit later; a
        // configured timestamp pins that instant, otherwise it is the single clock
        // reading of this attempt.
        let created_at = match commit_metadata.timestamp() {
            Some(fixed) => TimestampMillis::from_system_time(fixed)
                .ok_or(GitPublicationApplicationError::SystemClockUnavailable)?,
            None => TimestampMillis::from_system_time(SystemTime::now())
                .ok_or(GitPublicationApplicationError::SystemClockUnavailable)?,
        };
        let publish_run_id = publish_run_ids
            .next_id()
            .map_err(GitPublicationApplicationError::PublishRunId)?;
        let preparation = GitPublicationPreparer::prepare(
            &git_repository,
            &GitPublicationPrepareRequest {
                id: publish_run_id,
                target_id: &target_id,
                repository: repository.locator(),
                target: &target,
                text_projection: delivery.text(),
                delivery: delivery_binding,
                observed_base: &commit_oid,
                author_name: commit_metadata.author_name(),
                author_email: commit_metadata.author_email(),
                message: commit_metadata.message(),
                created_at,
            },
        )
        .map_err(GitPublicationApplicationError::Prepare)?;
        let publish_plan_sha256 = preparation.publish_plan_sha256();
        let publish_run = preparation.into_publish_run();
        publish_run_store
            .save(&publish_run)
            .map_err(GitPublicationApplicationError::PublishRunPersistence)?;
        let remote = GitRemoteAdapter::new(repository.path())
            .map_err(GitPublicationApplicationError::RemoteAdapter)?;
        let workflow = DeliveryPublicationExecutor::execute(
            publish_run_id,
            publish_run_store,
            delivery_projections,
            asset_target,
            asset_observations,
            asset_observation_ids,
            content_store,
            observation_store,
            observation_ids,
            &git_repository,
            &remote,
            &SystemClock,
        )
        .map_err(GitPublicationApplicationError::Workflow)?;
        Ok(GitPublicationApplicationResult {
            publish_run_id,
            projection_sha256: projection.projection_sha256(),
            delivery_sha256: delivery.delivery_sha256(),
            publish_plan_sha256,
            workflow,
        })
    }

    /// Continues one persisted attempt.
    ///
    /// Recovery needs the same runtime bindings preparation used: the intent only
    /// stores an opaque repository locator, so the runtime resolves it back into a
    /// repository (with its immutable blob source) and a remote. The intent itself
    /// is reloaded by the executor, which is the only reader that decides what to
    /// publish.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn resume<P, D, T, S, G, O, I, B>(
        publish_run_id: PublishRunId,
        publish_run_store: &P,
        delivery_projections: &D,
        asset_target: &T,
        asset_observations: &S,
        asset_observation_ids: &mut G,
        observation_store: &O,
        observation_ids: &mut I,
        content_store: &B,
    ) -> Result<
        DeliveryPublicationExecution,
        GitPublicationApplicationError<
            P::Error,
            D::Error,
            T::Error,
            S::Error,
            G::Error,
            O::Error,
            std::convert::Infallible,
            I::Error,
        >,
    >
    where
        P: PublishRunStore,
        D: DeliveryProjectionStore,
        T: AssetTarget,
        S: AssetObservationStore,
        G: AssetObservationIdGenerator,
        O: RemoteObservationStore,
        I: RemoteObservationIdGenerator,
        B: BlobStore,
    {
        // Binding the adapters needs the locator the intent persisted. This read is
        // plumbing, not a trust decision: the executor reloads the intent itself
        // before anything can reach a remote.
        let publish_run = publish_run_store
            .get(publish_run_id)
            .map_err(GitPublicationApplicationError::PublishRunLoad)?
            .ok_or(GitPublicationApplicationError::PublishRunMissing(
                publish_run_id,
            ))?;
        let git_repository =
            GitRepositoryAdapter::from_locator(publish_run.repository(), content_store)
                .map_err(GitPublicationApplicationError::RepositoryAdapter)?;
        let remote = GitRemoteAdapter::from_locator(publish_run.repository())
            .map_err(GitPublicationApplicationError::RemoteAdapter)?;
        DeliveryPublicationExecutor::execute(
            publish_run_id,
            publish_run_store,
            delivery_projections,
            asset_target,
            asset_observations,
            asset_observation_ids,
            content_store,
            observation_store,
            observation_ids,
            &git_repository,
            &remote,
            &SystemClock,
        )
        .map_err(GitPublicationApplicationError::Workflow)
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
        asset::{AssetObservationId, FilesystemAssetTarget, SequentialAssetObservationIdGenerator},
        domain::{ContentPath, Snapshot, SnapshotFile, SnapshotId, SourceId},
        publisher::{
            GitCommitObjectCreator, GitPublicationExecution, PublishRun, PublishRunPublication,
            SequentialRemoteObservationIdGenerator,
        },
        storage::{
            SqliteAssetObservationStore, SqliteDeliveryProjectionStore, SqlitePublishRunStore,
            SqliteRemoteObservationStore,
        },
        workflow::{DeliveryProjectionBuilder, FinalPublicationSet, ManagedRoot, PublicProjection},
    };

    use super::*;
    use crate::storage::LocalContentStore;

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

        fn projection(&self, store: &LocalContentStore) -> (Snapshot, PublicProjection) {
            self.projection_of(store, &[("note.md", b"new public content")])
        }

        /// Builds the complete desired state for an explicit set of documents, so
        /// a projection can also be made identical to what the remote already
        /// holds. The snapshot travels with it because the delivery stage resolves
        /// document references against that immutable source state.
        fn projection_of(
            &self,
            store: &LocalContentStore,
            entries: &[(&str, &[u8])],
        ) -> (Snapshot, PublicProjection) {
            let files = entries
                .iter()
                .map(|(path, bytes)| {
                    let identity = store.store(bytes).unwrap();
                    SnapshotFile::new(
                        ContentPath::new(*path).unwrap(),
                        bytes.len() as u64,
                        identity,
                        None,
                    )
                })
                .collect::<Vec<_>>();
            let snapshot = Snapshot::new(
                SnapshotId::new(1).unwrap(),
                SystemTime::UNIX_EPOCH,
                SourceId::new("test").unwrap(),
                files,
            )
            .unwrap();
            let set = FinalPublicationSet::from_parts_for_test(
                snapshot.id(),
                entries
                    .iter()
                    .map(|(path, _)| ContentPath::new(*path).unwrap())
                    .collect(),
                vec![],
            );
            let projection =
                PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap())
                    .unwrap();
            (snapshot, projection)
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

    fn target() -> GitRefTarget {
        GitRefTarget::new("origin", "refs/heads/main").unwrap()
    }

    fn target_id() -> PublishTargetId {
        PublishTargetId::new("origin:refs/heads/main").unwrap()
    }

    fn delivery_config() -> AssetDeliveryConfig {
        AssetDeliveryConfig::new("https://assets.example.com").unwrap()
    }

    #[derive(Debug)]
    struct SaveFailure;
    impl fmt::Display for SaveFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("save failed")
        }
    }
    impl Error for SaveFailure {}

    /// A delivery projection store that refuses every write, so the ordering
    /// between durable delivery intent and any remote effect can be pinned.
    struct RejectingDeliveryProjectionStore;

    impl crate::workflow::DeliveryProjectionStore for RejectingDeliveryProjectionStore {
        type Error = SaveFailure;

        fn save(&self, _: &crate::workflow::DeliveryProjection) -> Result<(), Self::Error> {
            Err(SaveFailure)
        }

        fn get(
            &self,
            _: crate::domain::Sha256,
        ) -> Result<Option<crate::workflow::DeliveryProjection>, Self::Error> {
            Ok(None)
        }
    }

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
        fn list_for_target(&self, _: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error> {
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

        fn list_for_target(&self, target: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error> {
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
        fn list_for_target(&self, _: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error> {
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
        let (snapshot, projection) = repository.projection(&store);
        let runs = MemoryPublishRunStore::default();
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            GitRefTarget::new("origin", "refs/heads/missing").unwrap(),
            &metadata(),
            &store,
            &runs,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
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

    /// §9: the delivery intent must be durable before anything else can happen, so
    /// a store that cannot capture it stops the attempt with zero remote effects.
    #[test]
    fn delivery_persistence_failure_prevents_any_publication() {
        let repository = TestRepository::new();
        let before = repository.remote_oid();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let (snapshot, projection) = repository.projection(&store);
        let runs = SqlitePublishRunStore::open(repository.root.join("runs.sqlite")).unwrap();
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            target(),
            &metadata(),
            &store,
            &runs,
            &RejectingDeliveryProjectionStore,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        );

        assert!(matches!(
            result,
            Err(GitPublicationApplicationError::DeliveryPersistence(_))
        ));
        assert_eq!(repository.remote_oid(), before);
        assert!(runs.list().unwrap().is_empty());
        // Nothing was captured, because the delivery stage refused first.
        let expected =
            DeliveryProjectionBuilder::build(&projection, &snapshot, &delivery_config(), &store)
                .unwrap();
        assert_eq!(deliveries.get(expected.delivery_sha256()).unwrap(), None);
    }

    #[test]
    fn persistence_failure_prevents_any_remote_push() {
        let repository = TestRepository::new();
        let before = repository.remote_oid();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let (snapshot, projection) = repository.projection(&store);
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            target(),
            &metadata(),
            &store,
            &RejectingPublishRunStore,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        );

        assert!(matches!(
            result,
            Err(GitPublicationApplicationError::PublishRunPersistence(_))
        ));
        assert_eq!(repository.remote_oid(), before);
        // The delivery intent was made durable first and is now an orphan: that is
        // explicitly allowed, because a content-addressed projection nothing acted
        // on does not constitute a publication.
        let expected =
            DeliveryProjectionBuilder::build(&projection, &snapshot, &delivery_config(), &store)
                .unwrap();
        assert_eq!(
            deliveries.get(expected.delivery_sha256()).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn remote_advance_after_preparation_is_reconciled_without_an_overwrite() {
        let repository = TestRepository::new();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let (snapshot, projection) = repository.projection(&store);
        let runs = AdvancingPublishRunStore {
            inner: SqlitePublishRunStore::open(repository.root.join("runs.sqlite")).unwrap(),
            repository: repository.local.clone(),
        };
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("obs.sqlite")).unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            target(),
            &metadata(),
            &store,
            &runs,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        )
        .unwrap();

        assert!(matches!(
            result.workflow().git(),
            GitPublicationExecution::RemoteChanged { .. }
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
        let (snapshot, projection) = repository.projection(&store);
        let run_db = repository.root.join("runs.sqlite");
        let observation_db = repository.root.join("observations.sqlite");
        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let observations = SqliteRemoteObservationStore::open(&observation_db).unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            target(),
            &metadata(),
            &store,
            &runs,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        )
        .unwrap();
        assert!(matches!(
            result.workflow().git(),
            GitPublicationExecution::Published { .. }
        ));
        // The required asset was published and verified before the push.
        assert!(result.workflow().is_satisfied());
        let published = repository.remote_oid();
        assert_ne!(
            published,
            git_stdout(&repository.local, &["rev-parse", "HEAD"])
        );
        drop(observations);
        drop(runs);

        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let observations = SqliteRemoteObservationStore::open(&observation_db).unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut recovery_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(3).unwrap());
        let recovered = GitPublicationApplication::resume(
            result.publish_run_id(),
            &runs,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut recovery_ids,
            &store,
        )
        .unwrap();
        assert!(matches!(
            recovered.git(),
            GitPublicationExecution::AlreadyPublished { .. }
        ));
        assert!(recovered.is_satisfied());
        assert_eq!(repository.remote_oid(), published);
    }

    /// The boundary this step establishes for real publications: a new non-Noop
    /// run persists the exact specification that created its commit, so the commit
    /// can be rebuilt from durable data alone — no configuration is consulted.
    #[test]
    fn a_published_run_persists_the_exact_specification_that_created_its_commit() {
        let repository = TestRepository::new();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        let (snapshot, projection) = repository.projection(&store);
        let run_db = repository.root.join("runs.sqlite");
        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("observations.sqlite"))
                .unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            target(),
            &metadata(),
            &store,
            &runs,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        )
        .unwrap();
        assert!(matches!(
            result.workflow().git(),
            GitPublicationExecution::Published { .. }
        ));
        // The required asset was published and verified before the push.
        assert!(result.workflow().is_satisfied());
        drop(observations);
        drop(runs);

        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let run = runs.get(result.publish_run_id()).unwrap().unwrap();
        let spec = run
            .commit_spec()
            .expect("a new non-Noop run freezes the identity of its commit");
        assert_eq!(spec.parent().as_str(), run.base_commit());
        assert_eq!(spec.tree().as_str(), run.reviewed_tree());
        assert_eq!(
            spec.author_time().as_unix_millis(),
            run.created_at_unix_ms()
        );
        assert_eq!(
            spec.committer_time().as_unix_millis(),
            run.created_at_unix_ms()
        );

        // Rebuilding the commit from the persisted specification alone must
        // reproduce the very object that was pushed.
        let rebuilt = GitCommitObjectCreator::create_from_spec(&repository.local, spec).unwrap();
        assert_eq!(Some(rebuilt), run.desired_commit().cloned());
        assert_eq!(
            repository.remote_oid(),
            run.desired_commit().unwrap().as_str()
        );
    }

    /// The other half of the boundary: a Noop run wants nothing and freezes
    /// nothing, but is still persisted.
    #[test]
    fn a_noop_publish_persists_an_intent_without_a_commit_or_a_specification() {
        let repository = TestRepository::new();
        let before = repository.remote_oid();
        let store = LocalContentStore::new(repository.root.join("content-store"));
        // The complete desired state already equals what the target holds.
        let (snapshot, projection) = repository.projection_of(&store, &[("old.md", b"old")]);
        let run_db = repository.root.join("runs.sqlite");
        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let observations =
            SqliteRemoteObservationStore::open(repository.root.join("observations.sqlite"))
                .unwrap();
        let deliveries =
            SqliteDeliveryProjectionStore::open(repository.root.join("deliveries.sqlite")).unwrap();
        let asset_target = FilesystemAssetTarget::new(repository.root.join("asset-target"));
        let asset_observations =
            SqliteAssetObservationStore::open(repository.root.join("asset-observations.sqlite"))
                .unwrap();
        let mut asset_observation_ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let mut run_ids = SequentialPublishRunIdGenerator::new(PublishRunId::new(1).unwrap());
        let mut observation_ids =
            SequentialRemoteObservationIdGenerator::new(RemoteObservationId::new(1).unwrap());

        let result = GitPublicationApplication::prepare_and_publish(
            &projection,
            &snapshot,
            &delivery_config(),
            &repository.local,
            target_id(),
            target(),
            &metadata(),
            &store,
            &runs,
            &deliveries,
            &asset_target,
            &asset_observations,
            &mut asset_observation_ids,
            &observations,
            &mut run_ids,
            &mut observation_ids,
        )
        .unwrap();
        assert!(matches!(
            result.workflow().git(),
            GitPublicationExecution::NoopSatisfied { .. }
        ));
        // The Git target needed nothing, and the asset side still had to hold.
        assert!(result.workflow().is_satisfied());
        drop(observations);
        drop(runs);

        let runs = SqlitePublishRunStore::open(&run_db).unwrap();
        let run = runs.get(result.publish_run_id()).unwrap().unwrap();
        assert_eq!(run.desired_commit(), None);
        assert_eq!(run.commit_spec(), None);
        assert_eq!(run.publication(), PublishRunPublication::Noop);
        assert_eq!(repository.remote_oid(), before);
        assert_eq!(
            git_stdout(&repository.local, &["rev-parse", "HEAD"]),
            before
        );
    }
}
