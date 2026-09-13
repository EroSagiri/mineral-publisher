//! Adapter conformance tests for the native Git remote.
//!
//! These tests build real Git state — a bare remote, a local repository, and a
//! second actor that advances or rewinds the remote — and then check that
//! `observe_ref` and `compare_and_swap` report what Git actually did. The engine's
//! classification of those facts is covered by the portable executor tests and by
//! the end-to-end publication tests; what can only be established here is that the
//! adapter's facts match real Git behaviour.

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::publisher::{
        CasOutcome, GitRefTarget, GitRemote, GitRemoteAdapter, RefUpdate, RemoteRefState,
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    /// A local repository, a bare remote it pushes to, and a second actor clone
    /// that can move the remote without the local repository noticing.
    struct TestRepository {
        root: PathBuf,
        local: PathBuf,
        actor: PathBuf,
        /// An older commit the remote no longer holds, so a rewind can be built.
        older: String,
        /// The commit the remote holds and a publication intent expects as its base.
        base: String,
        /// The commit the local repository prepared.
        prepared: String,
    }

    impl TestRepository {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-git-remote-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let local = root.join("local");
            let actor = root.join("actor");
            let remote = root.join("remote.git");
            fs::create_dir(&root).unwrap();
            git(&root, &["init", "--bare", remote.to_str().unwrap()]);
            git(&root, &["init", local.to_str().unwrap()]);
            configure_identity(&local);

            fs::write(local.join("file.txt"), b"older").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "older"]);
            let older = git_stdout(&local, &["rev-parse", "HEAD"]);
            fs::write(local.join("file.txt"), b"base").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "base"]);
            let base = git_stdout(&local, &["rev-parse", "HEAD"]);
            git(&local, &["branch", "-M", "main"]);
            git(
                &local,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(&local, &["push", "-u", "origin", "main"]);
            git(
                &root,
                &["clone", remote.to_str().unwrap(), actor.to_str().unwrap()],
            );
            git(&actor, &["checkout", "-b", "main", "origin/main"]);
            configure_identity(&actor);

            fs::write(local.join("file.txt"), b"prepared").unwrap();
            git(&local, &["add", "file.txt"]);
            git(&local, &["commit", "-m", "prepared"]);
            let prepared = git_stdout(&local, &["rev-parse", "HEAD"]);

            Self {
                root,
                local,
                actor,
                older,
                base,
                prepared,
            }
        }

        fn adapter(&self) -> GitRemoteAdapter {
            GitRemoteAdapter::new(&self.local).unwrap()
        }

        fn target(&self) -> GitRefTarget {
            GitRefTarget::new("origin", "refs/heads/main").unwrap()
        }

        fn remote_oid(&self) -> String {
            git_stdout(&self.local, &["ls-remote", "origin", "refs/heads/main"])
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
        }

        fn actor_commit_and_push(&self, contents: &[u8], message: &str) -> String {
            fs::write(self.actor.join("file.txt"), contents).unwrap();
            git(&self.actor, &["add", "file.txt"]);
            git(&self.actor, &["commit", "-m", message]);
            let oid = git_stdout(&self.actor, &["rev-parse", "HEAD"]);
            git(&self.actor, &["push", "origin", "HEAD:refs/heads/main"]);
            oid
        }

        fn update(&self, expected_old: &str, new_commit: &str) -> RefUpdate {
            RefUpdate::new(
                self.target(),
                crate::publisher::GitCommitOid::new(expected_old.to_owned()).unwrap(),
                crate::publisher::GitCommitOid::new(new_commit.to_owned()).unwrap(),
            )
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn configure_identity(repository: &Path) {
        git(
            repository,
            &["config", "user.name", "Mineral Publisher Test"],
        );
        git(
            repository,
            &["config", "user.email", "test@example.invalid"],
        );
    }

    fn git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn an_exact_compare_and_swap_moves_the_remote_and_is_confirmed_by_a_fresh_observation() {
        let repository = TestRepository::new();
        let remote = repository.adapter();
        let target = repository.target();

        let before = remote.observe_ref(&target).unwrap();
        assert_eq!(
            before,
            RemoteRefState::Present {
                commit_oid: crate::publisher::GitCommitOid::new(repository.base.clone()).unwrap()
            }
        );
        let outcome = remote
            .compare_and_swap(&repository.update(&repository.base, &repository.prepared))
            .unwrap();
        assert_eq!(outcome, CasOutcome::Updated);
        assert_eq!(repository.remote_oid(), repository.prepared);
        assert_eq!(
            remote.observe_ref(&target).unwrap(),
            RemoteRefState::Present {
                commit_oid: crate::publisher::GitCommitOid::new(repository.prepared.clone())
                    .unwrap()
            }
        );
    }

    #[test]
    fn a_concurrent_advance_is_rejected_by_the_exact_lease() {
        let repository = TestRepository::new();
        let remote = repository.adapter();
        let advanced = repository.actor_commit_and_push(b"advanced", "advanced");

        let outcome = remote
            .compare_and_swap(&repository.update(&repository.base, &repository.prepared))
            .unwrap();

        assert_eq!(outcome, CasOutcome::Rejected);
        // The concurrent advance is still what the remote holds: no overwrite.
        assert_eq!(repository.remote_oid(), advanced);
    }

    #[test]
    fn a_remote_rewind_is_rejected_even_when_the_new_commit_would_fast_forward() {
        let repository = TestRepository::new();
        // The prepared commit descends from the older commit the actor rewinds to,
        // so a "push if fast-forward" approximation would have accepted this
        // update; only the exact lease rejects it.
        git(
            &repository.local,
            &[
                "merge-base",
                "--is-ancestor",
                &repository.older,
                &repository.prepared,
            ],
        );
        let remote = repository.adapter();
        git(&repository.actor, &["reset", "--hard", &repository.older]);
        git(
            &repository.actor,
            &["push", "--force", "origin", "HEAD:refs/heads/main"],
        );
        let rewound = repository.remote_oid();
        assert_eq!(rewound, repository.older);

        let outcome = remote
            .compare_and_swap(&repository.update(&repository.base, &repository.prepared))
            .unwrap();

        assert_eq!(outcome, CasOutcome::Rejected);
        assert_eq!(repository.remote_oid(), rewound);
    }

    #[test]
    fn a_stale_remote_tracking_ref_does_not_control_the_explicit_lease() {
        let repository = TestRepository::new();
        let remote = repository.adapter();
        // A stale remote-tracking ref must not become the lease value.
        git(
            &repository.local,
            &[
                "update-ref",
                "refs/remotes/origin/main",
                repository.base.as_str(),
            ],
        );

        let outcome = remote
            .compare_and_swap(&repository.update(&repository.base, &repository.prepared))
            .unwrap();

        assert_eq!(outcome, CasOutcome::Updated);
        assert_eq!(repository.remote_oid(), repository.prepared);
    }

    #[test]
    fn a_compare_and_swap_does_not_modify_the_worktree_index_head_or_local_branch() {
        let repository = TestRepository::new();
        let remote = repository.adapter();
        fs::write(repository.local.join("file.txt"), b"dirty").unwrap();
        fs::write(repository.local.join("staged.txt"), b"staged").unwrap();
        git(&repository.local, &["add", "staged.txt"]);
        let before_status = git_stdout(&repository.local, &["status", "--porcelain=v1"]);
        let before_head = git_stdout(&repository.local, &["rev-parse", "HEAD"]);
        let before_branch = git_stdout(&repository.local, &["rev-parse", "refs/heads/main"]);

        remote
            .compare_and_swap(&repository.update(&repository.base, &repository.prepared))
            .unwrap();

        assert_eq!(
            git_stdout(&repository.local, &["status", "--porcelain=v1"]),
            before_status
        );
        assert_eq!(
            git_stdout(&repository.local, &["rev-parse", "HEAD"]),
            before_head
        );
        assert_eq!(
            git_stdout(&repository.local, &["rev-parse", "refs/heads/main"]),
            before_branch
        );
    }
}
