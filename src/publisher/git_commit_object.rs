use std::{
    error::Error,
    fmt, fs, io,
    path::Path,
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    domain::{Sha256, SnapshotId},
    workflow::ManagedRoot,
};

use super::ReviewedGitTree;

/// Explicit identity and message used to create one Mineral Publisher commit object.
///
/// V1 deliberately uses the same identity for author and committer. When no timestamp is
/// supplied, Git records the current time. A fixed timestamp is intended for controlled tests
/// and reproducible publication attempts, not as a production default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitMetadata {
    author_name: String,
    author_email: String,
    message: String,
    timestamp: Option<SystemTime>,
}

impl GitCommitMetadata {
    pub fn new(
        author_name: impl Into<String>,
        author_email: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, GitCommitMetadataError> {
        Self::build(
            author_name.into(),
            author_email.into(),
            message.into(),
            None,
        )
    }

    pub fn with_timestamp(
        author_name: impl Into<String>,
        author_email: impl Into<String>,
        message: impl Into<String>,
        timestamp: SystemTime,
    ) -> Result<Self, GitCommitMetadataError> {
        Self::build(
            author_name.into(),
            author_email.into(),
            message.into(),
            Some(timestamp),
        )
    }

    fn build(
        author_name: String,
        author_email: String,
        message: String,
        timestamp: Option<SystemTime>,
    ) -> Result<Self, GitCommitMetadataError> {
        validate_identity_field(&author_name, GitCommitMetadataError::InvalidAuthorName)?;
        validate_identity_field(&author_email, GitCommitMetadataError::InvalidAuthorEmail)?;
        if message.is_empty() || message.contains('\0') {
            return Err(GitCommitMetadataError::InvalidMessage);
        }
        if timestamp.is_some_and(|value| value.duration_since(UNIX_EPOCH).is_err()) {
            return Err(GitCommitMetadataError::InvalidTimestamp);
        }
        Ok(Self {
            author_name,
            author_email,
            message,
            timestamp,
        })
    }

    pub fn author_name(&self) -> &str {
        &self.author_name
    }

    pub fn author_email(&self) -> &str {
        &self.author_email
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn timestamp(&self) -> Option<SystemTime> {
        self.timestamp
    }
}

fn validate_identity_field(
    value: &str,
    error: GitCommitMetadataError,
) -> Result<(), GitCommitMetadataError> {
    if value.trim().is_empty() || value.contains(['\0', '\n', '\r', '<', '>']) {
        Err(error)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitCommitMetadataError {
    InvalidAuthorName,
    InvalidAuthorEmail,
    InvalidMessage,
    InvalidTimestamp,
}

impl fmt::Display for GitCommitMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAuthorName => formatter.write_str("commit author name is invalid"),
            Self::InvalidAuthorEmail => formatter.write_str("commit author email is invalid"),
            Self::InvalidMessage => formatter.write_str("commit message is invalid"),
            Self::InvalidTimestamp => formatter.write_str("commit timestamp is before Unix epoch"),
        }
    }
}

impl Error for GitCommitMetadataError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitCommitResult {
    Noop(GitCommitNoop),
    Created(ReviewedGitCommit),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitNoop {
    base_commit: String,
    tree_oid: String,
    projection_sha256: Sha256,
    snapshot_id: SnapshotId,
    managed_root: ManagedRoot,
}

impl GitCommitNoop {
    fn from_reviewed_tree(reviewed: &ReviewedGitTree) -> Self {
        Self {
            base_commit: reviewed.base_commit().to_owned(),
            tree_oid: reviewed.tree_oid().to_owned(),
            projection_sha256: reviewed.projection_sha256(),
            snapshot_id: reviewed.snapshot_id(),
            managed_root: reviewed.managed_root().clone(),
        }
    }

    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub fn tree_oid(&self) -> &str {
        &self.tree_oid
    }

    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }
}

/// An immutable fact binding a verified Git commit object to its reviewed inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewedGitCommit {
    commit_oid: String,
    tree_oid: String,
    base_commit: String,
    projection_sha256: Sha256,
    snapshot_id: SnapshotId,
    managed_root: ManagedRoot,
}

impl ReviewedGitCommit {
    pub fn commit_oid(&self) -> &str {
        &self.commit_oid
    }

    pub fn tree_oid(&self) -> &str {
        &self.tree_oid
    }

    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GitCommitObjectCreator;

impl GitCommitObjectCreator {
    pub fn create(
        repository: impl AsRef<Path>,
        reviewed: &ReviewedGitTree,
        metadata: &GitCommitMetadata,
    ) -> Result<GitCommitResult, GitCommitObjectError> {
        if reviewed.is_noop() {
            return Ok(GitCommitResult::Noop(GitCommitNoop::from_reviewed_tree(
                reviewed,
            )));
        }

        let repository = repository.as_ref();
        verify_repository(repository)?;
        verify_object(
            repository,
            reviewed.base_commit(),
            "commit",
            GitCommitObjectError::ReviewedBaseCommitMissing,
        )?;
        verify_object(
            repository,
            reviewed.tree_oid(),
            "tree",
            GitCommitObjectError::ReviewedTreeMissing,
        )?;

        let commit_oid = create_commit(repository, reviewed, metadata)?;
        let facts = read_commit(repository, &commit_oid)?;
        verify_commit_facts(reviewed.tree_oid(), reviewed.base_commit(), &facts)?;

        Ok(GitCommitResult::Created(ReviewedGitCommit {
            commit_oid,
            tree_oid: reviewed.tree_oid().to_owned(),
            base_commit: reviewed.base_commit().to_owned(),
            projection_sha256: reviewed.projection_sha256(),
            snapshot_id: reviewed.snapshot_id(),
            managed_root: reviewed.managed_root().clone(),
        }))
    }
}

fn verify_repository(repository: &Path) -> Result<(), GitCommitObjectError> {
    let expected = fs::canonicalize(repository).map_err(|source| {
        GitCommitObjectError::RepositoryUnavailable {
            status: None,
            stderr: source.to_string(),
        }
    })?;
    let bare_output = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
        .map_err(|source| GitCommitObjectError::RepositoryUnavailable {
            status: None,
            stderr: source.to_string(),
        })?;
    if !bare_output.status.success() {
        return Err(GitCommitObjectError::RepositoryUnavailable {
            status: bare_output.status.code(),
            stderr: stderr_text(&bare_output),
        });
    }
    let bare = match std::str::from_utf8(&bare_output.stdout).map(str::trim) {
        Ok("true") => true,
        Ok("false") => false,
        _ => {
            return Err(GitCommitObjectError::RepositoryUnavailable {
                status: bare_output.status.code(),
                stderr: "Git returned malformed repository kind".to_owned(),
            });
        }
    };
    let identity_arg = if bare {
        "--absolute-git-dir"
    } else {
        "--show-toplevel"
    };
    let output = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "--path-format=absolute", identity_arg])
        .output()
        .map_err(|source| GitCommitObjectError::RepositoryUnavailable {
            status: None,
            stderr: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(GitCommitObjectError::RepositoryUnavailable {
            status: output.status.code(),
            stderr: stderr_text(&output),
        });
    }
    let selected = std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(Path::new)
        .ok_or_else(|| GitCommitObjectError::RepositoryUnavailable {
            status: output.status.code(),
            stderr: "Git returned a malformed repository path".to_owned(),
        })?;
    let selected = fs::canonicalize(selected).map_err(|source| {
        GitCommitObjectError::RepositoryUnavailable {
            status: None,
            stderr: source.to_string(),
        }
    })?;
    if selected != expected {
        return Err(GitCommitObjectError::RepositoryUnavailable {
            status: output.status.code(),
            stderr: "the supplied path is not the selected repository root".to_owned(),
        });
    }
    Ok(())
}

fn verify_object(
    repository: &Path,
    oid: &str,
    kind: &str,
    error: GitCommitObjectError,
) -> Result<(), GitCommitObjectError> {
    let expression = format!("{oid}^{{{kind}}}");
    let output = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "-e", &expression])
        .output()
        .map_err(|source| GitCommitObjectError::RepositoryUnavailable {
            status: None,
            stderr: source.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(error)
    }
}

fn create_commit(
    repository: &Path,
    reviewed: &ReviewedGitTree,
    metadata: &GitCommitMetadata,
) -> Result<String, GitCommitObjectError> {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args(["-c", "commit.gpgSign=false", "commit-tree"])
        .arg(reviewed.tree_oid())
        .args(["-p", reviewed.base_commit()])
        .env("GIT_AUTHOR_NAME", metadata.author_name())
        .env("GIT_AUTHOR_EMAIL", metadata.author_email())
        .env("GIT_COMMITTER_NAME", metadata.author_name())
        .env("GIT_COMMITTER_EMAIL", metadata.author_email())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(timestamp) = metadata.timestamp() {
        let seconds = timestamp
            .duration_since(UNIX_EPOCH)
            .expect("GitCommitMetadata validates timestamps")
            .as_secs();
        let date = format!("@{seconds} +0000");
        command
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", date);
    } else {
        command
            .env_remove("GIT_AUTHOR_DATE")
            .env_remove("GIT_COMMITTER_DATE");
    }
    let mut child =
        command
            .spawn()
            .map_err(|source| GitCommitObjectError::CommitCreationFailed {
                status: None,
                stderr: source.to_string(),
            })?;
    io::Write::write_all(
        child.stdin.as_mut().expect("piped stdin is available"),
        metadata.message().as_bytes(),
    )
    .map_err(|source| GitCommitObjectError::CommitCreationFailed {
        status: None,
        stderr: source.to_string(),
    })?;
    let output =
        child
            .wait_with_output()
            .map_err(|source| GitCommitObjectError::CommitCreationFailed {
                status: None,
                stderr: source.to_string(),
            })?;
    if !output.status.success() {
        return Err(GitCommitObjectError::CommitCreationFailed {
            status: output.status.code(),
            stderr: stderr_text(&output),
        });
    }
    parse_oid(&output.stdout).ok_or(GitCommitObjectError::MalformedCommitCreationOutput)
}

#[derive(Debug, Eq, PartialEq)]
struct CommitFacts {
    tree_oid: String,
    parents: Vec<String>,
}

fn read_commit(repository: &Path, commit_oid: &str) -> Result<CommitFacts, GitCommitObjectError> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "-p", commit_oid])
        .output()
        .map_err(|source| GitCommitObjectError::CommitReadFailed {
            status: None,
            stderr: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(GitCommitObjectError::CommitReadFailed {
            status: output.status.code(),
            stderr: stderr_text(&output),
        });
    }
    parse_commit(&output.stdout)
}

fn parse_commit(output: &[u8]) -> Result<CommitFacts, GitCommitObjectError> {
    let text = std::str::from_utf8(output).map_err(|_| GitCommitObjectError::MalformedCommit)?;
    let mut trees = Vec::new();
    let mut parents = Vec::new();
    for line in text.lines().take_while(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("tree ") {
            trees.push(value);
        } else if let Some(value) = line.strip_prefix("parent ") {
            parents.push(value.to_owned());
        }
    }
    if trees.len() != 1 || !is_oid(trees[0]) || parents.iter().any(|parent| !is_oid(parent)) {
        return Err(GitCommitObjectError::MalformedCommit);
    }
    Ok(CommitFacts {
        tree_oid: trees[0].to_owned(),
        parents,
    })
}

fn verify_commit_facts(
    expected_tree: &str,
    expected_parent: &str,
    facts: &CommitFacts,
) -> Result<(), GitCommitObjectError> {
    if facts.tree_oid != expected_tree {
        return Err(GitCommitObjectError::CommittedTreeMismatch {
            expected: expected_tree.to_owned(),
            actual: facts.tree_oid.clone(),
        });
    }
    if facts.parents.as_slice() != [expected_parent] {
        return Err(GitCommitObjectError::ParentMismatch {
            expected: expected_parent.to_owned(),
            actual: facts.parents.clone(),
        });
    }
    Ok(())
}

fn parse_oid(output: &[u8]) -> Option<String> {
    std::str::from_utf8(output)
        .ok()
        .map(str::trim)
        .filter(|value| is_oid(value))
        .map(str::to_owned)
}

fn is_oid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

#[derive(Debug, Eq, PartialEq)]
pub enum GitCommitObjectError {
    RepositoryUnavailable {
        status: Option<i32>,
        stderr: String,
    },
    ReviewedBaseCommitMissing,
    ReviewedTreeMissing,
    CommitCreationFailed {
        status: Option<i32>,
        stderr: String,
    },
    MalformedCommitCreationOutput,
    CommitReadFailed {
        status: Option<i32>,
        stderr: String,
    },
    MalformedCommit,
    CommittedTreeMismatch {
        expected: String,
        actual: String,
    },
    ParentMismatch {
        expected: String,
        actual: Vec<String>,
    },
}

impl fmt::Display for GitCommitObjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryUnavailable { .. } => {
                formatter.write_str("Git repository is unavailable or invalid")
            }
            Self::ReviewedBaseCommitMissing => {
                formatter.write_str("reviewed base commit object is missing")
            }
            Self::ReviewedTreeMissing => formatter.write_str("reviewed tree object is missing"),
            Self::CommitCreationFailed { .. } => {
                formatter.write_str("could not create Git commit object")
            }
            Self::MalformedCommitCreationOutput => {
                formatter.write_str("Git returned a malformed commit object ID")
            }
            Self::CommitReadFailed { .. } => {
                formatter.write_str("could not read created Git commit object")
            }
            Self::MalformedCommit => formatter.write_str("created Git commit object is malformed"),
            Self::CommittedTreeMismatch { .. } => {
                formatter.write_str("created commit tree does not equal the reviewed Git tree")
            }
            Self::ParentMismatch { .. } => formatter
                .write_str("created commit does not have exactly the reviewed base as parent"),
        }
    }
}

impl Error for GitCommitObjectError {}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use crate::{
        domain::{ContentPath, Snapshot, SnapshotFile, SourceId},
        publisher::{
            GitProjectionMaterializer, GitRepositoryIdentity, PublicationTarget, PublishRun,
            PublishRunId, PublishRunPublication,
        },
        storage::LocalContentStore,
        workflow::{FinalPublicationSet, PublicProjection},
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
                "mineral-publisher-git-commit-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            let repository = Self {
                store: LocalContentStore::new(path.join("cas")),
                path,
            };
            repository.git(["init", "--quiet"]);
            repository.git(["config", "user.name", "Unrelated User"]);
            repository.git(["config", "user.email", "unrelated@example.invalid"]);
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

        fn object_count(&self) -> usize {
            fs::read_dir(self.path.join(".git/objects"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.len() == 2 && name != "info" && name != "pack")
                })
                .map(|entry| fs::read_dir(entry.path()).unwrap().count())
                .sum()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn metadata(message: &str, seconds: u64) -> GitCommitMetadata {
        GitCommitMetadata::with_timestamp(
            "Mineral Publisher",
            "publisher@mineral.invalid",
            message,
            UNIX_EPOCH + Duration::from_secs(seconds),
        )
        .unwrap()
    }

    fn projection(repository: &TestRepository, entries: &[(&str, &[u8])]) -> PublicProjection {
        let files = entries
            .iter()
            .map(|(path, bytes)| {
                let sha256 = repository.store.store(bytes).unwrap();
                SnapshotFile::new(
                    ContentPath::new(*path).unwrap(),
                    bytes.len() as u64,
                    sha256,
                    None,
                )
            })
            .collect::<Vec<_>>();
        let snapshot = Snapshot::new(
            SnapshotId::new(7).unwrap(),
            UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files,
        )
        .unwrap();
        let set = FinalPublicationSet::from_parts_for_test(
            snapshot.id(),
            entries
                .iter()
                .map(|(path, _)| ContentPath::new(*path).unwrap())
                .collect(),
            vec![],
        );
        PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap()
    }

    fn reviewed(
        repository: &TestRepository,
        base: &str,
        entries: &[(&str, &[u8])],
    ) -> ReviewedGitTree {
        GitProjectionMaterializer::materialize(
            &repository.path,
            base,
            &projection(repository, entries),
            &repository.store,
        )
        .unwrap()
    }

    fn created(result: GitCommitResult) -> ReviewedGitCommit {
        match result {
            GitCommitResult::Created(commit) => commit,
            GitCommitResult::Noop(_) => panic!("expected a created commit"),
        }
    }

    #[test]
    fn creates_one_commit_from_the_exact_reviewed_tree_and_explicit_parent() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"outside");
        repository.write("content/old.md", b"old");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("new.md", b"new")]);

        let result = GitCommitObjectCreator::create(
            &repository.path,
            &reviewed,
            &metadata("Publish Mineral content", 10),
        )
        .unwrap();
        let publish_run = PublishRun::from_git_commit_result(
            PublishRunId::new(1).unwrap(),
            GitRepositoryIdentity::new(&repository.path).unwrap(),
            PublicationTarget::new("origin", "refs/heads/main").unwrap(),
            &result,
            UNIX_EPOCH + Duration::from_secs(20),
        )
        .unwrap();
        let commit = created(result);

        assert_eq!(commit.tree_oid(), reviewed.tree_oid());
        assert_eq!(commit.base_commit(), base);
        assert_eq!(commit.projection_sha256(), reviewed.projection_sha256());
        assert_eq!(commit.snapshot_id(), reviewed.snapshot_id());
        assert_eq!(commit.managed_root(), reviewed.managed_root());
        assert_eq!(publish_run.snapshot_id(), reviewed.snapshot_id());
        assert_eq!(
            publish_run.projection_sha256(),
            reviewed.projection_sha256()
        );
        assert_eq!(publish_run.managed_root(), reviewed.managed_root());
        assert_eq!(publish_run.base_commit(), base);
        assert_eq!(publish_run.reviewed_tree(), reviewed.tree_oid());
        assert!(matches!(
            publish_run.publication(),
            PublishRunPublication::CommitReady { commit_oid } if commit_oid == commit.commit_oid()
        ));
        assert_eq!(repository.head(), base);
        assert_eq!(
            String::from_utf8(
                repository.git(["rev-parse", &format!("{}^{{tree}}", commit.commit_oid())])
            )
            .unwrap()
            .trim(),
            reviewed.tree_oid()
        );
        assert_eq!(
            String::from_utf8(repository.git(["rev-parse", &format!("{}^", commit.commit_oid())]))
                .unwrap()
                .trim(),
            base
        );
        let identity = String::from_utf8(repository.git([
            "show",
            "-s",
            "--format=%an%n%ae%n%cn%n%ce%n%B",
            commit.commit_oid(),
        ]))
        .unwrap();
        assert_eq!(
            identity.lines().take(5).collect::<Vec<_>>(),
            [
                "Mineral Publisher",
                "publisher@mineral.invalid",
                "Mineral Publisher",
                "publisher@mineral.invalid",
                "Publish Mineral content",
            ]
        );
    }

    #[test]
    fn noop_creates_no_commit_object_or_ref_change() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("content/a.md", b"A");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"A")]);
        let objects_before = repository.object_count();

        let result =
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("noop", 10))
                .unwrap();

        let GitCommitResult::Noop(noop) = result else {
            panic!("expected noop");
        };
        assert_eq!(noop.base_commit(), base);
        assert_eq!(noop.tree_oid(), reviewed.tree_oid());
        assert_eq!(repository.object_count(), objects_before);
        assert_eq!(repository.head(), base);
    }

    #[test]
    fn moved_head_does_not_change_the_explicit_reviewed_parent() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"A")]);
        repository.write("README.md", b"advanced");
        let advanced = repository.commit_all("advance");

        let commit = created(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("publish", 10))
                .unwrap(),
        );

        assert_eq!(commit.base_commit(), base);
        assert_eq!(repository.head(), advanced);
    }

    #[test]
    fn dirty_worktree_staged_index_head_and_branch_ref_are_unchanged() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"published")]);
        repository.write("README.md", b"staged");
        repository.git(["add", "README.md"]);
        repository.write("README.md", b"working");
        let index_before = fs::read(repository.path.join(".git/index")).unwrap();
        let head_before = repository.head();
        let branch_before = repository.git(["symbolic-ref", "HEAD"]);

        let commit = created(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("publish", 10))
                .unwrap(),
        );

        assert_eq!(
            fs::read(repository.path.join("README.md")).unwrap(),
            b"working"
        );
        assert_eq!(
            fs::read(repository.path.join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(repository.head(), head_before);
        assert_eq!(repository.git(["symbolic-ref", "HEAD"]), branch_before);
        assert_eq!(commit.tree_oid(), reviewed.tree_oid());
    }

    #[test]
    fn fixed_metadata_is_deterministic_and_metadata_changes_commit_identity() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"A")]);

        let first = created(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("publish", 10))
                .unwrap(),
        );
        let second = created(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("publish", 10))
                .unwrap(),
        );
        let changed_message = created(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("different", 10))
                .unwrap(),
        );
        let changed_time = created(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("publish", 11))
                .unwrap(),
        );

        assert_eq!(first.commit_oid(), second.commit_oid());
        assert_ne!(first.commit_oid(), changed_message.commit_oid());
        assert_ne!(first.commit_oid(), changed_time.commit_oid());
    }

    #[test]
    fn invalid_repository_and_missing_reviewed_objects_are_typed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"A")]);
        let invalid = repository.path.join("not-a-repository");
        fs::create_dir(&invalid).unwrap();
        assert!(matches!(
            GitCommitObjectCreator::create(&invalid, &reviewed, &metadata("publish", 10)),
            Err(GitCommitObjectError::RepositoryUnavailable { .. })
        ));

        let Some(other) = TestRepository::new() else {
            return;
        };
        other.write("README.md", b"other");
        other.commit_all("other");
        assert_eq!(
            GitCommitObjectCreator::create(&other.path, &reviewed, &metadata("publish", 10)),
            Err(GitCommitObjectError::ReviewedBaseCommitMissing)
        );

        let tree_object = repository
            .path
            .join(".git/objects")
            .join(&reviewed.tree_oid()[..2])
            .join(&reviewed.tree_oid()[2..]);
        fs::remove_file(tree_object).unwrap();
        assert_eq!(
            GitCommitObjectCreator::create(&repository.path, &reviewed, &metadata("publish", 10)),
            Err(GitCommitObjectError::ReviewedTreeMissing)
        );
    }

    #[test]
    fn metadata_validation_is_independent_of_user_git_identity() {
        assert_eq!(
            GitCommitMetadata::new("", "publisher@mineral.invalid", "publish"),
            Err(GitCommitMetadataError::InvalidAuthorName)
        );
        assert_eq!(
            GitCommitMetadata::new("Mineral Publisher", "bad\nemail", "publish"),
            Err(GitCommitMetadataError::InvalidAuthorEmail)
        );
        assert_eq!(
            GitCommitMetadata::new("Mineral Publisher", "publisher@mineral.invalid", ""),
            Err(GitCommitMetadataError::InvalidMessage)
        );
        assert_eq!(
            GitCommitMetadata::with_timestamp(
                "Mineral Publisher",
                "publisher@mineral.invalid",
                "publish",
                UNIX_EPOCH - Duration::from_secs(1),
            ),
            Err(GitCommitMetadataError::InvalidTimestamp)
        );
    }

    #[test]
    fn verification_fails_closed_on_tree_or_parent_mismatch() {
        let expected_tree = "1111111111111111111111111111111111111111";
        let expected_parent = "2222222222222222222222222222222222222222";
        assert!(matches!(
            verify_commit_facts(
                expected_tree,
                expected_parent,
                &CommitFacts {
                    tree_oid: "3333333333333333333333333333333333333333".to_owned(),
                    parents: vec![expected_parent.to_owned()],
                },
            ),
            Err(GitCommitObjectError::CommittedTreeMismatch { .. })
        ));
        assert!(matches!(
            verify_commit_facts(
                expected_tree,
                expected_parent,
                &CommitFacts {
                    tree_oid: expected_tree.to_owned(),
                    parents: vec![expected_parent.to_owned(), expected_parent.to_owned()],
                },
            ),
            Err(GitCommitObjectError::ParentMismatch { .. })
        ));
    }
}
