//! Offline integration: one Snapshot becomes a restorable backup in a real Git
//! repository, with only the LFS endpoint faked.
//!
//! The test drives the real content store, the real Git object database and the real
//! Git remote compare-and-swap against a local bare "origin". Only the LFS transport
//! is a fake, because it stands in for a remote HTTP service that the offline suite
//! must never contact. The restore step is a plain `git clone` plus a pointer lookup,
//! which is exactly what a user would do.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use mineral_core::{
    backup::{
        BackupExecutionOutcome, BackupManifest, BackupRepresentationKind, BackupTreeReader,
        LfsRemote, LfsUploadPlan, RequiredLfsObject, TypeFirstBackupRepresentationPolicy,
        parse_lfs_pointer, verify_backup,
    },
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId, SourceId, TimestampMillis},
    publication::asset::ImmutableBlobSource,
    publication::git::{GitRefTarget, RemoteRefState},
};

use crate::{
    backup::{
        application::{BackupApplicationRequest, SequentialBackupRunIdGenerator, run_backup},
        git_backup::GitBackupRepository,
    },
    publisher::{GitCommitMetadata, GitRemoteAdapter},
    storage::{LocalContentStore, SqliteBackupRunStore},
};

const SOURCE_ID: &str = "bedrock";

#[derive(Debug)]
struct FakeLfsError(String);

impl std::fmt::Display for FakeLfsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FakeLfsError {}

/// The LFS endpoint, in memory. It verifies the bytes it receives exactly like a
/// real endpoint would, so a damaged CAS blob cannot pass through it.
#[derive(Default)]
struct FakeLfs {
    objects: RefCell<HashMap<Sha256, Vec<u8>>>,
    uploads: Cell<usize>,
    refuse: RefCell<Option<Sha256>>,
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
            if self.objects.borrow().contains_key(&object.oid()) {
                present.push(*object);
            } else {
                uploads.push(mineral_core::backup::LfsUpload::new(*object, (), None));
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
            return Err(FakeLfsError("refused".to_owned()));
        }
        let mut hasher = {
            use sha2::Digest;
            sha2::Sha256::new()
        };
        let mut bytes = Vec::new();
        let mut buffer = vec![0_u8; 4 * 1024];
        loop {
            let read = source
                .read_chunk(&mut buffer)
                .map_err(|error| FakeLfsError(error.to_string()))?;
            if read == 0 {
                break;
            }
            {
                use sha2::Digest;
                hasher.update(&buffer[..read]);
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let actual = {
            use sha2::Digest;
            Sha256::new(hasher.finalize().into())
        };
        if actual != object.oid() || bytes.len() as u64 != object.size() {
            return Err(FakeLfsError("content mismatch".to_owned()));
        }
        self.objects.borrow_mut().insert(object.oid(), bytes);
        self.uploads.set(self.uploads.get() + 1);
        Ok(())
    }

    fn verify(&self, _: &RequiredLfsObject, _: &()) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct TestWorkspace {
    root: PathBuf,
}

impl TestWorkspace {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "mineral-backup-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn cas(&self) -> LocalContentStore {
        LocalContentStore::new(self.root.join("cas"))
    }

    fn store(&self) -> SqliteBackupRunStore {
        SqliteBackupRunStore::open(self.root.join("backup-runs.sqlite3")).unwrap()
    }

    fn git(&self, directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A bare origin plus a clone with one initial commit on `main`.
    fn repository(&self) -> PathBuf {
        let origin = self.path("origin.git");
        fs::create_dir_all(&origin).unwrap();
        self.git(&origin, &["init", "--bare", "--initial-branch=main"]);
        let work = self.path("backup");
        fs::create_dir_all(&work).unwrap();
        self.git(&work, &["init", "--initial-branch=main"]);
        fs::write(work.join("README.md"), b"# Mineral backup\n").unwrap();
        self.git(&work, &["add", "README.md"]);
        self.git(&work, &["commit", "-m", "init"]);
        self.git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        self.git(&work, &["push", "-u", "origin", "main"]);
        work
    }

    fn target() -> GitRefTarget {
        GitRefTarget::new("origin", "refs/heads/main").unwrap()
    }

    fn remote(&self, repository: &Path) -> GitRemoteAdapter {
        GitRemoteAdapter::new(repository).unwrap()
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Builds one Snapshot and stores every file's bytes in the content store, exactly
/// as a source stage would before the Snapshot is frozen.
fn snapshot(cas: &LocalContentStore, entries: &[(&str, &[u8])]) -> Snapshot {
    let files = entries
        .iter()
        .map(|(path, bytes)| {
            crate::ports::BlobStore::store(cas, bytes).unwrap();
            SnapshotFile::new(
                ContentPath::new(*path).unwrap(),
                bytes.len() as u64,
                Sha256::digest(bytes),
                None,
            )
        })
        .collect::<Vec<_>>();
    Snapshot::new(
        SnapshotId::new(11).unwrap(),
        SystemTime::UNIX_EPOCH,
        SourceId::new(SOURCE_ID).unwrap(),
        files,
    )
    .unwrap()
}

fn metadata() -> GitCommitMetadata {
    GitCommitMetadata::new("Mineral", "backup@example.invalid", "Backup content").unwrap()
}

struct Harness {
    workspace: TestWorkspace,
    repository: PathBuf,
}

impl Harness {
    fn new(name: &str) -> Self {
        let workspace = TestWorkspace::new(name);
        let repository = workspace.repository();
        Self {
            workspace,
            repository,
        }
    }

    fn run(
        &self,
        snapshot: &Snapshot,
        lfs: &FakeLfs,
        run_ids: &SequentialBackupRunIdGenerator,
    ) -> Result<
        crate::backup::application::BackupApplicationOutcome,
        crate::backup::application::BackupApplicationError<
            crate::storage::SqliteBackupRunStoreError,
            crate::backup::git_backup::GitBackupError,
            crate::publisher::GitRemoteError,
            FakeLfsError,
        >,
    > {
        let cas = self.workspace.cas();
        let store = self.workspace.store();
        let repository = GitBackupRepository::new(&self.repository, cas.clone()).unwrap();
        let remote = self.workspace.remote(&self.repository);
        let target = TestWorkspace::target();
        let metadata = metadata();
        run_backup(
            BackupApplicationRequest {
                snapshot,
                target: &target,
                commit_metadata: &metadata,
                policy: &TypeFirstBackupRepresentationPolicy,
                created_at: TimestampMillis::from_unix_millis(1_500),
            },
            &store,
            &repository,
            &remote,
            lfs,
            &cas,
            run_ids,
        )
    }
}

#[test]
fn a_snapshot_becomes_a_backup_that_restores_byte_for_byte() {
    let harness = Harness::new("restore");
    let lfs = FakeLfs::default();
    let run_ids =
        SequentialBackupRunIdGenerator::new(mineral_core::backup::BackupRunId::new(1).unwrap());
    // `private/` is never filtered, two paths share bytes (one LFS object), and one
    // file is empty.
    let snapshot = snapshot(
        &harness.workspace.cas(),
        &[
            ("notes/a.md", b"# note\n"),
            ("private/secret.md", b"password: hunter2\n"),
            ("attachments/a.jpg", b"binary payload"),
            ("attachments/copy.jpg", b"binary payload"),
            ("empty.md", b""),
        ],
    );

    let outcome = harness.run(&snapshot, &lfs, &run_ids).unwrap();

    assert!(matches!(
        outcome.execution(),
        BackupExecutionOutcome::BackedUp { .. }
    ));
    assert_eq!(outcome.files(), 5);
    assert_eq!(
        outcome.lfs_objects(),
        1,
        "two paths with identical bytes require one LFS object"
    );
    assert_eq!(lfs.uploads.get(), 1);

    // The remote ref holds exactly the frozen commit.
    let remote = harness.workspace.remote(&harness.repository);
    let observed =
        mineral_core::publication::git::GitRemote::observe_ref(&remote, &TestWorkspace::target())
            .unwrap();
    let RemoteRefState::Present { commit_oid } = observed else {
        panic!("the backup ref must exist after a successful backup");
    };
    assert_eq!(
        &commit_oid,
        match outcome.execution() {
            BackupExecutionOutcome::BackedUp { commit, .. } => commit,
            other => panic!("{other:?}"),
        }
    );

    // A plain clone is enough to read the text side and every pointer.
    let clone = harness.workspace.path("restore");
    harness.workspace.git(
        &harness.workspace.root.clone(),
        &[
            "clone",
            harness.workspace.path("origin.git").to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );

    let manifest =
        BackupManifest::parse(&fs::read(clone.join(".mineral-backup/manifest")).unwrap()).unwrap();
    assert_eq!(manifest.source_id().as_str(), SOURCE_ID);
    assert_eq!(manifest.entries().len(), snapshot.files().len());

    let restored = snapshot
        .files()
        .iter()
        .map(|file| (file.path().as_str().to_owned(), file))
        .collect::<HashMap<_, _>>();
    let restored_paths = manifest
        .entries()
        .iter()
        .map(|entry| entry.path().as_str().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        restored_paths,
        restored.keys().cloned().collect(),
        "the manifest and the Snapshot describe the same path set"
    );
    for entry in manifest.entries() {
        let file = restored[entry.path().as_str()];
        let path = clone.join("vault").join(entry.path().as_str());
        let bytes = fs::read(&path).unwrap_or_else(|_| panic!("{path:?} is missing"));
        match entry.storage() {
            BackupRepresentationKind::Git => {
                assert_eq!(Sha256::digest(&bytes), file.sha256(), "{}", entry.path());
                assert_eq!(bytes.len() as u64, file.size());
            }
            BackupRepresentationKind::Lfs => {
                let pointer = parse_lfs_pointer(&bytes).unwrap();
                // Invariants 1 and 2 survive the whole round trip.
                assert_eq!(pointer.oid(), file.sha256(), "{}", entry.path());
                assert_eq!(pointer.size(), file.size());
                let payload = lfs.objects.borrow().get(&pointer.oid()).cloned().unwrap();
                assert_eq!(Sha256::digest(&payload), file.sha256());
                assert_eq!(payload.len() as u64, file.size());
            }
        }
    }

    // `backup verify` agrees with the restore.
    let repository =
        GitBackupRepository::new(&harness.repository, harness.workspace.cas()).unwrap();
    let report = verify_backup(
        &repository,
        &remote,
        &lfs,
        &TestWorkspace::target(),
        Some(&commit_oid),
    )
    .unwrap();
    assert_eq!(report.commit(), &commit_oid);
    assert_eq!(report.files_verified(), snapshot.files().len());
    assert_eq!(report.lfs_objects_verified(), 1);
}

#[test]
fn an_unchanged_second_backup_uploads_nothing_and_only_a_changed_object_is_sent() {
    let harness = Harness::new("incremental");
    let lfs = FakeLfs::default();
    let run_ids =
        SequentialBackupRunIdGenerator::new(mineral_core::backup::BackupRunId::new(1).unwrap());

    let first = snapshot(
        &harness.workspace.cas(),
        &[
            ("attachments/a.jpg", b"first payload"),
            ("attachments/b.jpg", b"stable payload"),
        ],
    );
    harness.run(&first, &lfs, &run_ids).unwrap();
    assert_eq!(lfs.uploads.get(), 2);

    // Same content again: nothing is uploaded, and the ref still moves forward.
    harness.run(&first, &lfs, &run_ids).unwrap();
    assert_eq!(lfs.uploads.get(), 2, "no object is uploaded twice");

    // One changed binary: exactly one new object.
    let second = snapshot(
        &harness.workspace.cas(),
        &[
            ("attachments/a.jpg", b"second payload"),
            ("attachments/b.jpg", b"stable payload"),
        ],
    );
    harness.run(&second, &lfs, &run_ids).unwrap();
    assert_eq!(lfs.uploads.get(), 3);
    assert!(
        lfs.objects
            .borrow()
            .contains_key(&Sha256::digest(b"second payload"))
    );
}

#[test]
fn a_refused_lfs_upload_leaves_the_backup_ref_where_it_was() {
    let harness = Harness::new("ordering");
    let lfs = FakeLfs::default();
    let run_ids =
        SequentialBackupRunIdGenerator::new(mineral_core::backup::BackupRunId::new(1).unwrap());
    let snapshot = snapshot(
        &harness.workspace.cas(),
        &[("attachments/a.jpg", b"payload")],
    );
    *lfs.refuse.borrow_mut() = Some(Sha256::digest(b"payload"));

    let error = harness.run(&snapshot, &lfs, &run_ids).unwrap_err();

    assert!(error.to_string().contains("Git LFS"), "{error}");
    let remote = harness.workspace.remote(&harness.repository);
    let observed =
        mineral_core::publication::git::GitRemote::observe_ref(&remote, &TestWorkspace::target())
            .unwrap();
    let RemoteRefState::Present { commit_oid } = observed else {
        panic!("the ref must still exist");
    };
    let head = harness
        .workspace
        .git(&harness.repository, &["rev-parse", "HEAD"]);
    assert_eq!(
        commit_oid.as_str(),
        head,
        "the backup ref must not move when an LFS object is missing"
    );
}

#[test]
fn a_ref_that_does_not_exist_yet_fails_closed_instead_of_inventing_a_base() {
    let workspace = TestWorkspace::new("no-base");
    workspace.repository();
    let lfs = FakeLfs::default();
    let cas = workspace.cas();
    let store = workspace.store();
    let repository = GitBackupRepository::new(workspace.path("backup"), cas.clone()).unwrap();
    let remote = workspace.remote(&workspace.path("backup"));
    let target = GitRefTarget::new("origin", "refs/heads/mineral-backup").unwrap();
    let snapshot = snapshot(&cas, &[("notes/a.md", b"# note")]);
    let metadata = metadata();
    let run_ids =
        SequentialBackupRunIdGenerator::new(mineral_core::backup::BackupRunId::new(1).unwrap());

    let error = run_backup(
        BackupApplicationRequest {
            snapshot: &snapshot,
            target: &target,
            commit_metadata: &metadata,
            policy: &TypeFirstBackupRepresentationPolicy,
            created_at: TimestampMillis::from_unix_millis(1_500),
        },
        &store,
        &repository,
        &remote,
        &lfs,
        &cas,
        &run_ids,
    )
    .unwrap_err();

    assert!(error.to_string().contains("does not exist yet"), "{error}");
    assert_eq!(lfs.uploads.get(), 0);
}

#[test]
fn the_tree_holds_pointer_bytes_for_binary_paths_and_original_bytes_for_text() {
    let harness = Harness::new("tree");
    let lfs = FakeLfs::default();
    let run_ids =
        SequentialBackupRunIdGenerator::new(mineral_core::backup::BackupRunId::new(1).unwrap());
    let snapshot = snapshot(
        &harness.workspace.cas(),
        &[("notes/a.md", b"# note\n"), ("img/a.jpg", b"payload")],
    );

    let outcome = harness.run(&snapshot, &lfs, &run_ids).unwrap();

    // The adapter pushes the commit without moving the local branch, so the frozen
    // commit identity is taken from the outcome, not from `refs/heads/main`.
    let commit = match outcome.execution() {
        BackupExecutionOutcome::BackedUp { commit, .. } => commit.clone(),
        other => panic!("{other:?}"),
    };
    let repository =
        GitBackupRepository::new(&harness.repository, harness.workspace.cas()).unwrap();
    let note = BackupTreeReader::read_blob(
        &repository,
        &commit,
        &ContentPath::new("vault/notes/a.md").unwrap(),
    )
    .unwrap()
    .unwrap();
    let pointer = BackupTreeReader::read_blob(
        &repository,
        &commit,
        &ContentPath::new("vault/img/a.jpg").unwrap(),
    )
    .unwrap()
    .unwrap();
    let paths = BackupTreeReader::list_paths(&repository, &commit).unwrap();

    assert_eq!(note, b"# note\n");
    assert_eq!(
        parse_lfs_pointer(&pointer).unwrap().oid(),
        Sha256::digest(b"payload")
    );
    assert!(paths.contains(&ContentPath::new(".gitattributes").unwrap()));
    assert!(paths.contains(&ContentPath::new(".mineral-backup/manifest").unwrap()));
    assert_eq!(
        paths.len(),
        snapshot.files().len() + 2,
        "the tree holds the vault, the attributes and the manifest"
    );
}
