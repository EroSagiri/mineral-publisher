use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use crate::{
    ports::BlobStore,
    publication::git::{
        GitCommitOid, GitCommitSpec, GitCurrentTarget, GitRepository, LocalCommitState,
        ReviewedGitTree,
    },
    publish::{RepositoryLocator, RepositoryLocatorError},
    workflow::{ManagedRoot, TextProjection},
};

use super::{
    GitCommitObjectCreator, GitCommitObjectError, GitCurrentTargetAdapter, GitCurrentTargetError,
    GitProjectionMaterializationError, GitProjectionMaterializer,
};

/// Native Git repository location.
///
/// A publication intent stores only the portable [`RepositoryLocator`] string.
/// This type is the native side of that seam: it canonicalizes a configured
/// path and resolves a persisted locator back into a local path for Git
/// commands, so the engine never has to interpret filesystem paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRepositoryIdentity {
    path: PathBuf,
    locator: RepositoryLocator,
}

impl GitRepositoryIdentity {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, GitRepositoryIdentityError> {
        let path = fs::canonicalize(path).map_err(GitRepositoryIdentityError::Canonicalize)?;
        Self::from_canonical_path(path)
    }

    /// Resolve a persisted locator back into a local Git repository path.
    pub fn from_locator(locator: &RepositoryLocator) -> Result<Self, GitRepositoryIdentityError> {
        Self::from_canonical_path(PathBuf::from(locator.as_str()))
    }

    pub fn from_canonical_path(path: PathBuf) -> Result<Self, GitRepositoryIdentityError> {
        if !path.is_absolute() || path.as_os_str().is_empty() {
            return Err(GitRepositoryIdentityError::NotAbsolute);
        }
        // The locator is persisted as text: a path that cannot be represented
        // without loss must fail here rather than silently recover a different
        // directory later.
        let locator = path
            .to_str()
            .ok_or(GitRepositoryIdentityError::NotUnicode)?;
        let locator =
            RepositoryLocator::new(locator).map_err(GitRepositoryIdentityError::Locator)?;
        Ok(Self { path, locator })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn locator(&self) -> &RepositoryLocator {
        &self.locator
    }
}

#[derive(Debug)]
pub enum GitRepositoryIdentityError {
    Canonicalize(std::io::Error),
    NotAbsolute,
    NotUnicode,
    Locator(RepositoryLocatorError),
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
            Self::NotUnicode => {
                formatter.write_str("Git repository path must be valid Unicode to be persisted")
            }
            Self::Locator(error) => write!(formatter, "Git repository locator is invalid: {error}"),
        }
    }
}

impl Error for GitRepositoryIdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Canonicalize(error) => Some(error),
            Self::Locator(error) => Some(error),
            Self::NotAbsolute | Self::NotUnicode => None,
        }
    }
}

/// The `git` command-line runtime for [`GitRepository`].
///
/// It owns the local repository *and* the blob source it materializes from, so
/// the engine never hands repository details or large media through the port.
#[derive(Clone, Debug)]
pub struct GitRepositoryAdapter<B: BlobStore> {
    repository: PathBuf,
    blobs: B,
}

impl<B: BlobStore> GitRepositoryAdapter<B> {
    pub fn new(repository: impl AsRef<Path>, blobs: B) -> Result<Self, GitRepositoryAdapterError> {
        let identity = GitRepositoryIdentity::new(repository)
            .map_err(GitRepositoryAdapterError::Repository)?;
        Ok(Self {
            repository: identity.path().to_owned(),
            blobs,
        })
    }

    /// Resolves the locator a publication intent persisted.
    pub fn from_locator(
        locator: &RepositoryLocator,
        blobs: B,
    ) -> Result<Self, GitRepositoryAdapterError> {
        let identity = GitRepositoryIdentity::from_locator(locator)
            .map_err(GitRepositoryAdapterError::Repository)?;
        Ok(Self {
            repository: identity.path().to_owned(),
            blobs,
        })
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }
}

impl<B: BlobStore> GitRepository for GitRepositoryAdapter<B> {
    type Error = GitRepositoryAdapterError;

    fn read_current(
        &self,
        base: &GitCommitOid,
        root: &ManagedRoot,
    ) -> Result<GitCurrentTarget, Self::Error> {
        GitCurrentTargetAdapter::read(&self.repository, base.as_str(), root.clone())
            .map_err(GitRepositoryAdapterError::CurrentTarget)
    }

    fn materialize(
        &self,
        base: &GitCommitOid,
        text: &TextProjection,
    ) -> Result<ReviewedGitTree, Self::Error> {
        GitProjectionMaterializer::materialize(&self.repository, base.as_str(), text, &self.blobs)
            .map_err(GitRepositoryAdapterError::Materialization)
    }

    fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
        GitCommitObjectCreator::create_from_spec(&self.repository, spec)
            .map_err(GitRepositoryAdapterError::Commit)
    }

    fn inspect_commit(&self, commit: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
        GitCommitObjectCreator::inspect(&self.repository, commit)
            .map_err(GitRepositoryAdapterError::Inspection)
    }
}

#[derive(Debug)]
pub enum GitRepositoryAdapterError {
    Repository(GitRepositoryIdentityError),
    CurrentTarget(GitCurrentTargetError),
    Materialization(GitProjectionMaterializationError),
    Commit(GitCommitObjectError),
    Inspection(GitCommitObjectError),
}

impl fmt::Display for GitRepositoryAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository(error) => write!(formatter, "could not identify repository: {error}"),
            Self::CurrentTarget(error) => {
                write!(formatter, "could not read current Git target: {error}")
            }
            Self::Materialization(error) => {
                write!(
                    formatter,
                    "could not materialize reviewed Git tree: {error}"
                )
            }
            Self::Commit(error) => write!(formatter, "could not create Git commit object: {error}"),
            Self::Inspection(error) => {
                write!(
                    formatter,
                    "could not inspect local Git commit object: {error}"
                )
            }
        }
    }
}

impl Error for GitRepositoryAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::CurrentTarget(error) => Some(error),
            Self::Materialization(error) => Some(error),
            Self::Commit(error) => Some(error),
            Self::Inspection(error) => Some(error),
        }
    }
}
