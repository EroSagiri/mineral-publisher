use crate::{
    domain::TimestampMillis,
    publication::git::{GitRefTarget, GitRemote, RemoteRefState},
};

use super::{
    GitRemoteAdapter, GitRemoteError, PublishRun, RemoteObservationId, RemoteRefObservation,
};

/// Observes the configured remote through the runtime's [`GitRemote`] adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitRemoteObserver;

impl GitRemoteObserver {
    /// Observes a target directly for preparation, before a `PublishRun` exists.
    /// This deliberately does not create an audit record: durable observations are
    /// scoped to a durable publish run by the engine's execution stage.
    pub fn observe_target(
        repository: impl AsRef<std::path::Path>,
        target: &GitRefTarget,
    ) -> Result<RemoteRefState, GitRemoteError> {
        GitRemoteAdapter::new(repository)?.observe_ref(target)
    }

    pub fn observe(
        id: RemoteObservationId,
        publish_run: &PublishRun,
        observed_at: TimestampMillis,
    ) -> Result<RemoteRefObservation, GitRemoteError> {
        let adapter = GitRemoteAdapter::from_locator(publish_run.repository())?;
        let observed = adapter.observe_ref(publish_run.target())?;
        Ok(RemoteRefObservation::new(
            id,
            publish_run,
            observed,
            observed_at,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Sha256, SnapshotId},
        publisher::{GitRepositoryIdentity, PublishRunId, PublishTargetId},
        workflow::ManagedRoot,
    };
    use std::{
        fs,
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
                Some(Sha256::new([1; 32])),
                None,
                None,
                ManagedRoot::new("content").unwrap(),
                PublishTargetId::new(format!("origin:{destination_ref}")).unwrap(),
                GitRepositoryIdentity::new(&self.local)
                    .unwrap()
                    .locator()
                    .clone(),
                GitRefTarget::new("origin", destination_ref).unwrap(),
                self.head(),
                "b".repeat(40),
                None,
                None,
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
    fn observes_present_and_missing_refs_from_the_real_remote() {
        let repository = TestRepository::new(true);
        let present = GitRemoteObserver::observe(
            RemoteObservationId::new(1).unwrap(),
            &repository.run("refs/heads/main"),
            TimestampMillis::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            present.observed(),
            &RemoteRefState::Present {
                commit_oid: crate::publisher::GitCommitOid::new(repository.head()).unwrap()
            }
        );
        assert_eq!(present.observed_at(), TimestampMillis::UNIX_EPOCH);

        let missing = GitRemoteObserver::observe(
            RemoteObservationId::new(2).unwrap(),
            &repository.run("refs/heads/missing"),
            TimestampMillis::from_unix_millis(1_500),
        )
        .unwrap();
        assert_eq!(missing.observed(), &RemoteRefState::Missing);
        assert_eq!(missing.observed_at_unix_ms(), 1_500);
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
            TimestampMillis::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            observation.observed(),
            &RemoteRefState::Present {
                commit_oid: crate::publisher::GitCommitOid::new(current).unwrap()
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
                TimestampMillis::UNIX_EPOCH,
            ),
            Err(GitRemoteError::RemoteNotConfigured)
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
                TimestampMillis::UNIX_EPOCH,
            )
            .is_err()
        );
    }
}
