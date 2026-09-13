use std::{error::Error, fmt};

use crate::{
    domain::{Sha256, SnapshotId, TimestampMillis},
    workflow::{CurrentTargetState, ManagedRoot},
};

/// Explicit remote and fully-qualified destination ref. This contains no remote
/// URL and no credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRefTarget {
    remote_name: String,
    destination_ref: String,
}

impl GitRefTarget {
    pub fn new(
        remote_name: impl Into<String>,
        destination_ref: impl Into<String>,
    ) -> Result<Self, GitRefTargetError> {
        let remote_name = remote_name.into();
        let destination_ref = destination_ref.into();
        if remote_name.trim().is_empty() || remote_name.contains(['\0', '\n', '\r']) {
            return Err(GitRefTargetError::InvalidRemoteName);
        }
        if !is_safe_destination_ref(&destination_ref) {
            return Err(GitRefTargetError::InvalidDestinationRef);
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
pub enum GitRefTargetError {
    InvalidRemoteName,
    InvalidDestinationRef,
}

impl fmt::Display for GitRefTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRemoteName => formatter.write_str("publication remote name is invalid"),
            Self::InvalidDestinationRef => {
                formatter.write_str("publication destination ref is invalid")
            }
        }
    }
}

impl Error for GitRefTargetError {}

/// A Git commit object identity.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GitCommitOid(String);

impl GitCommitOid {
    pub fn new(value: impl Into<String>) -> Result<Self, GitCommitOidError> {
        let value = value.into();
        if !is_object_id(&value) {
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

/// A Git tree object identity.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GitTreeOid(String);

impl GitTreeOid {
    pub fn new(value: impl Into<String>) -> Result<Self, GitTreeOidError> {
        let value = value.into();
        if !is_object_id(&value) {
            return Err(GitTreeOidError::Invalid);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitTreeOidError {
    Invalid,
}

impl fmt::Display for GitTreeOidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Git tree object ID is invalid")
    }
}

impl Error for GitTreeOidError {}

fn is_object_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Every input that determines the identity of one Git commit object.
///
/// A commit OID depends on its tree, its parent, the author and committer
/// identities, both timestamps, and the message. All of them are frozen here, at
/// publication-intent time: an adapter creates the object from this value and is
/// never allowed to read a clock or invent an identity of its own. Without that
/// rule, retrying one reviewed tree could produce two different commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitSpec {
    parent: GitCommitOid,
    tree: GitTreeOid,
    author_name: String,
    author_email: String,
    author_time: TimestampMillis,
    committer_name: String,
    committer_email: String,
    committer_time: TimestampMillis,
    message: String,
}

impl GitCommitSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parent: GitCommitOid,
        tree: GitTreeOid,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
        author_time: TimestampMillis,
        committer_name: impl Into<String>,
        committer_email: impl Into<String>,
        committer_time: TimestampMillis,
        message: impl Into<String>,
    ) -> Result<Self, GitCommitSpecError> {
        let author_name = author_name.into();
        let author_email = author_email.into();
        let committer_name = committer_name.into();
        let committer_email = committer_email.into();
        let message = message.into();
        if !is_valid_identity(&author_name) {
            return Err(GitCommitSpecError::InvalidAuthorName);
        }
        if !is_valid_identity(&author_email) {
            return Err(GitCommitSpecError::InvalidAuthorEmail);
        }
        if !is_valid_identity(&committer_name) {
            return Err(GitCommitSpecError::InvalidCommitterName);
        }
        if !is_valid_identity(&committer_email) {
            return Err(GitCommitSpecError::InvalidCommitterEmail);
        }
        if message.is_empty() || message.contains('\0') {
            return Err(GitCommitSpecError::InvalidMessage);
        }
        Ok(Self {
            parent,
            tree,
            author_name,
            author_email,
            author_time,
            committer_name,
            committer_email,
            committer_time,
            message,
        })
    }

    pub fn parent(&self) -> &GitCommitOid {
        &self.parent
    }

    pub fn tree(&self) -> &GitTreeOid {
        &self.tree
    }

    pub fn author_name(&self) -> &str {
        &self.author_name
    }

    pub fn author_email(&self) -> &str {
        &self.author_email
    }

    pub fn author_time(&self) -> TimestampMillis {
        self.author_time
    }

    pub fn committer_name(&self) -> &str {
        &self.committer_name
    }

    pub fn committer_email(&self) -> &str {
        &self.committer_email
    }

    pub fn committer_time(&self) -> TimestampMillis {
        self.committer_time
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

fn is_valid_identity(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains(['\0', '\n', '\r', '<', '>'])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitCommitSpecError {
    InvalidAuthorName,
    InvalidAuthorEmail,
    InvalidCommitterName,
    InvalidCommitterEmail,
    InvalidMessage,
}

impl fmt::Display for GitCommitSpecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAuthorName => formatter.write_str("commit author name is invalid"),
            Self::InvalidAuthorEmail => formatter.write_str("commit author email is invalid"),
            Self::InvalidCommitterName => formatter.write_str("commit committer name is invalid"),
            Self::InvalidCommitterEmail => formatter.write_str("commit committer email is invalid"),
            Self::InvalidMessage => formatter.write_str("commit message is invalid"),
        }
    }
}

impl Error for GitCommitSpecError {}

/// The current state of a destination ref, as observed on the remote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteRefState {
    Present { commit_oid: GitCommitOid },
    Missing,
}

/// The facts one local commit object carries.
///
/// A runtime reports only what it read: which commit this is, which parent it
/// builds on, and which tree it names. Whether those facts are the ones a
/// publication intent is allowed to trust is the engine's decision, never the
/// runtime's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitFacts {
    commit: GitCommitOid,
    parent: GitCommitOid,
    tree: GitTreeOid,
}

impl GitCommitFacts {
    /// Runtime-side constructor: the runtime reads a real object and reports it.
    pub fn from_parts(commit: GitCommitOid, parent: GitCommitOid, tree: GitTreeOid) -> Self {
        Self {
            commit,
            parent,
            tree,
        }
    }

    pub fn commit(&self) -> &GitCommitOid {
        &self.commit
    }

    pub fn parent(&self) -> &GitCommitOid {
        &self.parent
    }

    pub fn tree(&self) -> &GitTreeOid {
        &self.tree
    }
}

/// What a runtime found in its own object database for one commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalCommitState {
    Missing,
    Present(GitCommitFacts),
}

/// Exactly one compare-and-swap update of a destination ref.
///
/// The expected previous value is part of the request rather than implied by it,
/// so no implementation can satisfy this port with a "push if fast-forward"
/// approximation and still look correct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefUpdate {
    target: GitRefTarget,
    expected_old: GitCommitOid,
    new_commit: GitCommitOid,
}

impl RefUpdate {
    pub fn new(target: GitRefTarget, expected_old: GitCommitOid, new_commit: GitCommitOid) -> Self {
        Self {
            target,
            expected_old,
            new_commit,
        }
    }

    pub fn target(&self) -> &GitRefTarget {
        &self.target
    }

    pub fn expected_old(&self) -> &GitCommitOid {
        &self.expected_old
    }

    pub fn new_commit(&self) -> &GitCommitOid {
        &self.new_commit
    }
}

/// The current state of the managed subtree, pinned to the commit it was read from.
///
/// The engine compares the resolved base commit with the base it observed on the
/// remote before it trusts any of this state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCurrentTarget {
    base_commit: String,
    state: CurrentTargetState,
}

impl GitCurrentTarget {
    /// Runtime-side constructor: the runtime reads a real commit and reports the
    /// subtree it holds.
    pub fn from_parts(base_commit: impl Into<String>, state: CurrentTargetState) -> Self {
        Self {
            base_commit: base_commit.into(),
            state,
        }
    }

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

/// An immutable fact identifying the exact Git tree reviewed for one projection.
///
/// Runtime-side constructor: the engine defines this fact and the invariants that
/// depend on it (`reviewed tree == committed tree`), and the runtime that
/// materializes the tree fills it in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewedGitTree {
    base_commit: String,
    base_tree_oid: String,
    projection_sha256: Sha256,
    snapshot_id: SnapshotId,
    tree_oid: String,
    managed_root: ManagedRoot,
}

impl ReviewedGitTree {
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        base_commit: impl Into<String>,
        base_tree_oid: impl Into<String>,
        projection_sha256: Sha256,
        snapshot_id: SnapshotId,
        tree_oid: impl Into<String>,
        managed_root: ManagedRoot,
    ) -> Self {
        Self {
            base_commit: base_commit.into(),
            base_tree_oid: base_tree_oid.into(),
            projection_sha256,
            snapshot_id,
            tree_oid: tree_oid.into(),
            managed_root,
        }
    }

    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub fn base_tree_oid(&self) -> &str {
        &self.base_tree_oid
    }

    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn tree_oid(&self) -> &str {
        &self.tree_oid
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }

    pub fn is_noop(&self) -> bool {
        self.tree_oid == self.base_tree_oid
    }
}

/// The domain result of one compare-and-swap attempt.
///
/// Transport, authentication, and command failures are adapter errors, not
/// values here, and process exit codes never cross this boundary. `Rejected`
/// deliberately carries no remote state: the authoritative fact about what the
/// remote holds is the observation the engine takes immediately afterwards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CasOutcome {
    Updated,
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn tree(value: char) -> GitTreeOid {
        GitTreeOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn spec() -> GitCommitSpec {
        GitCommitSpec::new(
            commit('a'),
            tree('b'),
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(1_000),
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(1_000),
            "Publish Mineral content",
        )
        .unwrap()
    }

    #[test]
    fn target_requires_a_fully_qualified_safe_ref() {
        assert!(GitRefTarget::new("origin", "refs/heads/main").is_ok());
        for value in [
            "main",
            "refs/heads/../main",
            "refs/heads/a b",
            "refs/heads/",
        ] {
            assert!(
                GitRefTarget::new("origin", value).is_err(),
                "accepted {value:?}"
            );
        }
        for value in ["", " ", "origin\nmain"] {
            assert!(
                GitRefTarget::new(value, "refs/heads/main").is_err(),
                "accepted {value:?}"
            );
        }
    }

    #[test]
    fn object_ids_must_be_non_empty_hexadecimal() {
        assert!(GitCommitOid::new("a".repeat(40)).is_ok());
        assert!(GitTreeOid::new("b".repeat(64)).is_ok());
        for value in ["", " ", "xyz", "a\nb"] {
            assert!(GitCommitOid::new(value).is_err(), "accepted {value:?}");
            assert!(GitTreeOid::new(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn commit_spec_freezes_every_identity_input() {
        let spec = spec();

        assert_eq!(spec.parent(), &commit('a'));
        assert_eq!(spec.tree(), &tree('b'));
        assert_eq!(spec.author_time().as_unix_millis(), 1_000);
        assert_eq!(spec.committer_email(), "publisher@example.invalid");
        assert_eq!(spec.message(), "Publish Mineral content");
    }

    #[test]
    fn commit_spec_rejects_unusable_identity_fields() {
        for name in ["", "   ", "a<b", "a\nb"] {
            assert!(
                GitCommitSpec::new(
                    commit('a'),
                    tree('b'),
                    name,
                    "publisher@example.invalid",
                    TimestampMillis::UNIX_EPOCH,
                    "Mineral Publisher",
                    "publisher@example.invalid",
                    TimestampMillis::UNIX_EPOCH,
                    "message",
                )
                .is_err(),
                "accepted author name {name:?}"
            );
        }
        for message in ["", "line\0terminator"] {
            assert!(
                GitCommitSpec::new(
                    commit('a'),
                    tree('b'),
                    "Mineral Publisher",
                    "publisher@example.invalid",
                    TimestampMillis::UNIX_EPOCH,
                    "Mineral Publisher",
                    "publisher@example.invalid",
                    TimestampMillis::UNIX_EPOCH,
                    message,
                )
                .is_err(),
                "accepted message {message:?}"
            );
        }
    }

    #[test]
    fn ref_update_states_the_expected_previous_value() {
        let update = RefUpdate::new(
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            commit('a'),
            commit('b'),
        );

        assert_eq!(update.target().destination_ref(), "refs/heads/main");
        assert_eq!(update.expected_old(), &commit('a'));
        assert_eq!(update.new_commit(), &commit('b'));
    }

    #[test]
    fn local_commit_facts_report_facts_not_verdicts() {
        let facts = GitCommitFacts::from_parts(commit('a'), commit('b'), tree('c'));

        assert_eq!(facts.commit(), &commit('a'));
        assert_eq!(facts.parent(), &commit('b'));
        assert_eq!(facts.tree(), &tree('c'));
        assert_eq!(LocalCommitState::Missing, LocalCommitState::Missing);
    }
}
