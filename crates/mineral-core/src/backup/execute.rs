use std::{error::Error, fmt};

use crate::{
    domain::{ContentPath, Sha256},
    ports::{BlobStore, ContentStoreError},
    publication::git::{
        CasOutcome, GitCommitOid, GitRefTarget, GitRemote, GitTreeOid, LocalCommitState, RefUpdate,
        RemoteRefState,
    },
};

use super::{
    delivery::{
        BACKUP_GIT_ATTRIBUTES_PATH, BACKUP_MANIFEST_PATH, BackupDeliveryError, BackupManifest,
        BackupManifestError, BackupRepresentationKind, verify_manifest_pointer,
    },
    lfs::{LfsRemote, RequiredLfsObject},
    manifest::ManifestPathMismatch,
    run::{BackupRun, BackupRunId, BackupRunStore},
    tree::{BackupGitRepository, BackupTreeReader, build_backup_tree},
};

/// What one backup attempt did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupExecutionOutcome {
    /// The commit was created, every required LFS object is present, and the ref moved.
    BackedUp {
        commit: GitCommitOid,
        uploaded: Vec<RequiredLfsObject>,
        already_present: usize,
    },
    /// The target ref already held exactly the frozen commit.
    AlreadyBackedUp { commit: GitCommitOid },
    /// The target ref no longer holds the base the intent froze.
    RemoteChanged {
        expected: GitCommitOid,
        observed: RemoteRefState,
    },
}

/// Executes one frozen backup intent.
///
/// The order is the safety property, not an implementation detail:
///
/// ```text
/// reconcile the ref (equal to the frozen commit already? -> AlreadyBackedUp)
///   -> confirm the frozen commit exists, or rebuild it from the frozen delivery
///   -> ask the LFS endpoint what it holds
///   -> stream only the missing objects, each verified while streaming
///   -> ask again and require that nothing is missing
///   -> only then compare-and-swap the Git ref
/// ```
///
/// Invariant 3 lives here: the ref never moves while a required LFS object is
/// missing, so a commit that exists is always a commit that can be restored.
#[allow(clippy::type_complexity)]
pub fn execute_backup<S, R, M, L, B>(
    store: &S,
    repository: &R,
    remote: &M,
    lfs: &L,
    blobs: &B,
    run_id: BackupRunId,
) -> Result<BackupExecutionOutcome, BackupExecutionError<S::Error, R::Error, M::Error, L::Error>>
where
    S: BackupRunStore,
    R: BackupGitRepository,
    M: GitRemote,
    L: LfsRemote,
    B: BlobStore,
{
    let Some(run) = store.get(run_id).map_err(BackupExecutionError::Store)? else {
        return Err(BackupExecutionError::UnknownRun(run_id));
    };

    // 1. Reconcile with the remote before doing anything with side effects.
    let observed = remote
        .observe_ref(run.target())
        .map_err(BackupExecutionError::Remote)?;
    match &observed {
        RemoteRefState::Present { commit_oid } if commit_oid == run.desired_commit() => {
            require_lfs_complete(lfs, run.required_lfs_objects())?;
            return Ok(BackupExecutionOutcome::AlreadyBackedUp {
                commit: run.desired_commit().clone(),
            });
        }
        RemoteRefState::Present { commit_oid } if commit_oid == run.base_commit() => {}
        _ => {
            return Ok(BackupExecutionOutcome::RemoteChanged {
                expected: run.base_commit().clone(),
                observed,
            });
        }
    }

    // 2. The frozen commit must exist locally, or be rebuildable from the frozen
    //    delivery. A commit naming another tree or parent is never trusted.
    ensure_commit(repository, &run)?;

    // 3. LFS first, and only the objects the endpoint is missing.
    let plan = lfs
        .prepare_upload(run.required_lfs_objects())
        .map_err(BackupExecutionError::Lfs)?;
    let mut uploaded = Vec::new();
    for upload in plan.uploads() {
        let object = upload.object();
        let mut source = blobs.open(object.oid()).map_err(|error| match error {
            ContentStoreError::Missing(oid) => BackupExecutionError::SourceBlobMissing { oid },
            other => BackupExecutionError::Blob(other),
        })?;
        lfs.upload(&object, source.as_mut(), upload.action())
            .map_err(BackupExecutionError::Lfs)?;
        if let Some(action) = upload.verify() {
            lfs.verify(&object, action)
                .map_err(BackupExecutionError::Lfs)?;
        }
        uploaded.push(object);
    }

    // 4. The gate: nothing may be missing now, or the ref stays where it is.
    require_lfs_complete(lfs, run.required_lfs_objects())?;

    // 5. Exact compare-and-swap, last.
    let update = RefUpdate::new(
        run.target().clone(),
        run.base_commit().clone(),
        run.desired_commit().clone(),
    );
    match remote
        .compare_and_swap(&update)
        .map_err(BackupExecutionError::Remote)?
    {
        CasOutcome::Updated => Ok(BackupExecutionOutcome::BackedUp {
            commit: run.desired_commit().clone(),
            uploaded,
            already_present: plan.present().len(),
        }),
        CasOutcome::Rejected => {
            // The authoritative fact after a rejected swap is a fresh observation.
            let observed = remote
                .observe_ref(run.target())
                .map_err(BackupExecutionError::Remote)?;
            if matches!(&observed, RemoteRefState::Present { commit_oid } if commit_oid == run.desired_commit())
            {
                return Ok(BackupExecutionOutcome::AlreadyBackedUp {
                    commit: run.desired_commit().clone(),
                });
            }
            Ok(BackupExecutionOutcome::RemoteChanged {
                expected: run.base_commit().clone(),
                observed,
            })
        }
    }
}

/// Confirms the frozen commit exists with exactly the frozen parent and tree, or
/// rebuilds it deterministically from the frozen delivery.
fn ensure_commit<R, S, M, L>(
    repository: &R,
    run: &BackupRun,
) -> Result<(), BackupExecutionError<S, R::Error, M, L>>
where
    R: BackupGitRepository,
{
    match repository
        .inspect_commit(run.desired_commit())
        .map_err(BackupExecutionError::Repository)?
    {
        LocalCommitState::Present(facts) => {
            if facts.parent() != run.base_commit() || facts.tree() != run.commit_spec().tree() {
                return Err(BackupExecutionError::CommitFactsMismatch {
                    commit: run.desired_commit().clone(),
                });
            }
            return Ok(());
        }
        LocalCommitState::Missing => {}
    }

    let tree = build_backup_tree(run.delivery()).map_err(BackupExecutionError::Tree)?;
    let reviewed = repository
        .materialize_backup(run.base_commit(), &tree)
        .map_err(BackupExecutionError::Repository)?;
    if reviewed.tree_oid() != run.commit_spec().tree() {
        return Err(BackupExecutionError::MaterializedTreeMismatch {
            expected: run.commit_spec().tree().clone(),
            actual: reviewed.tree_oid().clone(),
        });
    }
    let rebuilt = repository
        .create_commit(run.commit_spec())
        .map_err(BackupExecutionError::Repository)?;
    if &rebuilt != run.desired_commit() {
        return Err(BackupExecutionError::RebuiltCommitMismatch {
            expected: run.desired_commit().clone(),
            actual: rebuilt,
        });
    }
    Ok(())
}

/// The final gate before any ref update.
fn require_lfs_complete<L, S, R, M>(
    lfs: &L,
    required: &[RequiredLfsObject],
) -> Result<(), BackupExecutionError<S, R, M, L::Error>>
where
    L: LfsRemote,
{
    let plan = lfs
        .prepare_upload(required)
        .map_err(BackupExecutionError::Lfs)?;
    if plan.is_complete() {
        return Ok(());
    }
    Err(BackupExecutionError::LfsObjectsMissing {
        missing: plan.missing(),
    })
}

/// Why a backup attempt stopped.
#[derive(Debug)]
pub enum BackupExecutionError<StoreError, RepositoryError, RemoteError, LfsError> {
    Store(StoreError),
    UnknownRun(BackupRunId),
    Repository(RepositoryError),
    Remote(RemoteError),
    Lfs(LfsError),
    Blob(ContentStoreError),
    /// A required object's bytes are not in the content store at all.
    SourceBlobMissing {
        oid: Sha256,
    },
    /// The frozen commit exists but names another tree or parent.
    CommitFactsMismatch {
        commit: GitCommitOid,
    },
    /// The materialized tree is not the tree the frozen commit names.
    MaterializedTreeMismatch {
        expected: GitTreeOid,
        actual: GitTreeOid,
    },
    /// Rebuilding the commit did not produce the frozen commit identity.
    RebuiltCommitMismatch {
        expected: GitCommitOid,
        actual: GitCommitOid,
    },
    /// The endpoint still reports missing objects, so the ref must not move.
    LfsObjectsMissing {
        missing: Vec<RequiredLfsObject>,
    },
    /// The frozen delivery could not be turned into a tree.
    Tree(BackupDeliveryError),
}

impl<S: fmt::Display, R: fmt::Display, M: fmt::Display, L: fmt::Display> fmt::Display
    for BackupExecutionError<S, R, M, L>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "could not read the backup intent: {error}"),
            Self::UnknownRun(id) => write!(formatter, "backup run {} does not exist", id.get()),
            Self::Repository(error) => write!(formatter, "Git object database failed: {error}"),
            Self::Remote(error) => write!(formatter, "Git remote failed: {error}"),
            Self::Lfs(error) => write!(formatter, "Git LFS endpoint failed: {error}"),
            Self::Blob(error) => write!(formatter, "content store failed: {error}"),
            Self::SourceBlobMissing { oid } => {
                write!(
                    formatter,
                    "required LFS object is not in the content store: {oid}"
                )
            }
            Self::CommitFactsMismatch { commit } => write!(
                formatter,
                "commit {} does not carry the frozen tree and parent",
                commit.as_str()
            ),
            Self::MaterializedTreeMismatch { expected, actual } => write!(
                formatter,
                "materialized tree {} is not the frozen tree {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::RebuiltCommitMismatch { expected, actual } => write!(
                formatter,
                "rebuilt commit {} is not the frozen commit {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::LfsObjectsMissing { missing } => write!(
                formatter,
                "{} required LFS object(s) are missing; the backup ref was not moved",
                missing.len()
            ),
            Self::Tree(error) => write!(formatter, "could not build the backup tree: {error}"),
        }
    }
}

impl<S, R, M, L> Error for BackupExecutionError<S, R, M, L>
where
    S: Error + 'static,
    R: Error + 'static,
    M: Error + 'static,
    L: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Repository(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::Lfs(error) => Some(error),
            Self::Blob(error) => Some(error),
            Self::Tree(error) => Some(error),
            _ => None,
        }
    }
}

/// What one `backup verify` proved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupVerificationReport {
    commit: GitCommitOid,
    files_verified: usize,
    lfs_objects_verified: usize,
}

impl BackupVerificationReport {
    pub fn commit(&self) -> &GitCommitOid {
        &self.commit
    }

    pub fn files_verified(&self) -> usize {
        self.files_verified
    }

    pub fn lfs_objects_verified(&self) -> usize {
        self.lfs_objects_verified
    }
}

/// Verifies that one backup commit can be restored byte-for-byte.
///
/// It reads the commit's manifest and tree, re-hashes every Git-side blob, checks
/// every LFS pointer against its manifest entry, and finally asks the LFS endpoint
/// whether it still holds every required object. The manifest is only a claim: the
/// tree and the bytes are the evidence.
#[allow(clippy::type_complexity)]
pub fn verify_backup<Rd, M, L>(
    reader: &Rd,
    remote: &M,
    lfs: &L,
    target: &GitRefTarget,
    commit: Option<&GitCommitOid>,
) -> Result<BackupVerificationReport, BackupVerificationError<Rd::Error, M::Error, L::Error>>
where
    Rd: BackupTreeReader,
    M: GitRemote,
    L: LfsRemote,
{
    let commit = match commit {
        Some(commit) => commit.clone(),
        None => match remote
            .observe_ref(target)
            .map_err(BackupVerificationError::Remote)?
        {
            RemoteRefState::Present { commit_oid } => commit_oid,
            RemoteRefState::Missing => return Err(BackupVerificationError::NothingToVerify),
        },
    };

    let manifest_path = ContentPath::new(BACKUP_MANIFEST_PATH).expect("the manifest path is valid");
    let manifest_bytes = reader
        .read_blob(&commit, &manifest_path)
        .map_err(BackupVerificationError::Reader)?
        .ok_or(BackupVerificationError::ManifestMissing)?;
    let manifest =
        BackupManifest::parse(&manifest_bytes).map_err(BackupVerificationError::Manifest)?;

    let expected_vault_paths = manifest
        .entries()
        .iter()
        .map(|entry| super::projection::BackupProjection::tree_path(entry.path()))
        .collect::<Vec<_>>();
    let tree_paths = reader
        .list_paths(&commit)
        .map_err(BackupVerificationError::Reader)?;
    let expected = expected_vault_paths
        .iter()
        .cloned()
        .chain([
            manifest_path.clone(),
            ContentPath::new(BACKUP_GIT_ATTRIBUTES_PATH).expect("the attributes path is valid"),
        ])
        .collect::<Vec<_>>();
    let tree_set = tree_paths
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let expected_set = expected
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if tree_set != expected_set {
        return Err(BackupVerificationError::TreePathMismatch {
            missing: expected_set.difference(&tree_set).cloned().collect(),
            unexpected: tree_set.difference(&expected_set).cloned().collect(),
        });
    }

    let mut required = Vec::new();
    for entry in manifest.entries() {
        let tree_path = super::projection::BackupProjection::tree_path(entry.path());
        let bytes = reader
            .read_blob(&commit, &tree_path)
            .map_err(BackupVerificationError::Reader)?
            .ok_or_else(|| BackupVerificationError::PathMissing(entry.path().clone()))?;
        match entry.storage() {
            BackupRepresentationKind::Git => {
                if Sha256::digest(&bytes) != entry.sha256()
                    || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.size()
                {
                    return Err(BackupVerificationError::ContentMismatch(
                        entry.path().clone(),
                    ));
                }
            }
            BackupRepresentationKind::Lfs => {
                verify_manifest_pointer(entry, &bytes)
                    .map_err(BackupVerificationError::Manifest)?;
                required.push(RequiredLfsObject::new(entry.sha256(), entry.size()));
            }
        }
    }

    // Two paths holding the same bytes are one object; the report counts objects,
    // not paths, because that is what the endpoint stores and restores.
    required.sort();
    required.dedup();
    let plan = lfs
        .prepare_upload(&required)
        .map_err(BackupVerificationError::Lfs)?;
    if !plan.is_complete() {
        return Err(BackupVerificationError::LfsObjectsMissing {
            missing: plan.missing(),
        });
    }

    Ok(BackupVerificationReport {
        commit,
        files_verified: manifest.entries().len(),
        lfs_objects_verified: required.len(),
    })
}

/// Why a backup cannot be verified.
#[derive(Debug)]
pub enum BackupVerificationError<ReaderError, RemoteError, LfsError> {
    Reader(ReaderError),
    Remote(RemoteError),
    Lfs(LfsError),
    NothingToVerify,
    ManifestMissing,
    Manifest(BackupManifestError),
    ManifestPaths(ManifestPathMismatch),
    TreePathMismatch {
        missing: Vec<ContentPath>,
        unexpected: Vec<ContentPath>,
    },
    PathMissing(ContentPath),
    ContentMismatch(ContentPath),
    LfsObjectsMissing {
        missing: Vec<RequiredLfsObject>,
    },
}

impl<R: fmt::Display, M: fmt::Display, L: fmt::Display> fmt::Display
    for BackupVerificationError<R, M, L>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(error) => write!(formatter, "could not read the backup commit: {error}"),
            Self::Remote(error) => write!(formatter, "Git remote failed: {error}"),
            Self::Lfs(error) => write!(formatter, "Git LFS endpoint failed: {error}"),
            Self::NothingToVerify => {
                formatter.write_str("the backup ref holds no commit to verify")
            }
            Self::ManifestMissing => formatter.write_str("backup commit has no manifest"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::ManifestPaths(error) => write!(formatter, "{error}"),
            Self::TreePathMismatch {
                missing,
                unexpected,
            } => write!(
                formatter,
                "backup tree does not match its manifest ({} missing, {} unexpected)",
                missing.len(),
                unexpected.len()
            ),
            Self::PathMissing(path) => write!(formatter, "backup commit is missing {path}"),
            Self::ContentMismatch(path) => {
                write!(
                    formatter,
                    "restored bytes for {path} do not match the manifest"
                )
            }
            Self::LfsObjectsMissing { missing } => write!(
                formatter,
                "{} LFS object(s) are missing from the endpoint",
                missing.len()
            ),
        }
    }
}

impl<R, M, L> Error for BackupVerificationError<R, M, L>
where
    R: Error + 'static,
    M: Error + 'static,
    L: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Reader(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::Lfs(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::ManifestPaths(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::{HashMap, HashSet},
        convert::Infallible,
        time::SystemTime,
    };

    use sha2::{Digest, Sha256 as Sha256Hasher};

    use super::*;
    use crate::{
        backup::{
            delivery::build_backup_delivery,
            lfs::LfsUploadPlan,
            projection::{BackupProjection, TypeFirstBackupRepresentationPolicy},
            run::{BackupRun, BackupRunId},
            tree::{BackupTreeProjection, build_backup_tree},
        },
        domain::{Snapshot, SnapshotFile, SnapshotId, SourceId, TimestampMillis},
        publication::asset::ImmutableBlobSource,
        publication::git::{GitCommitFacts, GitCommitSpec},
    };

    const RUN: u64 = 1;
    const CREATED_AT: u64 = 1_500;

    fn oid(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn tree_id(value: char) -> GitTreeOid {
        GitTreeOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn target() -> GitRefTarget {
        GitRefTarget::new("origin", "refs/heads/mineral-backup").unwrap()
    }

    fn hex40(bytes: &[u8]) -> String {
        let digest = Sha256Hasher::digest(bytes);
        digest[..20]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn fake_tree_oid(tree: &BackupTreeProjection) -> GitTreeOid {
        let mut hasher = Sha256Hasher::new();
        for entry in tree.entries() {
            hasher.update(entry.path().as_str().as_bytes());
            hasher.update(entry.blob_sha256().as_bytes());
        }
        GitTreeOid::new(hex40(&hasher.finalize())).unwrap()
    }

    fn fake_commit_oid(spec: &GitCommitSpec) -> GitCommitOid {
        let mut hasher = Sha256Hasher::new();
        hasher.update(spec.parent().as_str().as_bytes());
        hasher.update(spec.tree().as_str().as_bytes());
        hasher.update(spec.message().as_bytes());
        hasher.update(spec.author_time().as_unix_millis().to_be_bytes());
        GitCommitOid::new(hex40(&hasher.finalize())).unwrap()
    }

    #[derive(Default)]
    struct MemoryBlobs(RefCell<HashMap<Sha256, Vec<u8>>>);

    impl BlobStore for MemoryBlobs {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            let identity = Sha256::digest(content);
            self.0.borrow_mut().insert(identity, content.to_vec());
            Ok(identity)
        }

        fn open(
            &self,
            identity: Sha256,
        ) -> Result<Box<dyn ImmutableBlobSource + '_>, ContentStoreError> {
            Ok(Box::new(ChunkedSource {
                identity,
                bytes: self.read(identity)?,
                position: 0,
            }))
        }
    }

    struct ChunkedSource {
        identity: Sha256,
        bytes: Vec<u8>,
        position: usize,
    }

    impl ImmutableBlobSource for ChunkedSource {
        fn identity(&self) -> Sha256 {
            self.identity
        }

        fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, ContentStoreError> {
            let take = buffer.len().min(self.bytes.len() - self.position);
            buffer[..take].copy_from_slice(&self.bytes[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    #[derive(Default)]
    struct FakeStore(RefCell<HashMap<u64, BackupRun>>);

    impl BackupRunStore for FakeStore {
        type Error = Infallible;

        fn save(&self, run: &BackupRun) -> Result<(), Self::Error> {
            self.0.borrow_mut().insert(run.id().get(), run.clone());
            Ok(())
        }

        fn get(&self, id: BackupRunId) -> Result<Option<BackupRun>, Self::Error> {
            Ok(self.0.borrow().get(&id.get()).cloned())
        }

        fn list(&self) -> Result<Vec<BackupRun>, Self::Error> {
            Ok(self.0.borrow().values().cloned().collect())
        }
    }

    #[derive(Default)]
    struct FakeRepository {
        commits: RefCell<HashMap<String, (GitCommitOid, GitTreeOid)>>,
        materializations: Cell<usize>,
    }

    impl BackupGitRepository for FakeRepository {
        type Error = Infallible;

        fn materialize_backup(
            &self,
            base: &GitCommitOid,
            tree: &BackupTreeProjection,
        ) -> Result<crate::backup::tree::BackupReviewedTree, Self::Error> {
            self.materializations.set(self.materializations.get() + 1);
            Ok(crate::backup::tree::BackupReviewedTree::from_parts(
                base.clone(),
                tree_id('0'),
                fake_tree_oid(tree),
                tree.delivery_sha256(),
                tree.snapshot_id(),
            ))
        }

        fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
            let commit = fake_commit_oid(spec);
            self.commits.borrow_mut().insert(
                commit.as_str().to_owned(),
                (spec.parent().clone(), spec.tree().clone()),
            );
            Ok(commit)
        }

        fn inspect_commit(&self, commit: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
            Ok(match self.commits.borrow().get(commit.as_str()) {
                Some((parent, tree)) => LocalCommitState::Present(GitCommitFacts::from_parts(
                    commit.clone(),
                    parent.clone(),
                    tree.clone(),
                )),
                None => LocalCommitState::Missing,
            })
        }
    }

    struct FakeRemote {
        state: RefCell<RemoteRefState>,
        swaps: Cell<usize>,
        lose_answer: Cell<bool>,
    }

    impl FakeRemote {
        fn at(commit: GitCommitOid) -> Self {
            Self {
                state: RefCell::new(RemoteRefState::Present { commit_oid: commit }),
                swaps: Cell::new(0),
                lose_answer: Cell::new(false),
            }
        }

        fn commit(&self) -> GitCommitOid {
            match &*self.state.borrow() {
                RemoteRefState::Present { commit_oid } => commit_oid.clone(),
                RemoteRefState::Missing => panic!("the fake remote holds no commit"),
            }
        }
    }

    impl GitRemote for FakeRemote {
        type Error = Infallible;

        fn observe_ref(&self, _: &GitRefTarget) -> Result<RemoteRefState, Self::Error> {
            Ok(self.state.borrow().clone())
        }

        fn compare_and_swap(&self, update: &RefUpdate) -> Result<CasOutcome, Self::Error> {
            self.swaps.set(self.swaps.get() + 1);
            let matches = matches!(&*self.state.borrow(), RemoteRefState::Present { commit_oid } if commit_oid == update.expected_old());
            if !matches {
                return Ok(CasOutcome::Rejected);
            }
            *self.state.borrow_mut() = RemoteRefState::Present {
                commit_oid: update.new_commit().clone(),
            };
            if self.lose_answer.get() {
                // The update landed but the answer was lost: the engine must recover
                // by observing the ref again.
                Ok(CasOutcome::Rejected)
            } else {
                Ok(CasOutcome::Updated)
            }
        }
    }

    #[derive(Debug)]
    enum FakeLfsError {
        Refused,
        Content,
        Size,
        Verify,
        Blob(#[allow(dead_code)] ContentStoreError),
    }

    impl fmt::Display for FakeLfsError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{self:?}")
        }
    }

    impl Error for FakeLfsError {}

    #[derive(Default)]
    struct FakeLfs {
        present: RefCell<HashSet<Sha256>>,
        uploaded: RefCell<Vec<RequiredLfsObject>>,
        refuse: RefCell<Option<Sha256>>,
        verify_fails: Cell<bool>,
        offers_verify: Cell<bool>,
    }

    impl LfsRemote for FakeLfs {
        type Error = FakeLfsError;
        type UploadAction = ();
        type VerifyAction = ();

        fn prepare_upload(
            &self,
            objects: &[RequiredLfsObject],
        ) -> Result<LfsUploadPlan<(), ()>, Self::Error> {
            let mut present = Vec::new();
            let mut uploads = Vec::new();
            for object in objects {
                if self.present.borrow().contains(&object.oid()) {
                    present.push(*object);
                } else {
                    let verify = self.offers_verify.get().then_some(());
                    uploads.push(crate::backup::lfs::LfsUpload::new(*object, (), verify));
                }
            }
            Ok(LfsUploadPlan::new(present, uploads))
        }

        fn upload(
            &self,
            object: &RequiredLfsObject,
            source: &mut dyn ImmutableBlobSource,
            _: &(),
        ) -> Result<(), Self::Error> {
            if self.refuse.borrow().as_ref() == Some(&object.oid()) {
                return Err(FakeLfsError::Refused);
            }
            // A real adapter verifies the exact bytes it sent; so does this one.
            let mut hasher = Sha256Hasher::new();
            let mut count = 0_u64;
            let mut buffer = [0_u8; 16];
            loop {
                let read = source.read_chunk(&mut buffer).map_err(FakeLfsError::Blob)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                count += u64::try_from(read).unwrap_or(u64::MAX);
            }
            if Sha256::new(hasher.finalize().into()) != object.oid() {
                return Err(FakeLfsError::Content);
            }
            if count != object.size() {
                return Err(FakeLfsError::Size);
            }
            self.present.borrow_mut().insert(object.oid());
            self.uploaded.borrow_mut().push(*object);
            Ok(())
        }

        fn verify(&self, _: &RequiredLfsObject, _: &()) -> Result<(), Self::Error> {
            if self.verify_fails.get() {
                return Err(FakeLfsError::Verify);
            }
            Ok(())
        }
    }

    struct Fixture {
        store: FakeStore,
        repository: FakeRepository,
        remote: FakeRemote,
        lfs: FakeLfs,
        blobs: MemoryBlobs,
        run: BackupRun,
    }

    impl Fixture {
        /// Builds one frozen intent exactly the way the application does: snapshot ->
        /// projections -> tree -> commit -> intent, with the remote still on `base`.
        fn new(entries: &[(&str, &[u8])]) -> Self {
            Self::with_base(entries, oid('a'))
        }

        fn with_base(entries: &[(&str, &[u8])], base: GitCommitOid) -> Self {
            let blobs = MemoryBlobs::default();
            let files = entries
                .iter()
                .map(|(path, bytes)| {
                    blobs.store(bytes).unwrap();
                    SnapshotFile::new(
                        ContentPath::new(*path).unwrap(),
                        bytes.len() as u64,
                        Sha256::digest(bytes),
                        None,
                    )
                })
                .collect::<Vec<_>>();
            let snapshot = Snapshot::new(
                SnapshotId::new(7).unwrap(),
                SystemTime::UNIX_EPOCH,
                SourceId::new("bedrock").unwrap(),
                files,
            )
            .unwrap();
            let projection = BackupProjection::build(&snapshot).unwrap();
            let delivery =
                build_backup_delivery(&projection, &TypeFirstBackupRepresentationPolicy, &blobs)
                    .unwrap();
            let tree = build_backup_tree(&delivery).unwrap();
            let repository = FakeRepository::default();
            let reviewed = repository.materialize_backup(&base, &tree).unwrap();
            let spec = GitCommitSpec::new(
                base.clone(),
                reviewed.tree_oid().clone(),
                "Mineral",
                "backup@example.invalid",
                TimestampMillis::UNIX_EPOCH,
                "Mineral",
                "backup@example.invalid",
                TimestampMillis::UNIX_EPOCH,
                "Backup content",
            )
            .unwrap();
            let desired = repository.create_commit(&spec).unwrap();
            let run = BackupRun::new(
                BackupRunId::new(RUN).unwrap(),
                snapshot.id(),
                projection.projection_sha256(),
                delivery,
                base.clone(),
                desired,
                target(),
                spec,
                CREATED_AT,
            )
            .unwrap();
            let store = FakeStore::default();
            store.save(&run).unwrap();
            Self {
                store,
                repository,
                remote: FakeRemote::at(base),
                lfs: FakeLfs::default(),
                blobs,
                run,
            }
        }

        fn execute(
            &self,
        ) -> Result<
            BackupExecutionOutcome,
            BackupExecutionError<Infallible, Infallible, Infallible, FakeLfsError>,
        > {
            execute_backup(
                &self.store,
                &self.repository,
                &self.remote,
                &self.lfs,
                &self.blobs,
                BackupRunId::new(RUN).unwrap(),
            )
        }

        fn required(&self) -> Vec<RequiredLfsObject> {
            self.run.required_lfs_objects().to_vec()
        }
    }

    #[test]
    fn a_first_backup_uploads_every_object_and_moves_the_ref_last() {
        let fixture = Fixture::new(&[("notes/a.md", b"# note"), ("img/a.jpg", b"photo bytes")]);

        let outcome = fixture.execute().unwrap();

        assert_eq!(
            outcome,
            BackupExecutionOutcome::BackedUp {
                commit: fixture.run.desired_commit().clone(),
                uploaded: fixture.required(),
                already_present: 0,
            }
        );
        assert_eq!(fixture.remote.commit(), *fixture.run.desired_commit());
        assert_eq!(fixture.remote.swaps.get(), 1);
        assert_eq!(fixture.lfs.present.borrow().len(), 1);
    }

    #[test]
    fn an_object_the_endpoint_already_holds_is_never_uploaded_again() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        for object in fixture.required() {
            fixture.lfs.present.borrow_mut().insert(object.oid());
        }

        let outcome = fixture.execute().unwrap();

        assert_eq!(
            outcome,
            BackupExecutionOutcome::BackedUp {
                commit: fixture.run.desired_commit().clone(),
                uploaded: Vec::new(),
                already_present: 1,
            }
        );
        assert!(fixture.lfs.uploaded.borrow().is_empty());
    }

    #[test]
    fn only_a_changed_object_is_uploaded_on_the_next_backup() {
        let first = Fixture::new(&[
            ("img/a.jpg", b"unchanged bytes"),
            ("img/b.jpg", b"old bytes"),
        ]);
        first.execute().unwrap();
        let second = Fixture::with_base(
            &[
                ("img/a.jpg", b"unchanged bytes"),
                ("img/b.jpg", b"new bytes!"),
            ],
            first.run.desired_commit().clone(),
        );
        second
            .lfs
            .present
            .borrow_mut()
            .extend(first.required().into_iter().map(|object| object.oid()));

        let outcome = second.execute().unwrap();

        let BackupExecutionOutcome::BackedUp { uploaded, .. } = outcome else {
            panic!("the second backup must move the ref");
        };
        let changed = second
            .required()
            .into_iter()
            .find(|object| object.oid() == Sha256::digest(b"new bytes!"))
            .expect("the changed object is required");
        assert_eq!(uploaded, [changed], "only the changed object is new");
        assert_eq!(second.lfs.uploaded.borrow().len(), 1);
    }

    #[test]
    fn an_upload_failure_leaves_the_git_ref_untouched() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        let object = fixture.required()[0];
        *fixture.lfs.refuse.borrow_mut() = Some(object.oid());

        let error = fixture.execute().unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::Lfs(FakeLfsError::Refused)
        ));
        assert_eq!(fixture.remote.swaps.get(), 0, "the ref must not move");
        assert_eq!(fixture.remote.commit(), oid('a'));
    }

    #[test]
    fn a_verify_failure_leaves_the_git_ref_untouched() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        fixture.lfs.offers_verify.set(true);
        fixture.lfs.verify_fails.set(true);

        let error = fixture.execute().unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::Lfs(FakeLfsError::Verify)
        ));
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn a_source_blob_that_is_not_in_cas_leaves_the_git_ref_untouched() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        let object = fixture.required()[0];
        fixture.blobs.0.borrow_mut().remove(&object.oid());

        let error = fixture.execute().unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::SourceBlobMissing { oid } if oid == object.oid()
        ));
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn damaged_cas_bytes_are_never_uploaded_as_a_valid_object() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        let object = fixture.required()[0];
        // Same length, different bytes: only re-hashing during upload can catch it.
        fixture
            .blobs
            .0
            .borrow_mut()
            .insert(object.oid(), vec![b'x'; object.size() as usize]);

        let error = fixture.execute().unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::Lfs(FakeLfsError::Content)
        ));
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn a_commit_naming_another_tree_fails_closed_before_any_upload() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        fixture.repository.commits.borrow_mut().insert(
            fixture.run.desired_commit().as_str().to_owned(),
            (fixture.run.base_commit().clone(), tree_id('9')),
        );

        let error = fixture.execute().unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::CommitFactsMismatch { .. }
        ));
        assert!(fixture.lfs.uploaded.borrow().is_empty());
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn a_missing_commit_is_rebuilt_from_the_frozen_delivery() {
        let fixture = Fixture::new(&[("notes/a.md", b"# note")]);
        fixture.repository.commits.borrow_mut().clear();

        let outcome = fixture.execute().unwrap();

        assert!(matches!(outcome, BackupExecutionOutcome::BackedUp { .. }));
        assert_eq!(
            fixture.repository.materializations.get(),
            2,
            "the fixture materialized once, the resume rebuilt once"
        );
        assert_eq!(fixture.remote.commit(), *fixture.run.desired_commit());
    }

    #[test]
    fn a_remote_that_already_holds_the_frozen_commit_is_never_uploaded_again() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        *fixture.remote.state.borrow_mut() = RemoteRefState::Present {
            commit_oid: fixture.run.desired_commit().clone(),
        };
        for object in fixture.required() {
            fixture.lfs.present.borrow_mut().insert(object.oid());
        }

        let outcome = fixture.execute().unwrap();

        assert_eq!(
            outcome,
            BackupExecutionOutcome::AlreadyBackedUp {
                commit: fixture.run.desired_commit().clone()
            }
        );
        assert!(fixture.lfs.uploaded.borrow().is_empty());
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn an_already_moved_ref_with_missing_lfs_objects_is_refused() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        *fixture.remote.state.borrow_mut() = RemoteRefState::Present {
            commit_oid: fixture.run.desired_commit().clone(),
        };

        let error = fixture.execute().unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::LfsObjectsMissing { ref missing } if missing.len() == 1
        ));
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn a_remote_that_moved_is_a_conflict_without_uploads_or_a_swap() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        *fixture.remote.state.borrow_mut() = RemoteRefState::Present {
            commit_oid: oid('f'),
        };

        let outcome = fixture.execute().unwrap();

        assert_eq!(
            outcome,
            BackupExecutionOutcome::RemoteChanged {
                expected: oid('a'),
                observed: RemoteRefState::Present {
                    commit_oid: oid('f')
                }
            }
        );
        assert!(fixture.lfs.uploaded.borrow().is_empty());
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    #[test]
    fn a_lost_swap_answer_is_recovered_by_observing_the_ref_again() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        fixture.remote.lose_answer.set(true);

        let outcome = fixture.execute().unwrap();

        assert_eq!(
            outcome,
            BackupExecutionOutcome::AlreadyBackedUp {
                commit: fixture.run.desired_commit().clone()
            }
        );
        assert_eq!(fixture.remote.swaps.get(), 1);
    }

    #[test]
    fn the_final_gate_refuses_to_move_a_ref_when_an_object_is_still_missing() {
        /// An endpoint that accepts uploads but never reports the object as present.
        struct ForgetfulLfs(FakeLfs);

        impl LfsRemote for ForgetfulLfs {
            type Error = FakeLfsError;
            type UploadAction = ();
            type VerifyAction = ();

            fn prepare_upload(
                &self,
                objects: &[RequiredLfsObject],
            ) -> Result<LfsUploadPlan<(), ()>, Self::Error> {
                self.0.prepare_upload(objects)
            }

            fn upload(
                &self,
                object: &RequiredLfsObject,
                source: &mut dyn ImmutableBlobSource,
                action: &(),
            ) -> Result<(), Self::Error> {
                self.0.upload(object, source, action)?;
                self.0.present.borrow_mut().remove(&object.oid());
                Ok(())
            }

            fn verify(&self, _: &RequiredLfsObject, _: &()) -> Result<(), Self::Error> {
                Ok(())
            }
        }

        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        let lfs = ForgetfulLfs(FakeLfs::default());

        let error = execute_backup(
            &fixture.store,
            &fixture.repository,
            &fixture.remote,
            &lfs,
            &fixture.blobs,
            BackupRunId::new(RUN).unwrap(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BackupExecutionError::LfsObjectsMissing { ref missing } if missing.len() == 1
        ));
        assert_eq!(fixture.remote.swaps.get(), 0);
    }

    /// A reader built from one frozen tree and content store, for verification tests.
    struct FakeTreeReader {
        paths: Vec<ContentPath>,
        blobs: HashMap<ContentPath, Vec<u8>>,
    }

    impl BackupTreeReader for FakeTreeReader {
        type Error = Infallible;

        fn list_paths(&self, _: &GitCommitOid) -> Result<Vec<ContentPath>, Self::Error> {
            Ok(self.paths.clone())
        }

        fn read_blob(
            &self,
            _: &GitCommitOid,
            path: &ContentPath,
        ) -> Result<Option<Vec<u8>>, Self::Error> {
            Ok(self.blobs.get(path).cloned())
        }
    }

    fn reader_for(fixture: &Fixture) -> FakeTreeReader {
        let tree = build_backup_tree(fixture.run.delivery()).unwrap();
        let mut blobs = HashMap::new();
        for entry in tree.entries() {
            blobs.insert(
                entry.path().clone(),
                fixture.blobs.read(entry.blob_sha256()).unwrap(),
            );
        }
        FakeTreeReader {
            paths: tree.entries().map(|entry| entry.path().clone()).collect(),
            blobs,
        }
    }

    #[test]
    fn a_verification_reports_every_file_and_lfs_object() {
        let fixture = Fixture::new(&[("notes/a.md", b"# note"), ("img/a.jpg", b"photo bytes")]);
        for object in fixture.required() {
            fixture.lfs.present.borrow_mut().insert(object.oid());
        }
        *fixture.remote.state.borrow_mut() = RemoteRefState::Present {
            commit_oid: fixture.run.desired_commit().clone(),
        };
        let reader = reader_for(&fixture);

        let report =
            verify_backup(&reader, &fixture.remote, &fixture.lfs, &target(), None).unwrap();

        assert_eq!(report.commit(), fixture.run.desired_commit());
        assert_eq!(report.files_verified(), 2);
        assert_eq!(report.lfs_objects_verified(), 1);
    }

    #[test]
    fn a_tampered_git_blob_fails_verification() {
        let fixture = Fixture::new(&[("notes/a.md", b"# note")]);
        let mut reader = reader_for(&fixture);
        reader.blobs.insert(
            ContentPath::new("vault/notes/a.md").unwrap(),
            b"# NOTE".to_vec(),
        );

        let error =
            verify_backup(&reader, &fixture.remote, &fixture.lfs, &target(), None).unwrap_err();

        assert!(matches!(
            error,
            BackupVerificationError::ContentMismatch(ref path) if path.as_str() == "notes/a.md"
        ));
    }

    #[test]
    fn a_missing_lfs_object_fails_verification() {
        let fixture = Fixture::new(&[("img/a.jpg", b"photo bytes")]);
        *fixture.remote.state.borrow_mut() = RemoteRefState::Present {
            commit_oid: fixture.run.desired_commit().clone(),
        };
        let reader = reader_for(&fixture);

        let error =
            verify_backup(&reader, &fixture.remote, &fixture.lfs, &target(), None).unwrap_err();

        assert!(matches!(
            error,
            BackupVerificationError::LfsObjectsMissing { ref missing } if missing.len() == 1
        ));
    }

    #[test]
    fn a_backup_ref_that_holds_no_commit_has_nothing_to_verify() {
        let fixture = Fixture::new(&[("notes/a.md", b"# note")]);
        *fixture.remote.state.borrow_mut() = RemoteRefState::Missing;
        let reader = reader_for(&fixture);

        let error =
            verify_backup(&reader, &fixture.remote, &fixture.lfs, &target(), None).unwrap_err();

        assert!(matches!(error, BackupVerificationError::NothingToVerify));
    }
}
