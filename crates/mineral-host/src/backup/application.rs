use std::{error::Error, fmt};

use crate::{
    domain::{Snapshot, SnapshotId, TimestampMillis},
    ports::BlobStore,
    publication::git::{GitCommitOid, GitCommitSpec, GitRefTarget, GitRemote, RemoteRefState},
    publisher::GitCommitMetadata,
};
use mineral_core::backup::{
    BackupDeliveryError, BackupExecutionError, BackupExecutionOutcome, BackupGitRepository,
    BackupProjection, BackupProjectionError, BackupRun, BackupRunError, BackupRunId,
    BackupRunStore, LfsRemote, TypeFirstBackupRepresentationPolicy, build_backup_delivery,
    build_backup_tree, execute_backup,
};

use super::git_backup::GitBackupError;

/// Allocates immutable backup-attempt identities at the application boundary.
pub trait BackupRunIdGenerator {
    fn next_id(&self) -> BackupRunId;
}

/// A deterministic generator, for tests and single-shot runtimes.
#[derive(Clone, Debug)]
pub struct SequentialBackupRunIdGenerator {
    next: std::cell::Cell<u64>,
}

impl SequentialBackupRunIdGenerator {
    pub fn new(first: BackupRunId) -> Self {
        Self {
            next: std::cell::Cell::new(first.get()),
        }
    }
}

impl BackupRunIdGenerator for SequentialBackupRunIdGenerator {
    fn next_id(&self) -> BackupRunId {
        let value = self.next.get();
        self.next.set(value + 1);
        BackupRunId::new(value).expect("a sequential backup id is non-zero")
    }
}

/// Everything one backup attempt needs that is not a port.
pub struct BackupApplicationRequest<'a> {
    pub progress: &'a dyn crate::runtime::Progress,
    pub snapshot: &'a Snapshot,
    pub target: &'a GitRefTarget,
    pub commit_metadata: &'a GitCommitMetadata,
    pub policy: &'a TypeFirstBackupRepresentationPolicy,
    /// Frozen before the commit exists, so a retry cannot invent a second identity.
    pub created_at: TimestampMillis,
}

/// What one backup attempt did, with the identities a report needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupApplicationOutcome {
    /// The durable intent this attempt recorded, or `None` when the backup was
    /// already up to date and nothing new was recorded.
    run_id: Option<BackupRunId>,
    snapshot_id: SnapshotId,
    backup_projection_sha256: crate::domain::Sha256,
    delivery_sha256: crate::domain::Sha256,
    files: usize,
    lfs_objects: usize,
    execution: BackupExecutionOutcome,
}

impl BackupApplicationOutcome {
    pub fn run_id(&self) -> Option<BackupRunId> {
        self.run_id
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn backup_projection_sha256(&self) -> crate::domain::Sha256 {
        self.backup_projection_sha256
    }

    pub fn delivery_sha256(&self) -> crate::domain::Sha256 {
        self.delivery_sha256
    }

    pub fn files(&self) -> usize {
        self.files
    }

    pub fn lfs_objects(&self) -> usize {
        self.lfs_objects
    }

    pub fn execution(&self) -> &BackupExecutionOutcome {
        &self.execution
    }
}

/// Runs one backup: projections, tree, commit, durable intent, then the ordered
/// remote effects (LFS first, exact Git compare-and-swap last).
///
/// Nothing remote happens before the intent is durable, and no ref moves before the
/// LFS endpoint confirms every required object. A failure at any point leaves the
/// Git ref exactly where it was.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn run_backup<S, R, M, L, B>(
    request: BackupApplicationRequest<'_>,
    store: &S,
    repository: &R,
    remote: &M,
    lfs: &L,
    blobs: &B,
    run_ids: &dyn BackupRunIdGenerator,
) -> Result<BackupApplicationOutcome, BackupApplicationError<S::Error, R::Error, M::Error, L::Error>>
where
    S: BackupRunStore,
    R: BackupGitRepository,
    M: GitRemote,
    L: LfsRemote,
    B: BlobStore,
{
    request
        .progress
        .stage("Backup: build complete snapshot projection");
    let projection =
        BackupProjection::build(request.snapshot).map_err(BackupApplicationError::Projection)?;
    request
        .progress
        .stage("Backup: materialize files and Git LFS pointers");
    let delivery = build_backup_delivery(&projection, request.policy, blobs)
        .map_err(BackupApplicationError::Delivery)?;
    request.progress.detail(&format!(
        "{} files; {} required LFS objects",
        delivery.files().len(),
        delivery.required_lfs_objects().len()
    ));
    let tree = build_backup_tree(&delivery).map_err(BackupApplicationError::Delivery)?;

    // The base is observed, never chosen: a backup either continues the ref it found
    // or refuses to move it.
    request.progress.stage("Backup: observe remote base commit");
    let base_commit = match remote
        .observe_ref(request.target)
        .map_err(BackupApplicationError::Remote)?
    {
        RemoteRefState::Present { commit_oid } => commit_oid,
        RemoteRefState::Missing => {
            return Err(BackupApplicationError::NoBaseCommit(request.target.clone()));
        }
    };
    request
        .progress
        .stage("Backup: materialize and validate exact Git tree");
    let reviewed = repository
        .materialize_backup(&base_commit, &tree)
        .map_err(BackupApplicationError::Repository)?;

    // An unchanged Snapshot materializes exactly the tree the base commit already
    // holds. Creating a commit for it would advance the branch with an empty diff on
    // every run, so the attempt reports the existing backup and records nothing.
    if reviewed.is_noop() {
        return Ok(BackupApplicationOutcome {
            run_id: None,
            snapshot_id: projection.snapshot_id(),
            backup_projection_sha256: projection.projection_sha256(),
            delivery_sha256: delivery.delivery_sha256(),
            files: delivery.files().len(),
            lfs_objects: delivery.required_lfs_objects().len(),
            execution: BackupExecutionOutcome::AlreadyBackedUp {
                commit: base_commit,
            },
        });
    }

    let spec = GitCommitSpec::new(
        base_commit.clone(),
        reviewed.tree_oid().clone(),
        request.commit_metadata.author_name(),
        request.commit_metadata.author_email(),
        request.created_at,
        request.commit_metadata.author_name(),
        request.commit_metadata.author_email(),
        request.created_at,
        request.commit_metadata.message(),
    )
    .map_err(BackupApplicationError::CommitSpec)?;
    request
        .progress
        .stage("Backup: create commit from validated tree");
    let desired_commit = repository
        .create_commit(&spec)
        .map_err(BackupApplicationError::Repository)?;

    request.progress.detail(&format!(
        "Base {}; reviewed tree {}; commit {}",
        base_commit.as_str(),
        reviewed.tree_oid().as_str(),
        desired_commit.as_str()
    ));
    let run_id = run_ids.next_id();
    let run = BackupRun::new(
        run_id,
        projection.snapshot_id(),
        projection.projection_sha256(),
        delivery.clone(),
        base_commit,
        desired_commit,
        request.target.clone(),
        spec,
        request.created_at.as_unix_millis(),
    )
    .map_err(BackupApplicationError::Run)?;
    request
        .progress
        .detail(&format!("Backup run {}", run_id.get()));
    // Durable before any remote side effect: a restart resumes this attempt.
    request
        .progress
        .stage("Backup: persist immutable intent before remote effects");
    store.save(&run).map_err(BackupApplicationError::Store)?;

    request
        .progress
        .stage("Backup: deliver required LFS objects and compare-and-swap remote ref");
    let execution = execute_backup(store, repository, remote, lfs, blobs, run_id)
        .map_err(BackupApplicationError::Execution)?;

    Ok(BackupApplicationOutcome {
        run_id: Some(run_id),
        snapshot_id: projection.snapshot_id(),
        backup_projection_sha256: projection.projection_sha256(),
        delivery_sha256: delivery.delivery_sha256(),
        files: delivery.files().len(),
        lfs_objects: delivery.required_lfs_objects().len(),
        execution,
    })
}

/// Why one backup attempt stopped before or during its remote effects.
#[derive(Debug)]
pub enum BackupApplicationError<StoreError, RepositoryError, RemoteError, LfsError> {
    Projection(BackupProjectionError),
    Delivery(BackupDeliveryError),
    Repository(RepositoryError),
    Remote(RemoteError),
    CommitSpec(crate::publication::git::GitCommitSpecError),
    Run(BackupRunError),
    Store(StoreError),
    Execution(BackupExecutionError<StoreError, RepositoryError, RemoteError, LfsError>),
    /// The backup ref does not exist yet, so there is no base to build on.
    NoBaseCommit(GitRefTarget),
}

impl<S: fmt::Display, R: fmt::Display, M: fmt::Display, L: fmt::Display> fmt::Display
    for BackupApplicationError<S, R, M, L>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projection(error) => {
                write!(formatter, "could not build the backup projection: {error}")
            }
            Self::Delivery(error) => {
                write!(formatter, "could not build the backup delivery: {error}")
            }
            Self::Repository(error) => write!(formatter, "Git object database failed: {error}"),
            Self::Remote(error) => write!(formatter, "Git remote failed: {error}"),
            Self::CommitSpec(error) => {
                write!(formatter, "backup commit identity is unusable: {error}")
            }
            Self::Run(error) => write!(formatter, "backup intent is contradictory: {error}"),
            Self::Store(error) => write!(formatter, "could not persist the backup intent: {error}"),
            Self::Execution(error) => write!(formatter, "{error}"),
            Self::NoBaseCommit(target) => write!(
                formatter,
                "backup ref {}/{} does not exist yet; create it once before the first backup",
                target.remote_name(),
                target.destination_ref()
            ),
        }
    }
}

impl<S, R, M, L> Error for BackupApplicationError<S, R, M, L>
where
    S: Error + 'static,
    R: Error + 'static,
    M: Error + 'static,
    L: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(error) => Some(error),
            Self::Delivery(error) => Some(error),
            Self::Repository(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::CommitSpec(error) => Some(error),
            Self::Run(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Execution(error) => Some(error),
            Self::NoBaseCommit(_) => None,
        }
    }
}

/// Exposes the Git-side error type in application error signatures.
pub type BackedUpRepositoryError = GitBackupError;

/// Convenience alias for the outcome of a full backup application run.
pub type BackupRunOutcome = BackupApplicationOutcome;

/// The commit a backup would build on, or `None` when the ref is missing.
pub fn backup_base_commit(
    remote: &impl GitRemote,
    target: &GitRefTarget,
) -> Result<Option<GitCommitOid>, String> {
    match remote
        .observe_ref(target)
        .map_err(|error| error.to_string())?
    {
        RemoteRefState::Present { commit_oid } => Ok(Some(commit_oid)),
        RemoteRefState::Missing => Ok(None),
    }
}
