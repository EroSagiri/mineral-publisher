use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::{
    publication::git::{
        CasOutcome, GitCommitOid, GitRefTarget, GitRemote, RefUpdate, RemoteRefState,
    },
    publish::RepositoryLocator,
};

use super::GitRepositoryIdentity;

/// The `git` command-line runtime for [`GitRemote`].
///
/// This is the only host module that touches a real remote, and it only reports
/// facts or performs the one side effect it is asked for. Every other publisher
/// module works from the values this adapter returns.
#[derive(Clone, Debug)]
pub struct GitRemoteAdapter {
    repository: PathBuf,
}

impl GitRemoteAdapter {
    pub fn new(repository: impl AsRef<Path>) -> Result<Self, GitRemoteError> {
        let repository = repository.as_ref();
        match fs::metadata(repository) {
            Ok(metadata) if metadata.is_dir() => Ok(Self {
                repository: repository.to_path_buf(),
            }),
            _ => Err(GitRemoteError::RepositoryUnavailable),
        }
    }

    /// Resolves the locator a publication intent persisted.
    pub fn from_locator(locator: &RepositoryLocator) -> Result<Self, GitRemoteError> {
        let identity = GitRepositoryIdentity::from_locator(locator)
            .map_err(|_| GitRemoteError::RepositoryUnavailable)?;
        Self::new(identity.path())
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }

    fn run(&self, arguments: &[&str]) -> Option<Output> {
        Command::new("git")
            .current_dir(&self.repository)
            .args(arguments)
            .output()
            .ok()
    }

    fn verify_repository(&self) -> Result<(), GitRemoteError> {
        match self.run(&["rev-parse", "--git-dir"]) {
            Some(output) if output.status.success() => Ok(()),
            Some(_) => Err(GitRemoteError::RepositoryUnavailable),
            None => Err(GitRemoteError::GitUnavailable),
        }
    }
}

impl GitRemote for GitRemoteAdapter {
    type Error = GitRemoteError;

    fn observe_ref(&self, target: &GitRefTarget) -> Result<RemoteRefState, GitRemoteError> {
        self.verify_repository()?;

        let remote_check = self
            .run(&["remote", "get-url", "--", target.remote_name()])
            .ok_or(GitRemoteError::GitUnavailable)?;
        if !remote_check.status.success() {
            return Err(GitRemoteError::RemoteNotConfigured);
        }

        let output = self
            .run(&[
                "ls-remote",
                "--refs",
                "--",
                target.remote_name(),
                target.destination_ref(),
            ])
            .ok_or(GitRemoteError::GitUnavailable)?;
        if !output.status.success() {
            return Err(
                if looks_like_authentication_or_network_failure(&output.stderr) {
                    GitRemoteError::AuthenticationOrNetworkFailure {
                        status: output.status.code(),
                    }
                } else {
                    GitRemoteError::RemoteQueryFailed {
                        status: output.status.code(),
                    }
                },
            );
        }

        parse_ls_remote(&output.stdout, target.destination_ref())
    }

    fn compare_and_swap(&self, update: &RefUpdate) -> Result<CasOutcome, GitRemoteError> {
        self.verify_repository()?;

        let target = update.target();
        let lease = format!(
            "--force-with-lease={}:{}",
            target.destination_ref(),
            update.expected_old().as_str()
        );
        let refspec = format!(
            "{}:{}",
            update.new_commit().as_str(),
            target.destination_ref()
        );
        let output = self
            .run(&[
                "push",
                lease.as_str(),
                "--",
                target.remote_name(),
                refspec.as_str(),
            ])
            .ok_or(GitRemoteError::GitUnavailable)?;
        if output.status.success() {
            return Ok(CasOutcome::Updated);
        }
        if looks_like_authentication_or_network_failure(&output.stderr) {
            return Err(GitRemoteError::AuthenticationOrNetworkFailure {
                status: output.status.code(),
            });
        }
        // The command ran and the remote refused the update. What the remote
        // actually holds is established by the observation the engine takes next.
        Ok(CasOutcome::Rejected)
    }
}

fn parse_ls_remote(output: &[u8], expected_ref: &str) -> Result<RemoteRefState, GitRemoteError> {
    let output = std::str::from_utf8(output).map_err(|_| GitRemoteError::InvalidRemoteOutput)?;
    let mut observed_oid: Option<GitCommitOid> = None;
    for line in output.lines() {
        let (oid, reference) = line
            .split_once('\t')
            .ok_or(GitRemoteError::InvalidRemoteOutput)?;
        if reference != expected_ref {
            return Err(GitRemoteError::InvalidRemoteOutput);
        }
        let oid = GitCommitOid::new(oid).map_err(|_| GitRemoteError::InvalidRemoteOutput)?;
        if observed_oid.as_ref().is_some_and(|current| current != &oid) {
            return Err(GitRemoteError::MultipleInconsistentResults);
        }
        observed_oid = Some(oid);
    }
    Ok(match observed_oid {
        Some(commit_oid) => RemoteRefState::Present { commit_oid },
        None => RemoteRefState::Missing,
    })
}

fn looks_like_authentication_or_network_failure(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "authentication failed",
        "permission denied",
        "could not resolve host",
        "could not read from remote repository",
        "connection timed out",
        "connection refused",
        "network is unreachable",
        "unable to access",
    ]
    .iter()
    .any(|pattern| stderr.contains(pattern))
}

/// Every way a runtime can fail to observe or update the remote.
///
/// Process exit codes live here, in the adapter's diagnostics, and never cross
/// into the engine's values: the engine only ever sees observed facts,
/// [`CasOutcome`], or this error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitRemoteError {
    RepositoryUnavailable,
    GitUnavailable,
    RemoteNotConfigured,
    AuthenticationOrNetworkFailure { status: Option<i32> },
    RemoteQueryFailed { status: Option<i32> },
    CommitQueryFailed { status: Option<i32> },
    InvalidRemoteOutput,
    InvalidCommitOutput,
    MultipleInconsistentResults,
}

impl fmt::Display for GitRemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryUnavailable => {
                formatter.write_str("Git repository is unavailable or invalid")
            }
            Self::GitUnavailable => formatter.write_str("Git command could not be executed"),
            Self::RemoteNotConfigured => {
                formatter.write_str("publication remote is not configured")
            }
            Self::AuthenticationOrNetworkFailure { status } => write!(
                formatter,
                "remote authentication or network query failed with status {status:?}"
            ),
            Self::RemoteQueryFailed { status } => {
                write!(formatter, "remote query failed with status {status:?}")
            }
            Self::CommitQueryFailed { status } => {
                write!(
                    formatter,
                    "local commit query failed with status {status:?}"
                )
            }
            Self::InvalidRemoteOutput => {
                formatter.write_str("remote query returned invalid output")
            }
            Self::InvalidCommitOutput => {
                formatter.write_str("local commit query returned invalid output")
            }
            Self::MultipleInconsistentResults => {
                formatter.write_str("remote query returned inconsistent results for one ref")
            }
        }
    }
}

impl Error for GitRemoteError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_empty_output_is_missing() {
        assert_eq!(
            parse_ls_remote(b"", "refs/heads/main").unwrap(),
            RemoteRefState::Missing
        );
    }

    #[test]
    fn inconsistent_multiple_results_fail_closed() {
        let output = format!(
            "{}\trefs/heads/main\n{}\trefs/heads/main\n",
            "a".repeat(40),
            "b".repeat(40)
        );

        assert_eq!(
            parse_ls_remote(output.as_bytes(), "refs/heads/main"),
            Err(GitRemoteError::MultipleInconsistentResults)
        );
    }

    #[test]
    fn unexpected_refs_and_malformed_oids_fail_closed() {
        assert_eq!(
            parse_ls_remote(b"deadbeef\trefs/heads/other\n", "refs/heads/main"),
            Err(GitRemoteError::InvalidRemoteOutput)
        );
        assert_eq!(
            parse_ls_remote(b"not-an-oid\trefs/heads/main\n", "refs/heads/main"),
            Err(GitRemoteError::InvalidRemoteOutput)
        );
        assert_eq!(
            parse_ls_remote(b"no-separator\n", "refs/heads/main"),
            Err(GitRemoteError::InvalidRemoteOutput)
        );
    }

    #[test]
    fn authentication_and_network_failures_are_recognised() {
        for message in [
            "fatal: Authentication failed for 'https://example.invalid/repo'",
            "Permission denied (publickey).",
            "fatal: could not resolve host: example.invalid",
            "fatal: unable to access 'https://example.invalid/repo'",
        ] {
            assert!(
                looks_like_authentication_or_network_failure(message.as_bytes()),
                "did not recognise {message:?}"
            );
        }
        assert!(!looks_like_authentication_or_network_failure(
            b"! [rejected]        main -> main (stale info)"
        ));
    }
}
