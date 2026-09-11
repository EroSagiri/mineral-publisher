use std::{
    error::Error,
    fmt, fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{PublicationTarget, PublishRun, PublishRunId};

/// Stable identity for one immutable observation of a publication target.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RemoteObservationId(u64);

impl RemoteObservationId {
    pub fn new(value: u64) -> Result<Self, RemoteObservationIdError> {
        if value == 0 {
            return Err(RemoteObservationIdError::Zero);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteObservationIdError {
    Zero,
}

impl fmt::Display for RemoteObservationIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote observation ID must be positive")
    }
}

impl Error for RemoteObservationIdError {}

/// A Git commit object identity returned by a remote ref query.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GitCommitOid(String);

impl GitCommitOid {
    pub fn new(value: impl Into<String>) -> Result<Self, GitCommitOidError> {
        let value = value.into();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitCommitOidError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitCommitOidError {
    Invalid,
}

impl fmt::Display for GitCommitOidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Git commit object ID is invalid")
    }
}

impl Error for GitCommitOidError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteRefState {
    Present { commit_oid: GitCommitOid },
    Missing,
}

/// An immutable, time-stamped remote fact scoped to one publication intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRefObservation {
    id: RemoteObservationId,
    publish_run_id: PublishRunId,
    target: PublicationTarget,
    observed: RemoteRefState,
    observed_at_unix_ms: u64,
}

impl RemoteRefObservation {
    pub fn new(
        id: RemoteObservationId,
        publish_run: &PublishRun,
        observed: RemoteRefState,
        observed_at: SystemTime,
    ) -> Result<Self, RemoteObservationError> {
        let observed_at_unix_ms = observed_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RemoteObservationError::ObservedAtBeforeUnixEpoch)?
            .as_millis()
            .try_into()
            .map_err(|_| RemoteObservationError::ObservedAtOutOfRange)?;
        Ok(Self::from_parts(
            id,
            publish_run.id(),
            publish_run.target().clone(),
            observed,
            observed_at_unix_ms,
        ))
    }

    pub(crate) fn rehydrate(
        id: RemoteObservationId,
        publish_run_id: PublishRunId,
        target: PublicationTarget,
        observed: RemoteRefState,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self::from_parts(id, publish_run_id, target, observed, observed_at_unix_ms)
    }

    fn from_parts(
        id: RemoteObservationId,
        publish_run_id: PublishRunId,
        target: PublicationTarget,
        observed: RemoteRefState,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self {
            id,
            publish_run_id,
            target,
            observed,
            observed_at_unix_ms,
        }
    }

    pub fn id(&self) -> RemoteObservationId {
        self.id
    }
    pub fn publish_run_id(&self) -> PublishRunId {
        self.publish_run_id
    }
    pub fn target(&self) -> &PublicationTarget {
        &self.target
    }
    pub fn observed(&self) -> &RemoteRefState {
        &self.observed
    }
    pub fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteObservationError {
    ObservedAtBeforeUnixEpoch,
    ObservedAtOutOfRange,
}

impl fmt::Display for RemoteObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservedAtBeforeUnixEpoch => {
                formatter.write_str("remote observation timestamp is before Unix epoch")
            }
            Self::ObservedAtOutOfRange => {
                formatter.write_str("remote observation timestamp is outside supported range")
            }
        }
    }
}

impl Error for RemoteObservationError {}

pub trait RemoteObservationStore {
    type Error: Error;

    fn save(&self, observation: &RemoteRefObservation) -> Result<(), Self::Error>;
    fn get(&self, id: RemoteObservationId) -> Result<Option<RemoteRefObservation>, Self::Error>;
    fn list_for_publish_run(
        &self,
        publish_run_id: PublishRunId,
    ) -> Result<Vec<RemoteRefObservation>, Self::Error>;
}

/// Observes the configured remote directly without fetching or consulting local refs.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitRemoteObserver;

impl GitRemoteObserver {
    pub fn observe(
        id: RemoteObservationId,
        publish_run: &PublishRun,
        observed_at: SystemTime,
    ) -> Result<RemoteRefObservation, GitRemoteObservationError> {
        let repository = publish_run.repository().path();
        match fs::metadata(repository) {
            Ok(metadata) if metadata.is_dir() => {}
            _ => return Err(GitRemoteObservationError::RepositoryUnavailable),
        }

        let repository_check = Command::new("git")
            .current_dir(repository)
            .args(["rev-parse", "--git-dir"])
            .output()
            .map_err(|_| GitRemoteObservationError::RepositoryUnavailable)?;
        if !repository_check.status.success() {
            return Err(GitRemoteObservationError::RepositoryUnavailable);
        }

        let target = publish_run.target();
        let remote_check = Command::new("git")
            .current_dir(repository)
            .args(["remote", "get-url", "--", target.remote_name()])
            .output()
            .map_err(|_| GitRemoteObservationError::GitUnavailable)?;
        if !remote_check.status.success() {
            return Err(GitRemoteObservationError::RemoteNotConfigured);
        }

        let output = Command::new("git")
            .current_dir(repository)
            .args([
                "ls-remote",
                "--refs",
                "--",
                target.remote_name(),
                target.destination_ref(),
            ])
            .output()
            .map_err(|_| GitRemoteObservationError::GitUnavailable)?;
        if !output.status.success() {
            let error = if looks_like_authentication_or_network_failure(&output.stderr) {
                GitRemoteObservationError::AuthenticationOrNetworkFailure {
                    status: output.status.code(),
                }
            } else {
                GitRemoteObservationError::RemoteQueryFailed {
                    status: output.status.code(),
                }
            };
            return Err(error);
        }

        let observed = parse_ls_remote(&output.stdout, target.destination_ref())?;
        RemoteRefObservation::new(id, publish_run, observed, observed_at)
            .map_err(GitRemoteObservationError::Observation)
    }
}

fn parse_ls_remote(
    output: &[u8],
    expected_ref: &str,
) -> Result<RemoteRefState, GitRemoteObservationError> {
    let output =
        std::str::from_utf8(output).map_err(|_| GitRemoteObservationError::InvalidRemoteOutput)?;
    let mut observed_oid: Option<GitCommitOid> = None;
    for line in output.lines() {
        let (oid, reference) = line
            .split_once('\t')
            .ok_or(GitRemoteObservationError::InvalidRemoteOutput)?;
        if reference != expected_ref {
            return Err(GitRemoteObservationError::InvalidRemoteOutput);
        }
        let oid =
            GitCommitOid::new(oid).map_err(|_| GitRemoteObservationError::InvalidRemoteOutput)?;
        if observed_oid.as_ref().is_some_and(|current| current != &oid) {
            return Err(GitRemoteObservationError::MultipleInconsistentResults);
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitRemoteObservationError {
    RepositoryUnavailable,
    GitUnavailable,
    RemoteNotConfigured,
    AuthenticationOrNetworkFailure { status: Option<i32> },
    RemoteQueryFailed { status: Option<i32> },
    InvalidRemoteOutput,
    MultipleInconsistentResults,
    Observation(RemoteObservationError),
}

impl fmt::Display for GitRemoteObservationError {
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
            Self::InvalidRemoteOutput => {
                formatter.write_str("remote query returned invalid output")
            }
            Self::MultipleInconsistentResults => {
                formatter.write_str("remote query returned inconsistent results for one ref")
            }
            Self::Observation(error) => error.fmt(formatter),
        }
    }
}

impl Error for GitRemoteObservationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Observation(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Sha256, SnapshotId},
        publisher::{GitRepositoryIdentity, PublishRunPublication},
        workflow::ManagedRoot,
    };
    use std::{
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestRepository {
        root: PathBuf,
        local: PathBuf,
    }

    impl TestRepository {
        fn new(with_remote: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-remote-observer-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let local = root.join("local");
            let remote = root.join("remote.git");
            fs::create_dir(&root).unwrap();
            git(&root, ["init", "--bare", remote.to_str().unwrap()]);
            git(&root, ["init", local.to_str().unwrap()]);
            git(&local, ["config", "user.name", "Mineral Publisher Test"]);
            git(&local, ["config", "user.email", "test@example.invalid"]);
            fs::write(local.join("file.txt"), b"first").unwrap();
            git(&local, ["add", "file.txt"]);
            git(&local, ["commit", "-m", "first"]);
            git(&local, ["branch", "-M", "main"]);
            if with_remote {
                git(
                    &local,
                    ["remote", "add", "origin", remote.to_str().unwrap()],
                );
                git(&local, ["push", "-u", "origin", "main"]);
            }
            Self { root, local }
        }

        fn head(&self) -> String {
            git_stdout(&self.local, ["rev-parse", "HEAD"])
        }

        fn run(&self, destination_ref: &str) -> PublishRun {
            PublishRun::rehydrate(
                PublishRunId::new(1).unwrap(),
                SnapshotId::new(1).unwrap(),
                Sha256::new([1; 32]),
                ManagedRoot::new("content").unwrap(),
                GitRepositoryIdentity::new(&self.local).unwrap(),
                PublicationTarget::new("origin", destination_ref).unwrap(),
                self.head(),
                "b".repeat(40),
                PublishRunPublication::Noop,
                1,
            )
            .unwrap()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn git<const N: usize>(directory: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout<const N: usize>(directory: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

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
            Err(GitRemoteObservationError::MultipleInconsistentResults)
        );
    }

    #[test]
    fn observes_present_and_missing_refs_from_the_real_remote() {
        let repository = TestRepository::new(true);
        let present = GitRemoteObserver::observe(
            RemoteObservationId::new(1).unwrap(),
            &repository.run("refs/heads/main"),
            UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            present.observed(),
            &RemoteRefState::Present {
                commit_oid: GitCommitOid::new(repository.head()).unwrap()
            }
        );

        let missing = GitRemoteObserver::observe(
            RemoteObservationId::new(2).unwrap(),
            &repository.run("refs/heads/missing"),
            UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(missing.observed(), &RemoteRefState::Missing);
    }

    #[test]
    fn observes_remote_instead_of_a_stale_remote_tracking_ref() {
        let repository = TestRepository::new(true);
        let old = repository.head();
        fs::write(repository.local.join("file.txt"), b"second").unwrap();
        git(&repository.local, ["add", "file.txt"]);
        git(&repository.local, ["commit", "-m", "second"]);
        let current = repository.head();
        git(&repository.local, ["push", "origin", "main"]);
        git(
            &repository.local,
            ["update-ref", "refs/remotes/origin/main", old.as_str()],
        );

        let observation = GitRemoteObserver::observe(
            RemoteObservationId::new(1).unwrap(),
            &repository.run("refs/heads/main"),
            UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            observation.observed(),
            &RemoteRefState::Present {
                commit_oid: GitCommitOid::new(current).unwrap()
            }
        );
        assert_eq!(
            git_stdout(&repository.local, ["rev-parse", "refs/remotes/origin/main"]),
            old
        );
    }

    #[test]
    fn missing_remote_is_an_execution_error_not_a_missing_ref() {
        let repository = TestRepository::new(false);
        assert_eq!(
            GitRemoteObserver::observe(
                RemoteObservationId::new(1).unwrap(),
                &repository.run("refs/heads/main"),
                UNIX_EPOCH,
            ),
            Err(GitRemoteObservationError::RemoteNotConfigured)
        );
    }

    #[test]
    fn unavailable_remote_target_is_an_execution_error_not_a_missing_ref() {
        let repository = TestRepository::new(false);
        git(
            &repository.local,
            ["remote", "add", "origin", "missing-remote.git"],
        );
        assert!(
            GitRemoteObserver::observe(
                RemoteObservationId::new(1).unwrap(),
                &repository.run("refs/heads/main"),
                UNIX_EPOCH,
            )
            .is_err()
        );
    }
}
