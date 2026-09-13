use std::{error::Error, fmt};

use crate::{
    domain::{Sha256, SnapshotId, TimestampMillis},
    publication::git::{GitCommitOid, GitCommitSpec, GitRefTarget, ReviewedGitTree},
    workflow::ManagedRoot,
};

use super::{PublishTargetId, RepositoryLocator};

/// Stable identity for one explicit publication attempt.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublishRunId(u64);

impl PublishRunId {
    pub fn new(value: u64) -> Result<Self, PublishRunIdError> {
        if value == 0 {
            return Err(PublishRunIdError::Zero);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishRunIdError {
    Zero,
}

impl fmt::Display for PublishRunIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("publish run ID must be positive")
    }
}

impl Error for PublishRunIdError {}

/// The immutable publication intent. A commit-ready intent is not evidence of remote success.
///
/// `desired_commit` is the only commit fact an intent carries: `None` means this
/// run expects the target ref to stay exactly where it is (a Noop), and
/// `Some(oid)` names the commit the target ref must hold when the run is
/// satisfied. Which commit that is was decided before the intent was persisted;
/// nothing here can read a clock, a configuration, or a runtime's commit result.
///
/// `commit_spec` is the frozen identity of that commit, and it exists so the
/// commit object can be recreated when a runtime no longer holds it. Together the
/// two fields have four combinations, and which of them a new intent may use is
/// deliberately narrower than what a persisted one may hold:
///
/// | `desired_commit` | `commit_spec` | a new intent | a persisted intent |
/// | --- | --- | --- | --- |
/// | `None` | `None` | Noop | Noop |
/// | `Some` | `Some` | reconstructible | reconstructible |
/// | `Some` | `None` | rejected | written before the spec was persisted: only a surviving local object can satisfy it |
/// | `None` | `Some` | rejected | rejected |
///
/// So `Some(oid)` with no specification only ever comes from
/// [`PublishRun::rehydrate`]; [`PublishRun::from_reviewed_tree`] cannot produce
/// it, and therefore no normal creation path can either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRun {
    id: PublishRunId,
    snapshot_id: SnapshotId,
    projection_sha256: Sha256,
    managed_root: ManagedRoot,
    target_id: PublishTargetId,
    repository: RepositoryLocator,
    target: GitRefTarget,
    base_commit: String,
    reviewed_tree: String,
    desired_commit: Option<GitCommitOid>,
    commit_spec: Option<GitCommitSpec>,
    created_at_unix_ms: u64,
}

/// The publication kind of an intent, as a derived compatibility view.
///
/// This is a projection of [`PublishRun::desired_commit`], never stored
/// separately, so it cannot disagree with the fact it describes: `None` reads as
/// [`PublishRunPublication::Noop`] and `Some(oid)` reads as
/// [`PublishRunPublication::CommitReady`] for exactly that commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishRunPublication {
    Noop,
    CommitReady { commit_oid: GitCommitOid },
}

impl PublishRun {
    /// Builds an intent from the immutable fact of what was reviewed.
    ///
    /// Every reviewed input (`snapshot_id`, `projection_sha256`, `managed_root`,
    /// `base_commit`, `reviewed_tree`) is taken from `reviewed`; the caller only
    /// supplies the desired target state, the specification of the commit that
    /// implements it, and the already-frozen attempt time.
    ///
    /// A new intent must be reconstructible: asking for a commit without freezing
    /// the identity that produces it is exactly the state this constructor
    /// refuses, because nothing afterwards could rebuild the object from durable
    /// data. The specification must also be the one the runtime actually used to
    /// create the commit — passing a second, freshly derived specification would
    /// let the persisted identity drift from the published one.
    #[allow(clippy::too_many_arguments)]
    pub fn from_reviewed_tree(
        id: PublishRunId,
        target_id: PublishTargetId,
        repository: RepositoryLocator,
        target: GitRefTarget,
        reviewed: &ReviewedGitTree,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
        created_at: TimestampMillis,
    ) -> Result<Self, PublishRunError> {
        if desired_commit.is_some() && commit_spec.is_none() {
            return Err(PublishRunError::DesiredCommitWithoutCommitSpec);
        }
        Self::from_parts(
            id,
            reviewed.snapshot_id(),
            reviewed.projection_sha256(),
            reviewed.managed_root().clone(),
            target_id,
            repository,
            target,
            reviewed.base_commit().to_owned(),
            reviewed.tree_oid().to_owned(),
            desired_commit,
            commit_spec,
            created_at.as_unix_millis(),
        )
    }

    /// Rebuilds an intent that was persisted earlier.
    ///
    /// This is the tolerant path, and the only place where the historical
    /// `Some(desired_commit)` / `None` shape is accepted: intents written before
    /// the specification was stored are still perfectly publishable while the
    /// runtime holds the commit object, and rejecting them would declare existing
    /// data corrupt. Anything else is subject to the same rules as a new intent,
    /// and the cross-field checks below apply here too, because a persisted row is
    /// exactly the input that can be damaged or hand-edited.
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate(
        id: PublishRunId,
        snapshot_id: SnapshotId,
        projection_sha256: Sha256,
        managed_root: ManagedRoot,
        target_id: PublishTargetId,
        repository: RepositoryLocator,
        target: GitRefTarget,
        base_commit: String,
        reviewed_tree: String,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
        created_at_unix_ms: u64,
    ) -> Result<Self, PublishRunError> {
        Self::from_parts(
            id,
            snapshot_id,
            projection_sha256,
            managed_root,
            target_id,
            repository,
            target,
            base_commit,
            reviewed_tree,
            desired_commit,
            commit_spec,
            created_at_unix_ms,
        )
    }

    /// Validates one intent, wherever it came from.
    ///
    /// A successful deserialization is not evidence that the facts agree, so the
    /// always-illegal state and the agreement between a frozen specification and
    /// the run it belongs to are checked here, at load/validate-intent time,
    /// rather than being left to a later stage that might not look. The rule that
    /// separates a new intent from a persisted one lives in the constructors.
    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        id: PublishRunId,
        snapshot_id: SnapshotId,
        projection_sha256: Sha256,
        managed_root: ManagedRoot,
        target_id: PublishTargetId,
        repository: RepositoryLocator,
        target: GitRefTarget,
        base_commit: String,
        reviewed_tree: String,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
        created_at_unix_ms: u64,
    ) -> Result<Self, PublishRunError> {
        if !is_oid(&base_commit) {
            return Err(PublishRunError::InvalidBaseCommit);
        }
        if !is_oid(&reviewed_tree) {
            return Err(PublishRunError::InvalidReviewedTree);
        }
        // Deliberately not `desired_commit.is_some() == commit_spec.is_some()`:
        // intents persisted before the specification was stored carry no spec and
        // are a legal, degraded shape rather than corruption.
        if let Some(spec) = &commit_spec {
            if desired_commit.is_none() {
                return Err(PublishRunError::CommitSpecWithoutDesiredCommit);
            }
            if spec.parent().as_str() != base_commit {
                return Err(PublishRunError::CommitSpecParentMismatch);
            }
            if spec.tree().as_str() != reviewed_tree {
                return Err(PublishRunError::CommitSpecTreeMismatch);
            }
            if spec.author_time().as_unix_millis() != created_at_unix_ms {
                return Err(PublishRunError::CommitSpecAuthorTimeMismatch);
            }
            if spec.committer_time().as_unix_millis() != created_at_unix_ms {
                return Err(PublishRunError::CommitSpecCommitterTimeMismatch);
            }
        }
        Ok(Self {
            id,
            snapshot_id,
            projection_sha256,
            managed_root,
            target_id,
            repository,
            target,
            base_commit,
            reviewed_tree,
            desired_commit,
            commit_spec,
            created_at_unix_ms,
        })
    }

    pub fn id(&self) -> PublishRunId {
        self.id
    }
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }
    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }
    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }
    /// Stable audit identity of the logical target, independent of the remote
    /// name and ref that currently happen to implement it.
    pub fn target_id(&self) -> &PublishTargetId {
        &self.target_id
    }
    /// Opaque locator that the runtime resolves; the engine never interprets it.
    pub fn repository(&self) -> &RepositoryLocator {
        &self.repository
    }
    pub fn target(&self) -> &GitRefTarget {
        &self.target
    }
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    pub fn reviewed_tree(&self) -> &str {
        &self.reviewed_tree
    }
    /// The commit this run wants the target ref to hold; `None` means Noop.
    pub fn desired_commit(&self) -> Option<&GitCommitOid> {
        self.desired_commit.as_ref()
    }
    /// The frozen identity of [`Self::desired_commit`], when this run carries
    /// one. `None` alongside a desired commit is the historical shape: the
    /// runtime must still hold the commit object itself.
    pub fn commit_spec(&self) -> Option<&GitCommitSpec> {
        self.commit_spec.as_ref()
    }
    /// The same fact as [`Self::desired_commit`], as a publication kind.
    pub fn publication(&self) -> PublishRunPublication {
        match &self.desired_commit {
            None => PublishRunPublication::Noop,
            Some(commit_oid) => PublishRunPublication::CommitReady {
                commit_oid: commit_oid.clone(),
            },
        }
    }
    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }
}

fn is_oid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Eq, PartialEq)]
pub enum PublishRunError {
    InvalidBaseCommit,
    InvalidReviewedTree,
    /// A frozen commit specification without a desired commit can never be
    /// satisfied: there is nothing the target ref is supposed to point at.
    CommitSpecWithoutDesiredCommit,
    /// A newly created intent that wants a commit but did not freeze the identity
    /// that produces it could never be rebuilt from durable data alone.
    DesiredCommitWithoutCommitSpec,
    CommitSpecParentMismatch,
    CommitSpecTreeMismatch,
    CommitSpecAuthorTimeMismatch,
    CommitSpecCommitterTimeMismatch,
}

impl fmt::Display for PublishRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseCommit => formatter.write_str("publish run base commit ID is invalid"),
            Self::InvalidReviewedTree => {
                formatter.write_str("publish run reviewed tree ID is invalid")
            }
            Self::CommitSpecWithoutDesiredCommit => formatter
                .write_str("publish run carries a commit specification without a desired commit"),
            Self::DesiredCommitWithoutCommitSpec => formatter.write_str(
                "a new publish run must freeze the commit specification for its desired commit",
            ),
            Self::CommitSpecParentMismatch => {
                formatter.write_str("publish run commit specification has another parent")
            }
            Self::CommitSpecTreeMismatch => {
                formatter.write_str("publish run commit specification has another tree")
            }
            Self::CommitSpecAuthorTimeMismatch => {
                formatter.write_str("publish run commit specification author time differs")
            }
            Self::CommitSpecCommitterTimeMismatch => {
                formatter.write_str("publish run commit specification committer time differs")
            }
        }
    }
}

impl Error for PublishRunError {}

pub trait PublishRunStore {
    type Error: Error;

    fn save(&self, run: &PublishRun) -> Result<(), Self::Error>;
    fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error>;
    fn list(&self) -> Result<Vec<PublishRun>, Self::Error>;
    fn list_for_target(&self, target: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publication::git::GitTreeOid;

    const BASE: char = 'a';
    const TREE: char = 'b';
    const TIME: u64 = 1_500;

    fn commit(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn reviewed(base: &str, tree: &str) -> ReviewedGitTree {
        ReviewedGitTree::from_parts(
            base,
            "9".repeat(40),
            Sha256::new([7; 32]),
            SnapshotId::new(4).unwrap(),
            tree,
            ManagedRoot::new("content").unwrap(),
        )
    }

    /// A newly created intent, built exactly the way the publication application
    /// builds one: the specification that implements the desired commit travels
    /// with it.
    fn run(desired_commit: Option<GitCommitOid>) -> PublishRun {
        let commit_spec = desired_commit.as_ref().map(|_| spec());
        create(desired_commit, commit_spec).unwrap()
    }

    fn create(
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
    ) -> Result<PublishRun, PublishRunError> {
        PublishRun::from_reviewed_tree(
            PublishRunId::new(1).unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            &reviewed(&BASE.to_string().repeat(40), &TREE.to_string().repeat(40)),
            desired_commit,
            commit_spec,
            TimestampMillis::from_unix_millis(TIME),
        )
    }

    /// The one specification that agrees with [`run`] about parent, tree, and
    /// both times.
    fn spec() -> GitCommitSpec {
        spec_for(BASE, TREE, TIME)
    }

    fn spec_for(parent: char, tree: char, time_unix_ms: u64) -> GitCommitSpec {
        let time = TimestampMillis::from_unix_millis(time_unix_ms);
        GitCommitSpec::new(
            commit(parent),
            GitTreeOid::new(std::iter::repeat_n(tree, 40).collect::<String>()).unwrap(),
            "Mineral Publisher",
            "publisher@example.invalid",
            time,
            "Mineral Publisher",
            "publisher@example.invalid",
            time,
            "Publish Mineral content",
        )
        .unwrap()
    }

    fn rehydrate_with(
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
    ) -> Result<PublishRun, PublishRunError> {
        PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(4).unwrap(),
            Sha256::new([7; 32]),
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            BASE.to_string().repeat(40),
            TREE.to_string().repeat(40),
            desired_commit,
            commit_spec,
            TIME,
        )
    }

    #[test]
    fn intent_is_built_from_the_reviewed_tree_fact() {
        let run = run(Some(commit('c')));

        assert_eq!(run.snapshot_id(), SnapshotId::new(4).unwrap());
        assert_eq!(run.projection_sha256(), Sha256::new([7; 32]));
        assert_eq!(run.managed_root().as_str(), "content");
        assert_eq!(run.base_commit(), "a".repeat(40));
        assert_eq!(run.reviewed_tree(), "b".repeat(40));
        assert_eq!(run.created_at_unix_ms(), 1_500);
        assert_eq!(run.target_id().as_str(), "origin:refs/heads/main");
        assert_eq!(run.repository().as_str(), "/srv/public-repo");
        assert_eq!(run.target().destination_ref(), "refs/heads/main");
        assert_eq!(run.commit_spec(), Some(&spec()));
    }

    #[test]
    fn desired_commit_is_the_only_publication_fact() {
        assert_eq!(run(None).desired_commit(), None);
        assert_eq!(run(None).publication(), PublishRunPublication::Noop);

        let ready = run(Some(commit('c')));
        assert_eq!(ready.desired_commit(), Some(&commit('c')));
        assert_eq!(
            ready.publication(),
            PublishRunPublication::CommitReady {
                commit_oid: commit('c'),
            }
        );
    }

    #[test]
    fn rehydrated_desired_commit_survives_without_a_second_fact_source() {
        let original = run(Some(commit('c')));
        let rehydrated = PublishRun::rehydrate(
            original.id(),
            original.snapshot_id(),
            original.projection_sha256(),
            original.managed_root().clone(),
            original.target_id().clone(),
            original.repository().clone(),
            original.target().clone(),
            original.base_commit().to_owned(),
            original.reviewed_tree().to_owned(),
            original.desired_commit().cloned(),
            original.commit_spec().cloned(),
            original.created_at_unix_ms(),
        )
        .unwrap();

        assert_eq!(rehydrated, original);
        assert_eq!(rehydrated.publication(), original.publication());
    }

    #[test]
    fn the_three_legal_desired_commit_and_commit_spec_states_are_accepted() {
        // Noop: (None, None).
        let noop = rehydrate_with(None, None).unwrap();
        assert_eq!(noop.desired_commit(), None);
        assert_eq!(noop.commit_spec(), None);

        // Reconstructible: (Some, Some).
        let reconstructible = rehydrate_with(Some(commit('c')), Some(spec())).unwrap();
        assert_eq!(reconstructible.desired_commit(), Some(&commit('c')));
        assert_eq!(reconstructible.commit_spec(), Some(&spec()));

        // Historical: (Some, None) is a degraded but legal shape, not corruption.
        let legacy = rehydrate_with(Some(commit('c')), None).unwrap();
        assert_eq!(legacy.desired_commit(), Some(&commit('c')));
        assert_eq!(legacy.commit_spec(), None);
    }

    #[test]
    fn a_new_intent_is_always_reconstructible_or_a_noop() {
        let noop = create(None, None).unwrap();
        assert_eq!(noop.desired_commit(), None);
        assert_eq!(noop.commit_spec(), None);

        let reconstructible = create(Some(commit('c')), Some(spec())).unwrap();
        assert_eq!(reconstructible.desired_commit(), Some(&commit('c')));
        assert_eq!(reconstructible.commit_spec(), Some(&spec()));
    }

    #[test]
    fn a_new_intent_cannot_want_a_commit_without_freezing_its_identity() {
        assert_eq!(
            create(Some(commit('c')), None),
            Err(PublishRunError::DesiredCommitWithoutCommitSpec)
        );
    }

    #[test]
    fn a_new_intent_cannot_freeze_a_commit_identity_without_wanting_a_commit() {
        assert_eq!(
            create(None, Some(spec())),
            Err(PublishRunError::CommitSpecWithoutDesiredCommit)
        );
    }

    /// The boundary of this step: the same `(Some, None)` pair is legal when it
    /// comes back out of the database and illegal when something tries to create
    /// it, so no normal creation path can produce a legacy intent.
    #[test]
    fn legacy_tolerance_exists_only_on_the_persistence_path() {
        assert!(rehydrate_with(Some(commit('c')), None).is_ok());
        assert_eq!(
            create(Some(commit('c')), None),
            Err(PublishRunError::DesiredCommitWithoutCommitSpec)
        );
    }

    #[test]
    fn a_commit_spec_without_a_desired_commit_is_rejected() {
        assert_eq!(
            rehydrate_with(None, Some(spec())),
            Err(PublishRunError::CommitSpecWithoutDesiredCommit)
        );
    }

    #[test]
    fn a_commit_spec_that_disagrees_with_the_run_is_rejected() {
        // The tree a specification reviews is the tree that was reviewed.
        assert_eq!(
            rehydrate_with(Some(commit('c')), Some(spec_for(BASE, 'd', TIME))),
            Err(PublishRunError::CommitSpecTreeMismatch)
        );
        // The parent a specification builds on is the base the run observed.
        assert_eq!(
            rehydrate_with(Some(commit('c')), Some(spec_for('d', TREE, TIME))),
            Err(PublishRunError::CommitSpecParentMismatch)
        );
        // Both times belong to the frozen attempt instant.
        assert_eq!(
            rehydrate_with(Some(commit('c')), Some(spec_for(BASE, TREE, TIME + 1))),
            Err(PublishRunError::CommitSpecAuthorTimeMismatch)
        );
    }

    #[test]
    fn a_commit_spec_whose_committer_time_disagrees_is_rejected() {
        let time = TimestampMillis::from_unix_millis(TIME);
        let spec = GitCommitSpec::new(
            commit(BASE),
            GitTreeOid::new(std::iter::repeat_n(TREE, 40).collect::<String>()).unwrap(),
            "Mineral Publisher",
            "publisher@example.invalid",
            time,
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(TIME + 1),
            "Publish Mineral content",
        )
        .unwrap();

        assert_eq!(
            rehydrate_with(Some(commit('c')), Some(spec)),
            Err(PublishRunError::CommitSpecCommitterTimeMismatch)
        );
    }

    #[test]
    fn unusable_reviewed_identities_are_rejected() {
        let error = PublishRun::from_reviewed_tree(
            PublishRunId::new(1).unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            &reviewed("not-a-commit", &"b".repeat(40)),
            None,
            None,
            TimestampMillis::UNIX_EPOCH,
        )
        .unwrap_err();
        assert_eq!(error, PublishRunError::InvalidBaseCommit);

        let error = PublishRun::from_reviewed_tree(
            PublishRunId::new(1).unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            &reviewed(&"a".repeat(40), ""),
            None,
            None,
            TimestampMillis::UNIX_EPOCH,
        )
        .unwrap_err();
        assert_eq!(error, PublishRunError::InvalidReviewedTree);
    }
}
