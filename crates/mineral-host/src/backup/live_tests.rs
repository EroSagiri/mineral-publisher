//! Opt-in integration against a real private Git repository with Git LFS.
//!
//! `cargo test --workspace` never runs these: they are `#[ignore]`d and need a real
//! repository, real credentials and a `git lfs` executable. They prove the one thing
//! a fake endpoint cannot: that the pointers this engine writes are pointers a
//! standard `git clone && git lfs pull` restores byte-for-byte.
//!
//! Run them with:
//!
//! ```text
//! MINERAL_BACKUP_LIVE_REMOTE=git@github.com:owner/private-backup.git \
//! MINERAL_BACKUP_LIVE_BRANCH=refs/heads/mineral-backup \
//! MINERAL_BACKUP_LIVE_BATCH_URL=https://github.com/owner/private-backup.git/info/lfs \
//! MINERAL_BACKUP_LIVE_USER=… MINERAL_BACKUP_LIVE_TOKEN=… \
//!   cargo test -p mineral-publisher backup::live_tests -- --ignored --test-threads=1
//! ```
//!
//! The test never touches the repository's existing content: it backs up onto the
//! configured branch and reads it back from a fresh clone.

use std::{fs, path::PathBuf, process::Command, time::SystemTime};

use mineral_core::{
    backup::{
        BackupExecutionOutcome, BackupManifest, BackupRepresentationKind, BackupRunId,
        TypeFirstBackupRepresentationPolicy, parse_lfs_pointer, verify_backup,
    },
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId, SourceId, TimestampMillis},
    publication::git::GitRefTarget,
};

use crate::{
    backup::{
        application::{BackupApplicationRequest, SequentialBackupRunIdGenerator, run_backup},
        git_backup::GitBackupRepository,
        lfs_http::{LfsHttpConfig, LfsHttpRemote, LfsToken},
    },
    publisher::{GitCommitMetadata, GitRemoteAdapter},
    storage::{LocalContentStore, SqliteBackupRunStore},
};

struct Live {
    remote: String,
    branch: String,
    batch_url: String,
    username: String,
    token: String,
    workspace: PathBuf,
}

fn live() -> Live {
    let remote = std::env::var("MINERAL_BACKUP_LIVE_REMOTE")
        .expect("MINERAL_BACKUP_LIVE_REMOTE must name the private repository");
    let branch = std::env::var("MINERAL_BACKUP_LIVE_BRANCH")
        .unwrap_or_else(|_| "refs/heads/mineral-backup".to_owned());
    let batch_url = std::env::var("MINERAL_BACKUP_LIVE_BATCH_URL")
        .expect("MINERAL_BACKUP_LIVE_BATCH_URL must name the LFS endpoint");
    let username =
        std::env::var("MINERAL_BACKUP_LIVE_USER").expect("MINERAL_BACKUP_LIVE_USER must be set");
    let token =
        std::env::var("MINERAL_BACKUP_LIVE_TOKEN").expect("MINERAL_BACKUP_LIVE_TOKEN must be set");
    let workspace = std::env::temp_dir().join(format!(
        "mineral-backup-live-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&workspace).unwrap();
    Live {
        remote,
        branch,
        batch_url,
        username,
        token,
        workspace,
    }
}

fn git(directory: &std::path::Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("git {arguments:?} could not run: {error}"));
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn clone_into(live: &Live, name: &str) -> PathBuf {
    let path = live.workspace.join(name);
    let output = Command::new("git")
        .current_dir(&live.workspace)
        .args(["clone", &live.remote, path.to_str().unwrap()])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git clone must run");
    assert!(
        output.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

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
        SnapshotId::new(21).unwrap(),
        SystemTime::UNIX_EPOCH,
        SourceId::new("live-vault").unwrap(),
        files,
    )
    .unwrap()
}

#[test]
#[ignore = "requires a real private repository, credentials and git lfs"]
fn a_real_private_repository_restores_every_byte_after_git_lfs_pull() {
    let live = live();
    let repository = clone_into(&live, "backup");
    let target = GitRefTarget::new("origin", &live.branch).unwrap();

    // The branch must already exist: the pipeline refuses to invent a base commit.
    let remote = GitRemoteAdapter::new(&repository).unwrap();
    if !matches!(
        mineral_core::publication::git::GitRemote::observe_ref(&remote, &target).unwrap(),
        mineral_core::publication::git::RemoteRefState::Present { .. }
    ) {
        panic!(
            "create {} on {} first (git push origin HEAD:{})",
            live.branch, live.remote, live.branch
        );
    }

    let cas = LocalContentStore::new(live.workspace.join("cas"));
    let store = SqliteBackupRunStore::open(live.workspace.join("backup-runs.sqlite3")).unwrap();
    let git_backup = GitBackupRepository::new(&repository, cas.clone()).unwrap();
    let config = LfsHttpConfig::new(
        live.batch_url.clone(),
        live.username.clone(),
        LfsToken::new(live.token.clone()).unwrap(),
        std::time::Duration::from_secs(300),
    )
    .unwrap();
    let lfs = LfsHttpRemote::new(config).unwrap();
    let metadata =
        GitCommitMetadata::new("Mineral", "backup@example.invalid", "Backup content").unwrap();
    let run_ids = SequentialBackupRunIdGenerator::new(BackupRunId::new(1).unwrap());
    // A binary payload large enough to cross the chunk boundary.
    let photo = (0..(70 * 1024))
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect::<Vec<_>>();
    let text = b"# live note\n";
    let snapshot = snapshot(&cas, &[("live/index.md", text), ("live/photo.bin", &photo)]);

    let outcome = run_backup(
        BackupApplicationRequest {
            progress: &crate::runtime::NoProgress,
            snapshot: &snapshot,
            target: &target,
            commit_metadata: &metadata,
            policy: &TypeFirstBackupRepresentationPolicy,
            created_at: TimestampMillis::from_system_time(SystemTime::now()).unwrap(),
        },
        &store,
        &git_backup,
        &remote,
        &lfs,
        &cas,
        &run_ids,
    )
    .unwrap();

    let commit = match outcome.execution() {
        BackupExecutionOutcome::BackedUp { commit, .. } => commit.clone(),
        other => panic!("the live backup did not publish a commit: {other:?}"),
    };
    assert!(
        !lfs.describe().contains(&live.token),
        "the endpoint description must never contain the credential"
    );

    // A fresh clone plus `git lfs pull` is the whole restore path.
    let restored = clone_into(&live, "restore");
    git(&restored, &["checkout", commit.as_str()]);
    git(&restored, &["lfs", "pull"]);

    let manifest =
        BackupManifest::parse(&fs::read(restored.join(".mineral-backup/manifest")).unwrap())
            .unwrap();
    assert_eq!(manifest.entries().len(), snapshot.files().len());
    for entry in manifest.entries() {
        let source = snapshot
            .files()
            .iter()
            .find(|file| file.path() == entry.path())
            .unwrap();
        let path = restored.join("vault").join(entry.path().as_str());
        let bytes = fs::read(&path).unwrap_or_else(|_| panic!("{path:?} was not restored"));
        assert_eq!(bytes.len() as u64, source.size());
        assert_eq!(Sha256::digest(&bytes), source.sha256(), "{}", entry.path());
        if entry.storage() == BackupRepresentationKind::Lfs {
            assert!(
                bytes.len() as u64 > 64 * 1024,
                "the payload crossed a chunk"
            );
        }
    }

    // The pointer really is an LFS pointer in the repository, not the payload: the
    // tree blob is read straight out of the commit, so `git lfs pull` cannot hide it.
    let tree_pointer = git(
        &restored,
        &[
            "cat-file",
            "blob",
            &format!("{}:vault/live/photo.bin", commit.as_str()),
        ],
    );
    assert_eq!(
        parse_lfs_pointer(tree_pointer.as_bytes()).unwrap().oid(),
        Sha256::digest(&photo)
    );

    // And the engine's own verifier agrees with the restore.
    let report = verify_backup(&git_backup, &remote, &lfs, &target, Some(&commit)).unwrap();
    assert_eq!(report.files_verified(), snapshot.files().len());
    assert_eq!(report.lfs_objects_verified(), 1);

    let _ = fs::remove_dir_all(&live.workspace);
}
