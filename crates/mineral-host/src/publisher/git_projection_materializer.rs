use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    ports::{BlobStore, ContentStoreError},
    workflow::{
        CurrentTargetEntry, CurrentTargetState, ManagedRoot, PublicationFileMode, TextProjection,
        TextProjectionFile,
    },
};

use super::{GitCurrentTargetAdapter, GitCurrentTargetError, ReviewedGitTree};

static NEXT_TEMPORARY_INDEX: AtomicU64 = AtomicU64::new(1);

/// Materializes the delivery text side into a detached, isolated Git index.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitProjectionMaterializer;

impl GitProjectionMaterializer {
    pub fn materialize<B: BlobStore>(
        repository: impl AsRef<Path>,
        base_commit: &str,
        text: &TextProjection,
        content_store: &B,
    ) -> Result<ReviewedGitTree, GitProjectionMaterializationError> {
        let repository = repository.as_ref();

        // Resolve exactly once. Every later command is pinned to this object ID.
        let current =
            GitCurrentTargetAdapter::read(repository, base_commit, text.managed_root().clone())
                .map_err(GitProjectionMaterializationError::BaseCommit)?;
        let base_commit = current.base_commit().to_owned();
        let base_tree_oid = resolve_tree(repository, &base_commit)?;

        // Verify every desired CAS object before mutating even the repository's
        // unreachable object set or creating the temporary index.
        let blobs = text
            .files()
            .iter()
            .map(|entry| {
                content_store
                    .read(entry.blob_sha256())
                    .map(|bytes| (entry, bytes))
                    .map_err(|source| GitProjectionMaterializationError::ContentStore {
                        target_path: entry.target_path().as_str().to_owned(),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let temporary_index = TemporaryIndex::create()?;
        let operation = materialize_with_index(
            repository,
            &temporary_index.index_path,
            &base_commit,
            &base_tree_oid,
            text,
            &blobs,
        );
        match (operation, temporary_index.cleanup()) {
            (Ok(reviewed), Ok(())) => Ok(reviewed),
            (Ok(_), Err(source)) => Err(GitProjectionMaterializationError::Cleanup { source }),
            (Err(operation), Ok(())) => Err(operation),
            (Err(operation), Err(source)) => {
                Err(GitProjectionMaterializationError::OperationAndCleanup {
                    operation: Box::new(operation),
                    cleanup: source,
                })
            }
        }
    }
}

fn materialize_with_index(
    repository: &Path,
    index_path: &Path,
    base_commit: &str,
    base_tree_oid: &str,
    text: &TextProjection,
    blobs: &[(&TextProjectionFile, Vec<u8>)],
) -> Result<ReviewedGitTree, GitProjectionMaterializationError> {
    git_with_index(
        repository,
        index_path,
        "initialize isolated index",
        ["read-tree", base_commit],
        None,
    )?;

    let tracked = if text.managed_root().is_repository_root() {
        git_with_index(
            repository,
            index_path,
            "enumerate managed index entries",
            ["ls-files", "-z"],
            None,
        )?
    } else {
        git_with_index(
            repository,
            index_path,
            "enumerate managed index entries",
            [
                "ls-files",
                "-z",
                "--",
                &literal_pathspec(text.managed_root()),
            ],
            None,
        )?
    };
    if !tracked.stdout.is_empty() {
        git_with_index(
            repository,
            index_path,
            "remove previous managed index entries",
            ["update-index", "--force-remove", "-z", "--stdin"],
            Some(&tracked.stdout),
        )?;
    }

    for (entry, bytes) in blobs {
        let object_id = hash_blob(repository, bytes)?;
        let output = Command::new("git")
            .current_dir(repository)
            .env("GIT_INDEX_FILE", index_path)
            .args(["update-index", "--add", "--cacheinfo", "100644"])
            .arg(&object_id)
            .arg(entry.target_path().as_str())
            .output()
            .map_err(|source| GitProjectionMaterializationError::Staging {
                target_path: entry.target_path().as_str().to_owned(),
                status: None,
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitProjectionMaterializationError::Staging {
                target_path: entry.target_path().as_str().to_owned(),
                status: output.status.code(),
                stderr: stderr_text(&output),
            });
        }
    }

    let output = git_with_index(
        repository,
        index_path,
        "write candidate tree",
        ["write-tree"],
        None,
    )
    .map_err(|error| match error {
        GitProjectionMaterializationError::IndexCommand { status, stderr, .. } => {
            GitProjectionMaterializationError::WriteTree { status, stderr }
        }
        other => other,
    })?;
    let tree_oid = parse_object_id(&output.stdout, "write candidate tree")?;

    verify_text_tree(repository, &tree_oid, text)?;
    verify_outside_managed_root(repository, base_tree_oid, &tree_oid, text.managed_root())?;

    Ok(ReviewedGitTree::from_parts(
        base_commit,
        base_tree_oid,
        text.projection_sha256(),
        text.snapshot_id(),
        tree_oid,
        text.managed_root().clone(),
    ))
}

fn verify_text_tree(
    repository: &Path,
    tree_oid: &str,
    text: &TextProjection,
) -> Result<(), GitProjectionMaterializationError> {
    let actual =
        GitCurrentTargetAdapter::read_tree(repository, tree_oid, text.managed_root().clone())
            .map_err(GitProjectionMaterializationError::CandidateTreeRead)?;
    let expected = CurrentTargetState::new(
        text.managed_root().clone(),
        text.files()
            .iter()
            .map(|entry| {
                CurrentTargetEntry::with_mode(
                    entry.target_path().clone(),
                    entry.blob_sha256(),
                    PublicationFileMode::Regular,
                )
            })
            .collect(),
    )
    .expect("TextProjection already enforces managed paths and uniqueness");
    if actual != expected {
        return Err(GitProjectionMaterializationError::ProjectionTreeMismatch);
    }
    Ok(())
}

fn verify_outside_managed_root(
    repository: &Path,
    base_tree_oid: &str,
    candidate_tree_oid: &str,
    managed_root: &ManagedRoot,
) -> Result<(), GitProjectionMaterializationError> {
    if managed_root.is_repository_root() {
        return Ok(());
    }
    let output = git_output(
        repository,
        "compare base and candidate trees",
        [
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-z",
            base_tree_oid,
            candidate_tree_oid,
        ],
    )?;
    let root = managed_root.as_str().as_bytes();
    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
    {
        if !path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
        {
            return Err(
                GitProjectionMaterializationError::OutsideManagedRootChanged {
                    path: path.to_vec(),
                },
            );
        }
    }
    Ok(())
}

fn resolve_tree(
    repository: &Path,
    base_commit: &str,
) -> Result<String, GitProjectionMaterializationError> {
    let expression = format!("{base_commit}^{{tree}}");
    let output = git_output(
        repository,
        "resolve base tree",
        ["rev-parse", "--verify", "--end-of-options", &expression],
    )?;
    parse_object_id(&output.stdout, "resolve base tree")
}

fn hash_blob(repository: &Path, bytes: &[u8]) -> Result<String, GitProjectionMaterializationError> {
    let mut child = Command::new("git")
        .current_dir(repository)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| GitProjectionMaterializationError::BlobWrite {
            status: None,
            stderr: source.to_string(),
        })?;
    io::Write::write_all(
        child.stdin.as_mut().expect("piped stdin is available"),
        bytes,
    )
    .map_err(|source| GitProjectionMaterializationError::BlobWrite {
        status: None,
        stderr: source.to_string(),
    })?;
    let output = child.wait_with_output().map_err(|source| {
        GitProjectionMaterializationError::BlobWrite {
            status: None,
            stderr: source.to_string(),
        }
    })?;
    if !output.status.success() {
        return Err(GitProjectionMaterializationError::BlobWrite {
            status: output.status.code(),
            stderr: stderr_text(&output),
        });
    }
    parse_object_id(&output.stdout, "write exact Git blob")
}

fn git_with_index<const N: usize>(
    repository: &Path,
    index_path: &Path,
    operation: &'static str,
    args: [&str; N],
    stdin: Option<&[u8]>,
) -> Result<Output, GitProjectionMaterializationError> {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .env("GIT_INDEX_FILE", index_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child =
        command
            .spawn()
            .map_err(|source| GitProjectionMaterializationError::IndexCommand {
                operation,
                status: None,
                stderr: source.to_string(),
            })?;
    if let Some(input) = stdin {
        io::Write::write_all(
            child.stdin.as_mut().expect("piped stdin is available"),
            input,
        )
        .map_err(|source| GitProjectionMaterializationError::IndexCommand {
            operation,
            status: None,
            stderr: source.to_string(),
        })?;
    }
    let output = child.wait_with_output().map_err(|source| {
        GitProjectionMaterializationError::IndexCommand {
            operation,
            status: None,
            stderr: source.to_string(),
        }
    })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitProjectionMaterializationError::IndexCommand {
            operation,
            status: output.status.code(),
            stderr: stderr_text(&output),
        })
    }
}

fn git_output<const N: usize>(
    repository: &Path,
    operation: &'static str,
    args: [&str; N],
) -> Result<Output, GitProjectionMaterializationError> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()
        .map_err(|source| GitProjectionMaterializationError::GitCommand {
            operation,
            status: None,
            stderr: source.to_string(),
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitProjectionMaterializationError::GitCommand {
            operation,
            status: output.status.code(),
            stderr: stderr_text(&output),
        })
    }
}

fn parse_object_id(
    output: &[u8],
    operation: &'static str,
) -> Result<String, GitProjectionMaterializationError> {
    std::str::from_utf8(output)
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or(GitProjectionMaterializationError::MalformedGitOutput { operation })
}

fn literal_pathspec(managed_root: &ManagedRoot) -> String {
    format!(":(literal){}", managed_root.as_str())
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

struct TemporaryIndex {
    directory: PathBuf,
    index_path: PathBuf,
}

impl TemporaryIndex {
    fn create() -> Result<Self, GitProjectionMaterializationError> {
        for _ in 0..1000 {
            let sequence = NEXT_TEMPORARY_INDEX.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "mineral-publisher-git-index-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        index_path: directory.join("index"),
                        directory,
                    });
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(GitProjectionMaterializationError::TemporaryEnvironment { source });
                }
            }
        }
        Err(GitProjectionMaterializationError::TemporaryEnvironment {
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique isolated index directory",
            ),
        })
    }

    fn cleanup(self) -> Result<(), io::Error> {
        fs::remove_dir_all(self.directory)
    }
}

#[derive(Debug)]
pub enum GitProjectionMaterializationError {
    BaseCommit(GitCurrentTargetError),
    TemporaryEnvironment {
        source: io::Error,
    },
    ContentStore {
        target_path: String,
        source: ContentStoreError,
    },
    GitCommand {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    IndexCommand {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    BlobWrite {
        status: Option<i32>,
        stderr: String,
    },
    Staging {
        target_path: String,
        status: Option<i32>,
        stderr: String,
    },
    WriteTree {
        status: Option<i32>,
        stderr: String,
    },
    MalformedGitOutput {
        operation: &'static str,
    },
    CandidateTreeRead(GitCurrentTargetError),
    ProjectionTreeMismatch,
    OutsideManagedRootChanged {
        path: Vec<u8>,
    },
    Cleanup {
        source: io::Error,
    },
    OperationAndCleanup {
        operation: Box<GitProjectionMaterializationError>,
        cleanup: io::Error,
    },
}

impl fmt::Display for GitProjectionMaterializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BaseCommit(_) => formatter.write_str("repository or base commit is invalid"),
            Self::TemporaryEnvironment { .. } => {
                formatter.write_str("could not create an isolated Git index")
            }
            Self::ContentStore { target_path, .. } => {
                write!(
                    formatter,
                    "could not read exact projection blob for {target_path}"
                )
            }
            Self::GitCommand { operation, .. } | Self::IndexCommand { operation, .. } => {
                write!(
                    formatter,
                    "Git command failed while attempting to {operation}"
                )
            }
            Self::BlobWrite { .. } => formatter.write_str("could not write exact Git blob"),
            Self::Staging { target_path, .. } => {
                write!(formatter, "could not stage projection entry: {target_path}")
            }
            Self::WriteTree { .. } => formatter.write_str("could not write candidate Git tree"),
            Self::MalformedGitOutput { operation } => {
                write!(
                    formatter,
                    "Git returned malformed output while attempting to {operation}"
                )
            }
            Self::CandidateTreeRead(_) => {
                formatter.write_str("could not verify the candidate managed Git tree")
            }
            Self::ProjectionTreeMismatch => formatter.write_str(
                "candidate Git tree managed subtree does not exactly match the projection",
            ),
            Self::OutsideManagedRootChanged { path } => write!(
                formatter,
                "candidate Git tree changed a path outside the managed root: {}",
                String::from_utf8_lossy(path)
            ),
            Self::Cleanup { .. } => formatter.write_str("could not clean up isolated Git index"),
            Self::OperationAndCleanup { operation, .. } => {
                write!(
                    formatter,
                    "{operation}; isolated Git index cleanup also failed"
                )
            }
        }
    }
}

impl Error for GitProjectionMaterializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BaseCommit(source) | Self::CandidateTreeRead(source) => Some(source),
            Self::TemporaryEnvironment { source } | Self::Cleanup { source } => Some(source),
            Self::ContentStore { source, .. } => Some(source),
            Self::OperationAndCleanup { operation, .. } => Some(operation),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        domain::{ContentPath, Sha256, SnapshotId},
        storage::LocalContentStore,
        workflow::{PublishOperation, PublishPlan},
    };

    use super::*;

    static NEXT_REPOSITORY: AtomicU64 = AtomicU64::new(1);

    struct TestRepository {
        path: PathBuf,
        store: LocalContentStore,
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
            let sequence = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-git-materializer-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            let repository = Self {
                store: LocalContentStore::new(path.join("cas")),
                path,
            };
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

        fn tree_file(&self, tree: &str, path: &str) -> Vec<u8> {
            let spec = format!("{tree}:{path}");
            self.git(["show", &spec])
        }

        fn tree_mode(&self, tree: &str, path: &str) -> String {
            let output = self.git(["ls-tree", tree, "--", path]);
            String::from_utf8(output)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
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

    /// The materializer's only input is the delivery text side, so these tests
    /// hand it exact bytes identities directly.
    fn projection(repository: &TestRepository, entries: &[(&str, &[u8])]) -> TextProjection {
        projection_at(repository, entries, root())
    }

    fn projection_at(
        repository: &TestRepository,
        entries: &[(&str, &[u8])],
        managed_root: ManagedRoot,
    ) -> TextProjection {
        let files = entries
            .iter()
            .map(|(path, bytes)| {
                let sha256 = repository.store.store(bytes).unwrap();
                (ContentPath::new(*path).unwrap(), sha256)
            })
            .collect::<Vec<_>>();
        TextProjection::from_parts_for_test(
            SnapshotId::new(7).unwrap(),
            managed_root,
            Sha256::new([0; 32]),
            files,
        )
    }

    fn materialize(
        repository: &TestRepository,
        base: &str,
        projection: &TextProjection,
    ) -> ReviewedGitTree {
        GitProjectionMaterializer::materialize(
            &repository.path,
            base,
            projection,
            &repository.store,
        )
        .unwrap()
    }

    #[test]
    fn adds_modifies_deletes_and_preserves_outside_files() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"outside");
        repository.write("content/a.md", b"AAA");
        repository.write("content/old.md", b"OLD");
        let base = repository.commit_all("base");
        let desired = projection(&repository, &[("a.md", b"BBB"), ("new.md", b"NEW")]);

        let reviewed = materialize(&repository, &base, &desired);

        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "README.md"),
            b"outside"
        );
        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "content/a.md"),
            b"BBB"
        );
        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "content/new.md"),
            b"NEW"
        );
        let missing = format!("{}:content/old.md", reviewed.tree_oid());
        assert!(
            !Command::new("git")
                .current_dir(&repository.path)
                .args(["cat-file", "-e", &missing])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(reviewed.base_commit(), base);
        assert_eq!(reviewed.projection_sha256(), desired.projection_sha256());
        assert_eq!(reviewed.snapshot_id(), desired.snapshot_id());
    }

    #[test]
    fn empty_projection_removes_the_complete_managed_subtree() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"outside");
        repository.write("content/a.md", b"A");
        let base = repository.commit_all("base");
        let desired = projection(&repository, &[]);

        let reviewed = materialize(&repository, &base, &desired);

        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "README.md"),
            b"outside"
        );
        assert!(
            repository
                .git(["ls-tree", reviewed.tree_oid(), "--", "content"])
                .is_empty()
        );
    }

    #[test]
    fn repository_root_projection_replaces_the_complete_git_tree() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"not published");
        repository.write("old.md", b"OLD");
        let base = repository.commit_all("base");
        let desired = projection_at(
            &repository,
            &[("notes/a.md", b"A")],
            ManagedRoot::repository_root(),
        );

        let reviewed = materialize(&repository, &base, &desired);

        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "notes/a.md"),
            b"A"
        );
        let paths = repository.git(["ls-tree", "-r", "--name-only", reviewed.tree_oid()]);
        assert_eq!(paths, b"notes/a.md\n");
    }

    #[test]
    fn exact_binary_and_crlf_bytes_bypass_git_attributes() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write(".gitattributes", b"content/** text eol=lf\n");
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let bytes = b"a\r\n\0\xffb\r\n";
        let desired = projection(&repository, &[("binary.dat", bytes)]);

        let reviewed = materialize(&repository, &base, &desired);

        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "content/binary.dat"),
            bytes
        );
    }

    #[test]
    fn user_worktree_and_index_are_untouched() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"committed");
        let base = repository.commit_all("base");
        repository.write("README.md", b"staged");
        repository.git(["add", "README.md"]);
        repository.write("README.md", b"working");
        let index_before = repository.git(["ls-files", "--stage"]);
        let index_bytes_before = fs::read(repository.path.join(".git/index")).unwrap();
        let desired = projection(&repository, &[("a.md", b"A")]);

        materialize(&repository, &base, &desired);

        assert_eq!(
            fs::read(repository.path.join("README.md")).unwrap(),
            b"working"
        );
        assert_eq!(repository.git(["ls-files", "--stage"]), index_before);
        assert_eq!(
            fs::read(repository.path.join(".git/index")).unwrap(),
            index_bytes_before
        );
    }

    #[test]
    fn resolved_base_commit_is_stable_when_head_has_advanced() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        repository.write("README.md", b"new head");
        repository.commit_all("advance");
        let desired = projection(&repository, &[("a.md", b"A")]);

        let reviewed = materialize(&repository, &base, &desired);

        assert_eq!(reviewed.base_commit(), base);
        assert_eq!(
            repository.tree_file(reviewed.tree_oid(), "README.md"),
            b"base"
        );
    }

    #[test]
    fn identical_projection_is_a_tree_noop() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a.md", b"A");
        let base = repository.commit_all("base");
        let desired = projection(&repository, &[("a.md", b"A")]);

        let reviewed = materialize(&repository, &base, &desired);

        assert!(reviewed.is_noop());
    }

    #[test]
    fn executable_mode_is_normalized_and_visible_to_publish_plan() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a", b"A");
        repository.git(["add", "content/a"]);
        repository.git(["update-index", "--chmod=+x", "content/a"]);
        repository.git(["commit", "--quiet", "-m", "executable"]);
        let base = repository.head();
        let desired = projection(&repository, &[("a", b"A")]);
        let current = GitCurrentTargetAdapter::read(&repository.path, &base, root()).unwrap();

        let plan = PublishPlan::build(current.state(), &desired).unwrap();
        let reviewed = materialize(&repository, &base, &desired);

        assert!(matches!(
            plan.operations(),
            [PublishOperation::Modified { .. }]
        ));
        assert_eq!(
            repository.tree_mode(reviewed.tree_oid(), "content/a"),
            "100644"
        );
        assert!(!reviewed.is_noop());
    }

    #[test]
    fn missing_and_corrupt_cas_blobs_fail_closed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let desired = projection(&repository, &[("a.md", b"A")]);
        let identity = desired.files()[0].blob_sha256();
        fs::remove_file(repository.store.root().join(identity.to_string())).unwrap();
        assert!(matches!(
            GitProjectionMaterializer::materialize(
                &repository.path,
                &base,
                &desired,
                &repository.store
            ),
            Err(GitProjectionMaterializationError::ContentStore {
                source: ContentStoreError::Missing(_),
                ..
            })
        ));

        repository.store.store(b"A").unwrap();
        fs::write(
            repository.store.root().join(identity.to_string()),
            b"corrupt",
        )
        .unwrap();
        assert!(matches!(
            GitProjectionMaterializer::materialize(
                &repository.path,
                &base,
                &desired,
                &repository.store
            ),
            Err(GitProjectionMaterializationError::ContentStore {
                source: ContentStoreError::Corrupt { .. },
                ..
            })
        ));
    }

    #[test]
    fn missing_base_commit_is_typed_and_does_not_fall_back_to_head() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        repository.commit_all("base");
        let desired = projection(&repository, &[]);

        assert!(matches!(
            GitProjectionMaterializer::materialize(
                &repository.path,
                "0000000000000000000000000000000000000000",
                &desired,
                &repository.store,
            ),
            Err(GitProjectionMaterializationError::BaseCommit(
                GitCurrentTargetError::RevisionNotFound { .. }
            ))
        ));
    }

    #[test]
    fn materialization_is_deterministic_and_does_not_create_commits() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let desired = projection(&repository, &[("z.md", b"Z"), ("a.md", b"A")]);
        let commit_count = repository.git(["rev-list", "--count", "HEAD"]);

        let first = materialize(&repository, &base, &desired);
        let second = materialize(&repository, &base, &desired);

        assert_eq!(first, second);
        assert_eq!(repository.head(), base);
        assert_eq!(
            repository.git(["rev-list", "--count", "HEAD"]),
            commit_count
        );
    }
}
