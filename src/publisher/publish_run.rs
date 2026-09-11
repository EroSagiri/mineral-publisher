use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    domain::{Sha256, SnapshotId},
    workflow::ManagedRoot,
};

use super::GitCommitResult;

/// Stable identity for one explicit publication attempt.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublishRunId(u64);

impl PublishRunId {
    pub fn new(value: u64) -> Result<Self, PublishRunIdError> {
        if value == 0 {
            return Err(PublishRunIdError::Zero);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishRunIdError {
    Zero,
}

impl fmt::Display for PublishRunIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("publish run ID must be positive")
    }
}

impl Error for PublishRunIdError {}

/// Canonical local Git repository location used to recover a publication intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRepositoryIdentity(PathBuf);

impl GitRepositoryIdentity {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, GitRepositoryIdentityError> {
        let path = fs::canonicalize(path).map_err(GitRepositoryIdentityError::Canonicalize)?;
        Self::from_canonical_path(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn from_canonical_path(path: PathBuf) -> Result<Self, GitRepositoryIdentityError> {
        if !path.is_absolute() || path.as_os_str().is_empty() {
            return Err(GitRepositoryIdentityError::NotAbsolute);
        }
        Ok(Self(path))
    }
}

#[derive(Debug)]
pub enum GitRepositoryIdentityError {
    Canonicalize(std::io::Error),
    NotAbsolute,
}

impl fmt::Display for GitRepositoryIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canonicalize(_) => {
                formatter.write_str("could not canonicalize Git repository path")
            }
            Self::NotAbsolute => {
                formatter.write_str("Git repository identity must be an absolute path")
            }
        }
    }
}

impl Error for GitRepositoryIdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Canonicalize(error) => Some(error),
            Self::NotAbsolute => None,
        }
    }
}

/// Explicit remote and fully-qualified destination ref. This contains no remote URL or credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationTarget {
    remote_name: String,
    destination_ref: String,
}

impl PublicationTarget {
    pub fn new(
        remote_name: impl Into<String>,
        destination_ref: impl Into<String>,
    ) -> Result<Self, PublicationTargetError> {
        let remote_name = remote_name.into();
        let destination_ref = destination_ref.into();
        if remote_name.trim().is_empty() || remote_name.contains(['\0', '\n', '\r']) {
            return Err(PublicationTargetError::InvalidRemoteName);
        }
        if !is_safe_destination_ref(&destination_ref) {
            return Err(PublicationTargetError::InvalidDestinationRef);
        }
        Ok(Self {
            remote_name,
            destination_ref,
        })
    }

    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    pub fn destination_ref(&self) -> &str {
        &self.destination_ref
    }
}

fn is_safe_destination_ref(value: &str) -> bool {
    value.starts_with("refs/")
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains(['\0', '\\', ' ', '~', '^', ':', '?', '*', '['])
        && value
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.'))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationTargetError {
    InvalidRemoteName,
    InvalidDestinationRef,
}

impl fmt::Display for PublicationTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRemoteName => formatter.write_str("publication remote name is invalid"),
            Self::InvalidDestinationRef => {
                formatter.write_str("publication destination ref is invalid")
            }
        }
    }
}

impl Error for PublicationTargetError {}

/// The immutable publication intent. A commit-ready intent is not evidence of remote success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRun {
    id: PublishRunId,
    snapshot_id: SnapshotId,
    projection_sha256: Sha256,
    managed_root: ManagedRoot,
    repository: GitRepositoryIdentity,
    target: PublicationTarget,
    base_commit: String,
    reviewed_tree: String,
    publication: PublishRunPublication,
    created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishRunPublication {
    Noop,
    CommitReady { commit_oid: String },
}

impl PublishRun {
    pub fn from_git_commit_result(
        id: PublishRunId,
        repository: GitRepositoryIdentity,
        target: PublicationTarget,
        result: &GitCommitResult,
        created_at: SystemTime,
    ) -> Result<Self, PublishRunError> {
        let created_at_unix_ms = created_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PublishRunError::CreatedAtBeforeUnixEpoch)?
            .as_millis()
            .try_into()
            .map_err(|_| PublishRunError::CreatedAtOutOfRange)?;
        match result {
            GitCommitResult::Noop(noop) => Self::from_parts(
                id,
                noop.snapshot_id(),
                noop.projection_sha256(),
                noop.managed_root().clone(),
                repository,
                target,
                noop.base_commit().to_owned(),
                noop.tree_oid().to_owned(),
                PublishRunPublication::Noop,
                created_at_unix_ms,
            ),
            GitCommitResult::Created(commit) => Self::from_parts(
                id,
                commit.snapshot_id(),
                commit.projection_sha256(),
                commit.managed_root().clone(),
                repository,
                target,
                commit.base_commit().to_owned(),
                commit.tree_oid().to_owned(),
                PublishRunPublication::CommitReady {
                    commit_oid: commit.commit_oid().to_owned(),
                },
                created_at_unix_ms,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rehydrate(
        id: PublishRunId,
        snapshot_id: SnapshotId,
        projection_sha256: Sha256,
        managed_root: ManagedRoot,
        repository: GitRepositoryIdentity,
        target: PublicationTarget,
        base_commit: String,
        reviewed_tree: String,
        publication: PublishRunPublication,
        created_at_unix_ms: u64,
    ) -> Result<Self, PublishRunError> {
        Self::from_parts(
            id,
            snapshot_id,
            projection_sha256,
            managed_root,
            repository,
            target,
            base_commit,
            reviewed_tree,
            publication,
            created_at_unix_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        id: PublishRunId,
        snapshot_id: SnapshotId,
        projection_sha256: Sha256,
        managed_root: ManagedRoot,
        repository: GitRepositoryIdentity,
        target: PublicationTarget,
        base_commit: String,
        reviewed_tree: String,
        publication: PublishRunPublication,
        created_at_unix_ms: u64,
    ) -> Result<Self, PublishRunError> {
        if !is_oid(&base_commit) {
            return Err(PublishRunError::InvalidBaseCommit);
        }
        if !is_oid(&reviewed_tree) {
            return Err(PublishRunError::InvalidReviewedTree);
        }
        if let PublishRunPublication::CommitReady { commit_oid } = &publication
            && !is_oid(commit_oid)
        {
            return Err(PublishRunError::InvalidCommit);
        }
        Ok(Self {
            id,
            snapshot_id,
            projection_sha256,
            managed_root,
            repository,
            target,
            base_commit,
            reviewed_tree,
            publication,
            created_at_unix_ms,
        })
    }

    pub fn id(&self) -> PublishRunId {
        self.id
    }
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }
    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }
    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }
    pub fn repository(&self) -> &GitRepositoryIdentity {
        &self.repository
    }
    pub fn target(&self) -> &PublicationTarget {
        &self.target
    }
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    pub fn reviewed_tree(&self) -> &str {
        &self.reviewed_tree
    }
    pub fn publication(&self) -> &PublishRunPublication {
        &self.publication
    }
    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }
}

fn is_oid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Eq, PartialEq)]
pub enum PublishRunError {
    CreatedAtBeforeUnixEpoch,
    CreatedAtOutOfRange,
    InvalidBaseCommit,
    InvalidReviewedTree,
    InvalidCommit,
}

impl fmt::Display for PublishRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreatedAtBeforeUnixEpoch => {
                formatter.write_str("publish run timestamp is before Unix epoch")
            }
            Self::CreatedAtOutOfRange => {
                formatter.write_str("publish run timestamp is outside supported range")
            }
            Self::InvalidBaseCommit => formatter.write_str("publish run base commit ID is invalid"),
            Self::InvalidReviewedTree => {
                formatter.write_str("publish run reviewed tree ID is invalid")
            }
            Self::InvalidCommit => formatter.write_str("publish run commit ID is invalid"),
        }
    }
}

impl Error for PublishRunError {}

pub trait PublishRunStore {
    type Error: Error;

    fn save(&self, run: &PublishRun) -> Result<(), Self::Error>;
    fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error>;
    fn list(&self) -> Result<Vec<PublishRun>, Self::Error>;
    fn list_for_target(&self, target: &PublicationTarget) -> Result<Vec<PublishRun>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_requires_a_fully_qualified_safe_ref() {
        assert!(PublicationTarget::new("origin", "refs/heads/main").is_ok());
        for value in [
            "main",
            "refs/heads/../main",
            "refs/heads/a b",
            "refs/heads/",
        ] {
            assert!(
                PublicationTarget::new("origin", value).is_err(),
                "accepted {value:?}"
            );
        }
    }
}
