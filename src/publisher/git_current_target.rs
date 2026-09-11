use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::{
    domain::Sha256,
    workflow::{
        CurrentTargetEntry, CurrentTargetState, CurrentTargetStateError, ManagedRoot,
        ProjectionTargetPath, ProjectionTargetPathError,
    },
};

/// Git-specific provenance paired with the publisher-neutral observed target state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCurrentTarget {
    base_commit: String,
    state: CurrentTargetState,
}

impl GitCurrentTarget {
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub fn state(&self) -> &CurrentTargetState {
        &self.state
    }

    pub fn into_state(self) -> CurrentTargetState {
        self.state
    }
}

/// Reads an explicitly selected committed Git tree without consulting the index or worktree.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitCurrentTargetAdapter;

impl GitCurrentTargetAdapter {
    pub fn read(
        repository: impl AsRef<Path>,
        revision: &str,
        managed_root: ManagedRoot,
    ) -> Result<GitCurrentTarget, GitCurrentTargetError> {
        let repository = repository.as_ref();
        validate_repository_path(repository)?;
        ensure_git_repository(repository)?;
        let base_commit = resolve_commit(repository, revision)?;
        let root_entry = read_root_entry(repository, &base_commit, &managed_root)?;

        let Some(root_entry) = root_entry else {
            return Ok(GitCurrentTarget {
                base_commit,
                state: CurrentTargetState::new(managed_root, Vec::new())
                    .map_err(GitCurrentTargetError::CurrentTargetState)?,
            });
        };
        if root_entry.mode != b"040000" || root_entry.kind != b"tree" {
            return Err(GitCurrentTargetError::ManagedRootNotTree {
                path: root_entry.path,
                mode: bytes_for_context(root_entry.mode),
                kind: bytes_for_context(root_entry.kind),
            });
        }

        let output = git_output(
            repository,
            "enumerate managed Git subtree",
            [
                "ls-tree",
                "-r",
                "-z",
                "--full-tree",
                base_commit.as_str(),
                "--",
                &literal_pathspec(&managed_root),
            ],
        )?;
        let tree_entries = parse_tree_entries(&output.stdout)?;
        let mut entries = Vec::with_capacity(tree_entries.len());
        for tree_entry in tree_entries {
            classify_regular_file(&tree_entry)?;
            let target_path = target_path_from_bytes(tree_entry.path)?;
            let blob = read_blob(repository, tree_entry.object_id)?;
            entries.push(CurrentTargetEntry::new(target_path, Sha256::digest(&blob)));
        }

        let state = CurrentTargetState::new(managed_root, entries)
            .map_err(GitCurrentTargetError::CurrentTargetState)?;
        Ok(GitCurrentTarget { base_commit, state })
    }
}

fn target_path_from_bytes(path: &[u8]) -> Result<ProjectionTargetPath, GitCurrentTargetError> {
    let utf8 = std::str::from_utf8(path).map_err(|_| GitCurrentTargetError::NonUtf8TargetPath {
        path: path.to_vec(),
    })?;
    ProjectionTargetPath::new(utf8).map_err(|source| GitCurrentTargetError::InvalidTargetPath {
        path: path.to_vec(),
        source,
    })
}

fn literal_pathspec(managed_root: &ManagedRoot) -> String {
    format!(":(literal){}", managed_root.as_str())
}

fn validate_repository_path(repository: &Path) -> Result<(), GitCurrentTargetError> {
    match fs::metadata(repository) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(GitCurrentTargetError::RepositoryUnavailable {
            path: repository.to_path_buf(),
            reason: "path is not a directory".to_owned(),
        }),
        Err(error) => Err(GitCurrentTargetError::RepositoryUnavailable {
            path: repository.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

fn ensure_git_repository(repository: &Path) -> Result<(), GitCurrentTargetError> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map_err(|error| GitCurrentTargetError::GitCommandFailed {
            operation: "inspect Git repository",
            status: None,
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GitCurrentTargetError::NotRepository {
            path: repository.to_path_buf(),
            stderr: stderr_text(&output),
        })
    }
}

fn resolve_commit(repository: &Path, revision: &str) -> Result<String, GitCurrentTargetError> {
    let peeled = format!("{revision}^{{commit}}");
    let output = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--verify", "--end-of-options", peeled.as_str()])
        .output()
        .map_err(|error| GitCurrentTargetError::GitCommandFailed {
            operation: "resolve Git revision",
            status: None,
            stderr: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(GitCurrentTargetError::RevisionNotFound {
            revision: revision.to_owned(),
            stderr: stderr_text(&output),
        });
    }
    let resolved = std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or(GitCurrentTargetError::MalformedGitOutput {
            operation: "resolve Git revision",
        })?;
    Ok(resolved.to_owned())
}

fn read_root_entry(
    repository: &Path,
    base_commit: &str,
    managed_root: &ManagedRoot,
) -> Result<Option<OwnedTreeEntry>, GitCurrentTargetError> {
    let output = git_output(
        repository,
        "locate managed Git subtree",
        [
            "ls-tree",
            "-z",
            "--full-tree",
            base_commit,
            "--",
            &literal_pathspec(managed_root),
        ],
    )?;
    let entries = parse_tree_entries(&output.stdout)?;
    match entries.as_slice() {
        [] => Ok(None),
        [entry] => Ok(Some(OwnedTreeEntry {
            mode: entry.mode.to_vec(),
            kind: entry.kind.to_vec(),
            path: entry.path.to_vec(),
        })),
        _ => Err(GitCurrentTargetError::MalformedGitOutput {
            operation: "locate managed Git subtree",
        }),
    }
}

fn git_output<const N: usize>(
    repository: &Path,
    operation: &'static str,
    args: [&str; N],
) -> Result<Output, GitCurrentTargetError> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()
        .map_err(|error| GitCurrentTargetError::GitCommandFailed {
            operation,
            status: None,
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitCurrentTargetError::GitCommandFailed {
            operation,
            status: output.status.code(),
            stderr: stderr_text(&output),
        })
    }
}

fn read_blob(repository: &Path, object_id: &[u8]) -> Result<Vec<u8>, GitCurrentTargetError> {
    let object_id =
        std::str::from_utf8(object_id).map_err(|_| GitCurrentTargetError::MalformedGitOutput {
            operation: "parse Git blob object ID",
        })?;
    let output = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "blob", object_id])
        .output()
        .map_err(|error| GitCurrentTargetError::BlobReadFailed {
            object_id: object_id.to_owned(),
            status: None,
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitCurrentTargetError::BlobReadFailed {
            object_id: object_id.to_owned(),
            status: output.status.code(),
            stderr: stderr_text(&output),
        })
    }
}

#[derive(Debug)]
struct TreeEntry<'a> {
    mode: &'a [u8],
    kind: &'a [u8],
    object_id: &'a [u8],
    path: &'a [u8],
}

struct OwnedTreeEntry {
    mode: Vec<u8>,
    kind: Vec<u8>,
    path: Vec<u8>,
}

fn parse_tree_entries(output: &[u8]) -> Result<Vec<TreeEntry<'_>>, GitCurrentTargetError> {
    output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(parse_tree_entry)
        .collect()
}

fn parse_tree_entry(record: &[u8]) -> Result<TreeEntry<'_>, GitCurrentTargetError> {
    let tab = record.iter().position(|byte| *byte == b'\t').ok_or(
        GitCurrentTargetError::MalformedGitOutput {
            operation: "parse Git tree entry",
        },
    )?;
    let mut header = record[..tab].split(|byte| *byte == b' ');
    let mode = header.next();
    let kind = header.next();
    let object_id = header.next();
    if header.next().is_some()
        || mode.is_none_or(<[u8]>::is_empty)
        || kind.is_none_or(<[u8]>::is_empty)
        || object_id.is_none_or(<[u8]>::is_empty)
        || record[tab + 1..].is_empty()
    {
        return Err(GitCurrentTargetError::MalformedGitOutput {
            operation: "parse Git tree entry",
        });
    }
    Ok(TreeEntry {
        mode: mode.expect("checked above"),
        kind: kind.expect("checked above"),
        object_id: object_id.expect("checked above"),
        path: &record[tab + 1..],
    })
}

fn classify_regular_file(entry: &TreeEntry<'_>) -> Result<(), GitCurrentTargetError> {
    match (entry.mode, entry.kind) {
        (b"100644" | b"100755", b"blob") => Ok(()),
        (b"120000", b"blob") => Err(GitCurrentTargetError::UnsupportedSymlink {
            path: entry.path.to_vec(),
        }),
        (b"160000", b"commit") => Err(GitCurrentTargetError::UnsupportedGitlink {
            path: entry.path.to_vec(),
        }),
        _ => Err(GitCurrentTargetError::UnsupportedEntry {
            path: entry.path.to_vec(),
            mode: bytes_for_context(entry.mode),
            kind: bytes_for_context(entry.kind),
        }),
    }
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

fn bytes_for_context(bytes: impl AsRef<[u8]>) -> String {
    String::from_utf8_lossy(bytes.as_ref()).into_owned()
}

#[derive(Debug)]
pub enum GitCurrentTargetError {
    RepositoryUnavailable {
        path: PathBuf,
        reason: String,
    },
    NotRepository {
        path: PathBuf,
        stderr: String,
    },
    RevisionNotFound {
        revision: String,
        stderr: String,
    },
    GitCommandFailed {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    ManagedRootNotTree {
        path: Vec<u8>,
        mode: String,
        kind: String,
    },
    InvalidTargetPath {
        path: Vec<u8>,
        source: ProjectionTargetPathError,
    },
    NonUtf8TargetPath {
        path: Vec<u8>,
    },
    UnsupportedSymlink {
        path: Vec<u8>,
    },
    UnsupportedGitlink {
        path: Vec<u8>,
    },
    UnsupportedEntry {
        path: Vec<u8>,
        mode: String,
        kind: String,
    },
    BlobReadFailed {
        object_id: String,
        status: Option<i32>,
        stderr: String,
    },
    MalformedGitOutput {
        operation: &'static str,
    },
    CurrentTargetState(CurrentTargetStateError),
}

impl fmt::Display for GitCurrentTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryUnavailable { path, reason } => {
                write!(
                    formatter,
                    "Git repository is unavailable at {}: {reason}",
                    path.display()
                )
            }
            Self::NotRepository { path, .. } => {
                write!(
                    formatter,
                    "path is not a Git repository: {}",
                    path.display()
                )
            }
            Self::RevisionNotFound { revision, .. } => {
                write!(
                    formatter,
                    "Git revision does not resolve to a commit: {revision}"
                )
            }
            Self::GitCommandFailed { operation, .. } => {
                write!(
                    formatter,
                    "Git command failed while attempting to {operation}"
                )
            }
            Self::ManagedRootNotTree { path, .. } => write!(
                formatter,
                "managed root is not a Git tree: {}",
                String::from_utf8_lossy(path)
            ),
            Self::InvalidTargetPath { path, source } => write!(
                formatter,
                "Git target path is invalid ({}): {source}",
                String::from_utf8_lossy(path)
            ),
            Self::NonUtf8TargetPath { path } => write!(
                formatter,
                "Git target path is not UTF-8: {}",
                String::from_utf8_lossy(path)
            ),
            Self::UnsupportedSymlink { path } => write!(
                formatter,
                "managed Git tree contains an unsupported symbolic link: {}",
                String::from_utf8_lossy(path)
            ),
            Self::UnsupportedGitlink { path } => write!(
                formatter,
                "managed Git tree contains an unsupported gitlink: {}",
                String::from_utf8_lossy(path)
            ),
            Self::UnsupportedEntry { path, mode, kind } => write!(
                formatter,
                "managed Git tree contains an unsupported entry {} ({mode} {kind})",
                String::from_utf8_lossy(path)
            ),
            Self::BlobReadFailed { object_id, .. } => {
                write!(formatter, "failed to read Git blob bytes: {object_id}")
            }
            Self::MalformedGitOutput { operation } => {
                write!(
                    formatter,
                    "Git returned malformed output while attempting to {operation}"
                )
            }
            Self::CurrentTargetState(error) => {
                write!(
                    formatter,
                    "failed to construct current target state: {error}"
                )
            }
        }
    }
}

impl Error for GitCurrentTargetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTargetPath { source, .. } => Some(source),
            Self::CurrentTargetState(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    use crate::{
        domain::{ContentPath, Snapshot, SnapshotFile, SnapshotId, SourceId},
        workflow::{FinalPublicationSet, PublicProjection, PublishOperation, PublishPlan},
    };

    use super::*;

    static NEXT_REPOSITORY: AtomicU64 = AtomicU64::new(1);

    struct TestRepository {
        path: PathBuf,
    }

    impl TestRepository {
        fn new() -> Option<Self> {
            if !Command::new("git")
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
            {
                return None;
            }
            let unique = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-git-current-target-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            let repository = Self { path };
            repository.git(["init", "--quiet"]);
            repository.git(["config", "user.name", "Mineral Publisher Tests"]);
            repository.git(["config", "user.email", "tests@mineral.invalid"]);
            repository.git(["config", "core.autocrlf", "false"]);
            Some(repository)
        }

        fn git<const N: usize>(&self, args: [&str; N]) -> Vec<u8> {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn git_with_stdin<const N: usize>(&self, args: [&str; N], input: &[u8]) -> Vec<u8> {
            let mut child = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self
                .path
                .join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn commit_all(&self, message: &str) -> String {
            self.git(["add", "-A"]);
            self.git(["commit", "--quiet", "-m", message]);
            self.head()
        }

        fn head(&self) -> String {
            String::from_utf8(self.git(["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_owned()
        }

        fn add_index_entry(&self, mode: &str, object_id: &str, path: &str) {
            let cache_info = format!("{mode},{object_id},{path}");
            self.git(["update-index", "--add", "--cacheinfo", &cache_info]);
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn root() -> ManagedRoot {
        ManagedRoot::new("content").unwrap()
    }

    fn read(repository: &TestRepository, revision: &str) -> GitCurrentTarget {
        GitCurrentTargetAdapter::read(&repository.path, revision, root()).unwrap()
    }

    #[test]
    fn reads_only_regular_files_inside_the_managed_tree() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"outside");
        repository.write("content/a.md", b"A");
        repository.write("content/image.png", &[0, 1, 2, 255]);
        let commit = repository.commit_all("basic tree");

        let target = read(&repository, &commit);

        assert_eq!(target.base_commit(), commit);
        assert_eq!(target.state().entries().len(), 2);
        assert_eq!(
            target.state().entries()[0].target_path().as_str(),
            "content/a.md"
        );
        assert_eq!(
            target.state().entries()[0].blob_sha256(),
            Sha256::digest(b"A")
        );
        assert_eq!(
            target.state().entries()[1].target_path().as_str(),
            "content/image.png"
        );
        assert_eq!(
            target.state().entries()[1].blob_sha256(),
            Sha256::digest(&[0, 1, 2, 255])
        );
    }

    #[test]
    fn absent_managed_root_is_a_successful_empty_state() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"outside");
        let commit = repository.commit_all("no managed tree");

        let target = read(&repository, &commit);

        assert!(target.state().entries().is_empty());
    }

    #[test]
    fn hashes_exact_lf_crlf_and_binary_blob_bytes() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        let fixtures: [(&str, &[u8]); 3] = [
            ("content/lf.txt", b"one\ntwo\n"),
            ("content/crlf.txt", b"one\r\ntwo\r\n"),
            ("content/binary.bin", &[0, 13, 10, 255, 128]),
        ];
        for (path, bytes) in fixtures {
            repository.write(path, bytes);
        }
        let commit = repository.commit_all("exact bytes");

        let target = read(&repository, &commit);

        for (path, bytes) in fixtures {
            let entry = target
                .state()
                .entries()
                .iter()
                .find(|entry| entry.target_path().as_str() == path)
                .unwrap();
            assert_eq!(entry.blob_sha256(), Sha256::digest(bytes));
        }
    }

    #[test]
    fn explicit_historical_commit_ignores_new_head() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a.md", b"AAA");
        let old_commit = repository.commit_all("old");
        repository.write("content/a.md", b"BBB");
        repository.commit_all("new");

        let target = read(&repository, &old_commit);

        assert_eq!(target.base_commit(), old_commit);
        assert_eq!(
            target.state().entries()[0].blob_sha256(),
            Sha256::digest(b"AAA")
        );
    }

    #[test]
    fn dirty_and_staged_changes_do_not_affect_committed_state() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a.md", b"AAA");
        let commit = repository.commit_all("committed");
        repository.write("content/a.md", b"BBB");
        repository.git(["add", "content/a.md"]);
        repository.write("content/a.md", b"CCC");

        let target = read(&repository, "HEAD");

        assert_eq!(target.base_commit(), commit);
        assert_eq!(
            target.state().entries()[0].blob_sha256(),
            Sha256::digest(b"AAA")
        );
    }

    #[test]
    fn symlinks_fail_closed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        repository.commit_all("base");
        let object_id = String::from_utf8(
            repository.git_with_stdin(["hash-object", "-w", "--stdin"], b"destination"),
        )
        .unwrap();
        repository.add_index_entry("120000", object_id.trim(), "content/link");
        repository.git(["commit", "--quiet", "-m", "symlink"]);

        assert!(matches!(
            GitCurrentTargetAdapter::read(&repository.path, "HEAD", root()),
            Err(GitCurrentTargetError::UnsupportedSymlink { .. })
        ));
    }

    #[test]
    fn gitlinks_fail_closed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        repository.add_index_entry("160000", &base, "content/submodule");
        repository.git(["commit", "--quiet", "-m", "gitlink"]);

        assert!(matches!(
            GitCurrentTargetAdapter::read(&repository.path, "HEAD", root()),
            Err(GitCurrentTargetError::UnsupportedGitlink { .. })
        ));
    }

    #[test]
    fn executable_regular_blob_is_read_as_file_bytes() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/run.sh", b"#!/bin/sh\nexit 0\n");
        repository.git(["add", "content/run.sh"]);
        repository.git(["update-index", "--chmod=+x", "content/run.sh"]);
        repository.git(["commit", "--quiet", "-m", "executable"]);

        let target = read(&repository, "HEAD");

        assert_eq!(
            target.state().entries()[0].blob_sha256(),
            Sha256::digest(b"#!/bin/sh\nexit 0\n")
        );
    }

    #[test]
    fn missing_revision_is_typed_and_not_an_empty_state() {
        let Some(repository) = TestRepository::new() else {
            return;
        };

        assert!(matches!(
            GitCurrentTargetAdapter::read(&repository.path, "HEAD", root()),
            Err(GitCurrentTargetError::RevisionNotFound { .. })
        ));
    }

    #[test]
    fn non_repository_is_typed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        let directory = repository.path.with_extension("not-a-repository");
        fs::create_dir(&directory).unwrap();

        assert!(matches!(
            GitCurrentTargetAdapter::read(&directory, "HEAD", root()),
            Err(GitCurrentTargetError::NotRepository { .. })
        ));
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn unavailable_repository_path_is_typed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        let missing = repository.path.with_extension("missing");

        assert!(matches!(
            GitCurrentTargetAdapter::read(missing, "HEAD", root()),
            Err(GitCurrentTargetError::RepositoryUnavailable { .. })
        ));
    }

    #[test]
    fn same_commit_is_deterministic() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/z.md", b"Z");
        repository.write("content/a.md", b"A");
        let commit = repository.commit_all("deterministic");

        let first = read(&repository, &commit);
        let second = read(&repository, &commit);

        assert_eq!(first, second);
    }

    #[test]
    fn non_utf8_tree_path_is_rejected_without_lossy_conversion() {
        let output = b"100644 blob 0123456789abcdef\tcontent/invalid-\xff\0";
        let entries = parse_tree_entries(output).unwrap();

        assert!(matches!(
            target_path_from_bytes(entries[0].path),
            Err(GitCurrentTargetError::NonUtf8TargetPath { path }) if path == entries[0].path
        ));
    }

    #[test]
    fn git_paths_still_pass_through_projection_target_validation() {
        assert!(matches!(
            target_path_from_bytes(b"content/ambiguous\\path"),
            Err(GitCurrentTargetError::InvalidTargetPath { .. })
        ));
    }

    #[test]
    fn reads_committed_tree_from_a_bare_repository() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a.md", b"bare");
        let commit = repository.commit_all("bare source");
        let bare_path = repository.path.with_extension("bare.git");
        let output = Command::new("git")
            .args(["clone", "--quiet", "--bare"])
            .arg(&repository.path)
            .arg(&bare_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git clone --bare failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let target = GitCurrentTargetAdapter::read(&bare_path, &commit, root()).unwrap();

        assert_eq!(target.base_commit(), commit);
        assert_eq!(
            target.state().entries()[0].blob_sha256(),
            Sha256::digest(b"bare")
        );
        fs::remove_dir_all(bare_path).unwrap();
    }

    #[test]
    fn adapter_state_integrates_with_publish_plan() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a.md", b"AAA");
        let commit = repository.commit_all("current");
        let current = read(&repository, &commit);
        let desired_a = Sha256::digest(b"BBB");
        let desired_new = Sha256::digest(b"CCC");
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![
                SnapshotFile::new(ContentPath::new("a.md").unwrap(), 3, desired_a, None),
                SnapshotFile::new(ContentPath::new("new.md").unwrap(), 3, desired_new, None),
            ],
        )
        .unwrap();
        let publication_set = FinalPublicationSet::from_parts_for_test(
            SnapshotId::new(1).unwrap(),
            vec![
                ContentPath::new("a.md").unwrap(),
                ContentPath::new("new.md").unwrap(),
            ],
            vec![],
        );
        let projection = PublicProjection::build(&publication_set, &snapshot, root()).unwrap();

        let plan = PublishPlan::build(current.state(), &projection).unwrap();

        assert!(matches!(
            &plan.operations()[0],
            PublishOperation::Modified { target_path, .. }
                if target_path.as_str() == "content/a.md"
        ));
        assert!(matches!(
            &plan.operations()[1],
            PublishOperation::Added { target_path, .. }
                if target_path.as_str() == "content/new.md"
        ));
    }
}
