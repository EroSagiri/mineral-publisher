use std::{
    error::Error,
    fmt, fs, io,
    path::Path,
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    domain::TimestampMillis,
    publication::git::{GitCommitFacts, GitCommitOid, GitCommitSpec, GitTreeOid, LocalCommitState},
};

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

#[derive(Clone, Copy, Debug, Default)]
pub struct GitCommitObjectCreator;

impl GitCommitObjectCreator {
    /// Reports the facts of one commit in the local object database, or that it is
    /// absent.
    ///
    /// This is a local repository question and it answers with facts only: which
    /// commit this is, which parent it builds on, and which tree it names. Whether
    /// those facts are the ones a publication intent may trust is decided by the
    /// engine, never here.
    pub fn inspect(
        repository: impl AsRef<Path>,
        commit: &GitCommitOid,
    ) -> Result<LocalCommitState, GitCommitObjectError> {
        let repository = repository.as_ref();
        verify_repository(repository)?;

        let expression = format!("{}^{{commit}}", commit.as_str());
        let exists = Command::new("git")
            .current_dir(repository)
            .args(["cat-file", "-e", &expression])
            .output()
            .map_err(|source| GitCommitObjectError::RepositoryUnavailable {
                status: None,
                stderr: source.to_string(),
            })?;
        if !exists.status.success() {
            return Ok(LocalCommitState::Missing);
        }

        let facts = read_commit(repository, commit.as_str())?;
        let [parent] = facts.parents.as_slice() else {
            return Err(GitCommitObjectError::UnexpectedParentCount(
                facts.parents.len(),
            ));
        };
        Ok(LocalCommitState::Present(GitCommitFacts::from_parts(
            commit.clone(),
            GitCommitOid::new(parent).map_err(|_| GitCommitObjectError::MalformedCommit)?,
            GitTreeOid::new(facts.tree_oid).map_err(|_| GitCommitObjectError::MalformedCommit)?,
        )))
    }

    /// Creates one commit object from a frozen specification alone.
    ///
    /// The parent and the tree come from the specification, so the created object
    /// is verified against exactly the identities the specification froze — this is
    /// the entry point a runtime uses when it never held a `ReviewedGitTree`.
    pub fn create_from_spec(
        repository: impl AsRef<Path>,
        spec: &GitCommitSpec,
    ) -> Result<GitCommitOid, GitCommitObjectError> {
        let repository = repository.as_ref();
        verify_repository(repository)?;
        verify_object(
            repository,
            spec.parent().as_str(),
            "commit",
            GitCommitObjectError::ReviewedBaseCommitMissing,
        )?;
        verify_object(
            repository,
            spec.tree().as_str(),
            "tree",
            GitCommitObjectError::ReviewedTreeMissing,
        )?;

        let commit_oid = create_commit(repository, spec)?;
        let facts = read_commit(repository, &commit_oid)?;
        verify_commit_facts(spec.tree().as_str(), spec.parent().as_str(), &facts)?;

        GitCommitOid::new(commit_oid)
            .map_err(|_| GitCommitObjectError::MalformedCommitCreationOutput)
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

fn create_commit(repository: &Path, spec: &GitCommitSpec) -> Result<String, GitCommitObjectError> {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args(["-c", "commit.gpgSign=false", "commit-tree"])
        .arg(spec.tree().as_str())
        .args(["-p", spec.parent().as_str()])
        .env("GIT_AUTHOR_NAME", spec.author_name())
        .env("GIT_AUTHOR_EMAIL", spec.author_email())
        .env("GIT_AUTHOR_DATE", git_date(spec.author_time()))
        .env("GIT_COMMITTER_NAME", spec.committer_name())
        .env("GIT_COMMITTER_EMAIL", spec.committer_email())
        .env("GIT_COMMITTER_DATE", git_date(spec.committer_time()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        command
            .spawn()
            .map_err(|source| GitCommitObjectError::CommitCreationFailed {
                status: None,
                stderr: source.to_string(),
            })?;
    io::Write::write_all(
        child.stdin.as_mut().expect("piped stdin is available"),
        spec.message().as_bytes(),
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

/// Git accepts `<seconds> <timezone>` or `@<seconds> <timezone>` for the author
/// and committer dates. The frozen timestamp is a fixed instant, so it is always
/// written in UTC rather than in whatever the machine's local offset happens to be.
fn git_date(time: TimestampMillis) -> String {
    format!("@{} +0000", time.as_unix_seconds())
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
    CommitSpecMismatch,
    /// A local commit does not have the exactly-one-parent shape every publication
    /// commit has, so it cannot be the object this engine created.
    UnexpectedParentCount(usize),
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
            Self::CommitSpecMismatch => formatter
                .write_str("commit specification disagrees with the reviewed Git tree or base"),
            Self::UnexpectedParentCount(count) => write!(
                formatter,
                "local commit has {count} parents and not exactly one"
            ),
        }
    }
}

impl Error for GitCommitObjectError {}

#[cfg(test)]
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
        domain::{ContentPath, Snapshot, SnapshotFile, SnapshotId, SourceId},
        publisher::{
            GitProjectionMaterializer, GitRefTarget, GitRepositoryIdentity, PublishRun,
            PublishRunId, PublishTargetId, ReviewedGitTree,
        },
        storage::LocalContentStore,
        workflow::{FinalPublicationSet, ManagedRoot, PublicProjection},
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
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
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

    /// The frozen identity every publication commit is created from. Nothing here
    /// reads a clock: both times are the caller's frozen attempt instant.
    fn spec(reviewed: &ReviewedGitTree, message: &str, seconds: u64) -> GitCommitSpec {
        let time = TimestampMillis::from_unix_millis(seconds * 1_000);
        GitCommitSpec::new(
            GitCommitOid::new(reviewed.base_commit()).unwrap(),
            GitTreeOid::new(reviewed.tree_oid()).unwrap(),
            "Mineral Publisher",
            "publisher@mineral.invalid",
            time,
            "Mineral Publisher",
            "publisher@mineral.invalid",
            time,
            message,
        )
        .unwrap()
    }

    fn create(repository: &TestRepository, spec: &GitCommitSpec) -> GitCommitOid {
        GitCommitObjectCreator::create_from_spec(&repository.path, spec).unwrap()
    }

    fn inspect(repository: &TestRepository, commit: &GitCommitOid) -> LocalCommitState {
        GitCommitObjectCreator::inspect(&repository.path, commit).unwrap()
    }

    fn facts(repository: &TestRepository, commit: &GitCommitOid) -> GitCommitFacts {
        match inspect(repository, commit) {
            LocalCommitState::Present(facts) => facts,
            LocalCommitState::Missing => panic!("expected a present commit"),
        }
    }

    #[test]
    fn creates_one_commit_from_the_exact_reviewed_tree_and_states_its_facts() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"outside");
        repository.write("content/old.md", b"old");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("new.md", b"new")]);
        let frozen = spec(&reviewed, "Publish Mineral content", 10);

        let commit = create(&repository, &frozen);

        // The created object is the one the specification froze, and the local
        // facts agree with the reviewed tree about parent and tree.
        assert_eq!(
            inspect(&repository, &commit),
            LocalCommitState::Present(GitCommitFacts::from_parts(
                commit.clone(),
                GitCommitOid::new(reviewed.base_commit()).unwrap(),
                GitTreeOid::new(reviewed.tree_oid()).unwrap(),
            ))
        );
        assert_eq!(repository.head(), base);
        assert_eq!(
            String::from_utf8(
                repository.git(["rev-parse", &format!("{}^{{tree}}", commit.as_str())])
            )
            .unwrap()
            .trim(),
            reviewed.tree_oid()
        );
        assert_eq!(
            String::from_utf8(repository.git(["rev-parse", &format!("{}^", commit.as_str())]))
                .unwrap()
                .trim(),
            base
        );
        let identity = String::from_utf8(repository.git([
            "show",
            "-s",
            "--format=%an%n%ae%n%cn%n%ce%n%B",
            commit.as_str(),
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

        // The intent freezes the very specification that produced the commit, and
        // the run's cross-checks accept it.
        let publish_run = PublishRun::from_reviewed_tree(
            PublishRunId::new(1).unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            GitRepositoryIdentity::new(&repository.path)
                .unwrap()
                .locator()
                .clone(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            &reviewed,
            Some(commit.clone()),
            Some(frozen.clone()),
            TimestampMillis::from_unix_millis(10_000),
        )
        .unwrap();
        assert_eq!(publish_run.desired_commit(), Some(&commit));
        assert_eq!(publish_run.commit_spec(), Some(&frozen));
        assert_eq!(publish_run.reviewed_tree(), reviewed.tree_oid());
    }

    #[test]
    fn inspect_reports_a_missing_commit_without_inventing_one() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        repository.commit_all("base");

        assert_eq!(
            inspect(&repository, &GitCommitOid::new("d".repeat(40)).unwrap()),
            LocalCommitState::Missing
        );
    }

    #[test]
    fn inspect_fails_closed_on_a_commit_without_exactly_one_parent() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        repository.commit_all("base");
        let tree = String::from_utf8(repository.git(["rev-parse", "HEAD^{tree}"]))
            .unwrap()
            .trim()
            .to_owned();
        // A root commit is a perfectly valid Git object, but it cannot be the
        // commit this engine creates, so it must not be reported as usable facts.
        let root = String::from_utf8(repository.git(["commit-tree", tree.as_str()]))
            .unwrap()
            .trim()
            .to_owned();

        assert_eq!(
            GitCommitObjectCreator::inspect(&repository.path, &GitCommitOid::new(root).unwrap()),
            Err(GitCommitObjectError::UnexpectedParentCount(0))
        );
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

        let commit = create(&repository, &spec(&reviewed, "publish", 10));
        let facts = facts(&repository, &commit);

        assert_eq!(facts.parent().as_str(), base);
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

        let commit = create(&repository, &spec(&reviewed, "publish", 10));

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
        assert_eq!(
            facts(&repository, &commit).tree().as_str(),
            reviewed.tree_oid()
        );
    }

    #[test]
    fn one_frozen_specification_always_produces_the_same_commit_identity() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"A")]);

        let first = create(&repository, &spec(&reviewed, "publish", 10));
        let second = create(&repository, &spec(&reviewed, "publish", 10));
        let changed_message = create(&repository, &spec(&reviewed, "different", 10));
        let changed_time = create(&repository, &spec(&reviewed, "publish", 11));

        assert_eq!(first, second);
        assert_ne!(first, changed_message);
        assert_ne!(first, changed_time);
    }

    #[test]
    fn invalid_repository_and_missing_reviewed_objects_are_typed() {
        let Some(repository) = TestRepository::new() else {
            return;
        };
        repository.write("README.md", b"base");
        let base = repository.commit_all("base");
        let reviewed = reviewed(&repository, &base, &[("a.md", b"A")]);
        let frozen = spec(&reviewed, "publish", 10);
        let invalid = repository.path.join("not-a-repository");
        fs::create_dir(&invalid).unwrap();
        assert!(matches!(
            GitCommitObjectCreator::create_from_spec(&invalid, &frozen),
            Err(GitCommitObjectError::RepositoryUnavailable { .. })
        ));

        let Some(other) = TestRepository::new() else {
            return;
        };
        other.write("README.md", b"other");
        other.commit_all("other");
        assert_eq!(
            GitCommitObjectCreator::create_from_spec(&other.path, &frozen),
            Err(GitCommitObjectError::ReviewedBaseCommitMissing)
        );

        let tree_object = repository
            .path
            .join(".git/objects")
            .join(&reviewed.tree_oid()[..2])
            .join(&reviewed.tree_oid()[2..]);
        fs::remove_file(tree_object).unwrap();
        assert_eq!(
            GitCommitObjectCreator::create_from_spec(&repository.path, &frozen),
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
