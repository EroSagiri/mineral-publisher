use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    domain::ContentPath,
    ports::BlobStore,
    publication::git::{
        CasOutcome, GitCommitOid, GitCommitSpec, GitRefTarget, GitTreeOid, LocalCommitState,
        RefUpdate, RemoteRefState,
    },
};

use mineral_core::backup::{
    BackupGitRepository, BackupReviewedTree, BackupTreeProjection, BackupTreeReader,
};

use super::super::publisher::{GitCommitMetadata, GitRemoteAdapter, GitRemoteError};
use super::super::publisher::{GitCommitObjectCreator, GitCommitObjectError};

static NEXT_INDEX: AtomicU64 = AtomicU64::new(1);

/// The `git` command-line runtime for the backup pipeline.
///
/// It owns the local repository *and* the content store the tree is materialized
/// from, so the engine never hands repository details or large media through a port.
/// Binary paths are materialized as their pointer blobs and text paths as their
/// original blobs; nothing here reads a source file to decide anything.
#[derive(Clone, Debug)]
pub struct GitBackupRepository<B: BlobStore> {
    repository: PathBuf,
    blobs: B,
}

impl<B: BlobStore> GitBackupRepository<B> {
    pub fn new(repository: impl AsRef<Path>, blobs: B) -> Result<Self, GitBackupError> {
        let repository = fs::canonicalize(repository).map_err(GitBackupError::Repository)?;
        if !repository.is_dir() {
            return Err(GitBackupError::RepositoryUnavailable);
        }
        Ok(Self { repository, blobs })
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }

    /// The observable state of the backup ref, through the shared Git remote adapter.
    pub fn observe(&self, target: &GitRefTarget) -> Result<RemoteRefState, GitRemoteError> {
        let remote = GitRemoteAdapter::new(&self.repository)?;
        mineral_core::publication::git::GitRemote::observe_ref(&remote, target)
    }
}

impl<B: BlobStore> BackupGitRepository for GitBackupRepository<B> {
    type Error = GitBackupError;

    fn materialize_backup(
        &self,
        base: &GitCommitOid,
        tree: &BackupTreeProjection,
    ) -> Result<BackupReviewedTree, Self::Error> {
        let base_commit = self.resolve_commit(base.as_str())?;
        let base_tree_oid = self.resolve_tree(&base_commit)?;

        // Every blob is read before the object database is touched, so a backup that
        // cannot read one of its own blobs leaves no partial tree behind.
        let blobs = tree
            .entries()
            .map(|entry| {
                self.blobs
                    .read(entry.blob_sha256())
                    .map(|bytes| (entry, bytes))
                    .map_err(|source| GitBackupError::ContentStore {
                        path: entry.path().clone(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let index = TemporaryIndex::create()?;
        let operation = self.write_tree(&index.path, &blobs);
        match (operation, index.cleanup()) {
            (Ok(tree_oid), Ok(())) => Ok(BackupReviewedTree::from_parts(
                base.clone(),
                base_tree_oid,
                tree_oid,
                tree.delivery_sha256(),
                tree.snapshot_id(),
            )),
            (Ok(_), Err(source)) => Err(GitBackupError::Cleanup { source }),
            (Err(operation), Ok(())) => Err(operation),
            (Err(operation), Err(source)) => Err(GitBackupError::OperationAndCleanup {
                operation: Box::new(operation),
                cleanup: source,
            }),
        }
    }

    fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
        GitCommitObjectCreator::create_from_spec(&self.repository, spec)
            .map_err(GitBackupError::Commit)
    }

    fn inspect_commit(&self, commit: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
        GitCommitObjectCreator::inspect(&self.repository, commit).map_err(GitBackupError::Commit)
    }
}

impl<B: BlobStore> GitBackupRepository<B> {
    fn write_tree(
        &self,
        index_path: &Path,
        blobs: &[(&mineral_core::backup::BackupTreeEntry, Vec<u8>)],
    ) -> Result<GitTreeOid, GitBackupError> {
        self.git_with_index(index_path, &["read-tree", "--empty"])?;

        let mut index_info = String::new();
        for (entry, bytes) in blobs {
            // The tree must hold exactly the bytes the delivery froze, so the blob is
            // hashed from those bytes and checked against the identity the tree names.
            // Git addresses its own objects (SHA-1 by default), so the check is on the
            // content identity: the bytes about to be stored are the frozen bytes.
            let content = crate::domain::Sha256::digest(bytes);
            if content != entry.blob_sha256() {
                return Err(GitBackupError::BlobMismatch {
                    path: entry.path().clone(),
                    expected: entry.blob_sha256().to_string(),
                    actual: content.to_string(),
                });
            }
            let written = self.hash_blob(bytes)?;
            index_info.push_str(&format!("100644 {written}\t{}\n", entry.path().as_str()));
        }
        self.git_with_index_stdin(index_path, &["update-index", "--index-info"], &index_info)?;
        let output = self.git_with_index_stdout(index_path, &["write-tree"])?;
        GitTreeOid::new(output).map_err(|_| GitBackupError::MalformedObjectId)
    }

    fn hash_blob(&self, bytes: &[u8]) -> Result<String, GitBackupError> {
        let mut child = Command::new("git")
            .current_dir(&self.repository)
            // `--no-filters` keeps Git attributes from rewriting bytes on the way in:
            // a backup stores raw bytes, never a normalized form.
            .args(["hash-object", "-w", "--stdin", "--no-filters"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        {
            use std::io::Write;
            let stdin = child.stdin.as_mut().ok_or(GitBackupError::GitUnavailable {
                stderr: "no stdin".to_owned(),
            })?;
            stdin
                .write_all(bytes)
                .map_err(|source| GitBackupError::Io { source })?;
        }
        let output = child
            .wait_with_output()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitBackupError::GitFailed {
                operation: "hash-object",
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn resolve_commit(&self, revision: &str) -> Result<String, GitBackupError> {
        let output = self.git(&["rev-parse", "--verify", &format!("{revision}^{{commit}}")])?;
        Ok(String::from_utf8_lossy(&output).trim().to_owned())
    }

    fn resolve_tree(&self, commit: &str) -> Result<GitTreeOid, GitBackupError> {
        let output = self.git(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?;
        GitTreeOid::new(String::from_utf8_lossy(&output).trim().to_owned())
            .map_err(|_| GitBackupError::MalformedObjectId)
    }

    fn git(&self, arguments: &[&str]) -> Result<Vec<u8>, GitBackupError> {
        let output = Command::new("git")
            .current_dir(&self.repository)
            .args(arguments)
            .output()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitBackupError::GitFailed {
                operation: "git",
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }

    fn git_with_index(&self, index_path: &Path, arguments: &[&str]) -> Result<(), GitBackupError> {
        self.git_with_index_stdout(index_path, arguments)
            .map(|_| ())
    }

    fn git_with_index_stdin(
        &self,
        index_path: &Path,
        arguments: &[&str],
        input: &str,
    ) -> Result<(), GitBackupError> {
        let mut child = Command::new("git")
            .current_dir(&self.repository)
            .env("GIT_INDEX_FILE", index_path)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        {
            use std::io::Write;
            let stdin = child.stdin.as_mut().ok_or(GitBackupError::GitUnavailable {
                stderr: "no stdin".to_owned(),
            })?;
            stdin
                .write_all(input.as_bytes())
                .map_err(|source| GitBackupError::Io { source })?;
        }
        let output = child
            .wait_with_output()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitBackupError::GitFailed {
                operation: "git (index)",
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(())
    }

    fn git_with_index_stdout(
        &self,
        index_path: &Path,
        arguments: &[&str],
    ) -> Result<String, GitBackupError> {
        let output = Command::new("git")
            .current_dir(&self.repository)
            .env("GIT_INDEX_FILE", index_path)
            .args(arguments)
            .output()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitBackupError::GitFailed {
                operation: "git (index)",
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

impl<B: BlobStore> BackupTreeReader for GitBackupRepository<B> {
    type Error = GitBackupError;

    fn list_paths(&self, commit: &GitCommitOid) -> Result<Vec<ContentPath>, Self::Error> {
        let output = self.git(&["ls-tree", "-r", "--name-only", "-z", commit.as_str()])?;
        output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                ContentPath::new(String::from_utf8_lossy(entry).into_owned())
                    .map_err(|_| GitBackupError::MalformedTreePath)
            })
            .collect()
    }

    fn read_blob(
        &self,
        commit: &GitCommitOid,
        path: &ContentPath,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let expression = format!("{}:{}", commit.as_str(), path.as_str());
        let output = Command::new("git")
            .current_dir(&self.repository)
            .args(["cat-file", "blob", &expression])
            .output()
            .map_err(|source| GitBackupError::GitUnavailable {
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(output.stdout))
    }
}

/// One temporary index, removed however the materialization ends.
struct TemporaryIndex {
    path: PathBuf,
}

impl TemporaryIndex {
    fn create() -> Result<Self, GitBackupError> {
        let sequence = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mineral-backup-index-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        Ok(Self { path })
    }

    fn cleanup(self) -> Result<(), io::Error> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(source),
        }
    }
}

/// Why one backup tree could not be materialized.
#[derive(Debug)]
pub enum GitBackupError {
    Repository(io::Error),
    RepositoryUnavailable,
    GitUnavailable {
        stderr: String,
    },
    GitFailed {
        operation: &'static str,
        stderr: String,
    },
    Io {
        source: io::Error,
    },
    ContentStore {
        path: ContentPath,
        source: crate::storage::ContentStoreError,
    },
    /// Git hashed the bytes to something other than the identity the tree names.
    BlobMismatch {
        path: ContentPath,
        expected: String,
        actual: String,
    },
    MalformedObjectId,
    MalformedTreePath,
    Commit(GitCommitObjectError),
    Cleanup {
        source: io::Error,
    },
    OperationAndCleanup {
        operation: Box<GitBackupError>,
        cleanup: io::Error,
    },
}

impl fmt::Display for GitBackupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository(error) => {
                write!(formatter, "could not open the backup repository: {error}")
            }
            Self::RepositoryUnavailable => {
                formatter.write_str("the backup repository is not a directory")
            }
            Self::GitUnavailable { stderr } => write!(formatter, "git is unavailable: {stderr}"),
            Self::GitFailed { operation, stderr } => {
                write!(formatter, "{operation} failed: {stderr}")
            }
            Self::Io { source } => write!(formatter, "could not write to git: {source}"),
            Self::ContentStore { path, source } => {
                write!(formatter, "could not read backup blob for {path}: {source}")
            }
            Self::BlobMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "git stored {path} as {actual}, not the frozen {expected}"
            ),
            Self::MalformedObjectId => formatter.write_str("git returned a malformed object id"),
            Self::MalformedTreePath => formatter.write_str("the backup tree holds an invalid path"),
            Self::Commit(error) => {
                write!(formatter, "could not create or inspect a commit: {error}")
            }
            Self::Cleanup { source } => {
                write!(formatter, "could not remove the temporary index: {source}")
            }
            Self::OperationAndCleanup { operation, cleanup } => write!(
                formatter,
                "backup materialization failed ({operation}) and the temporary index could not be removed ({cleanup})"
            ),
        }
    }
}

impl Error for GitBackupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::Io { source } => Some(source),
            Self::ContentStore { source, .. } => Some(source),
            Self::Commit(error) => Some(error),
            Self::Cleanup { source } => Some(source),
            Self::OperationAndCleanup { operation, .. } => Some(operation),
            _ => None,
        }
    }
}

/// Reports the observed state of one backup ref through the shared Git remote.
pub fn observe_backup_ref(
    repository: impl AsRef<Path>,
    target: &GitRefTarget,
) -> Result<RemoteRefState, GitBackupError> {
    let remote = GitRemoteAdapter::new(repository).map_err(|error| match error {
        GitRemoteError::RepositoryUnavailable => GitBackupError::RepositoryUnavailable,
        other => GitBackupError::GitFailed {
            operation: "remote",
            stderr: other.to_string(),
        },
    })?;
    mineral_core::publication::git::GitRemote::observe_ref(&remote, target).map_err(|error| {
        GitBackupError::GitFailed {
            operation: "ls-remote",
            stderr: error.to_string(),
        }
    })
}

/// One compare-and-swap of the backup ref, through the shared Git remote adapter.
pub fn compare_and_swap_backup_ref(
    repository: impl AsRef<Path>,
    update: &RefUpdate,
) -> Result<CasOutcome, GitBackupError> {
    let remote =
        GitRemoteAdapter::new(repository).map_err(|_| GitBackupError::RepositoryUnavailable)?;
    mineral_core::publication::git::GitRemote::compare_and_swap(&remote, update).map_err(|error| {
        GitBackupError::GitFailed {
            operation: "push",
            stderr: error.to_string(),
        }
    })
}

/// The commit metadata one backup uses, frozen into the intent before the commit.
pub fn backup_commit_metadata(
    author_name: &str,
    author_email: &str,
    message: &str,
) -> Result<GitCommitMetadata, GitBackupError> {
    GitCommitMetadata::new(author_name, author_email, message).map_err(|error| {
        GitBackupError::GitFailed {
            operation: "commit metadata",
            stderr: error.to_string(),
        }
    })
}
