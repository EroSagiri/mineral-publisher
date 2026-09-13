use std::{error::Error, fmt};

use crate::{
    domain::{Sha256, SnapshotId, TimestampMillis},
    publication::git::{GitCommitOid, GitCommitSpec, GitRefTarget, ReviewedGitTree},
    workflow::{DeliveryProjection, ManagedRoot, PublicExclusionRules},
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

/// The immutable delivery projection one publication intent is bound to.
///
/// `delivery_sha256` locates the durable delivery intent in a
/// [`crate::workflow::DeliveryProjectionStore`]; `text_projection_sha256` is the
/// canonical identity of the exact text tree that review covered. Storing both
/// makes the run checkable from two directions: the projection it later recovers
/// from must carry the recorded delivery identity, and its text side must be the
/// very tree the run's `reviewed_tree` was materialized from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryProjectionBinding {
    delivery_sha256: Sha256,
    text_projection_sha256: Sha256,
}

impl DeliveryProjectionBinding {
    /// Binds one delivery projection and the text side inside it.
    ///
    /// This is the only constructor that reads both identities out of the same
    /// value, so a caller cannot pair a delivery identity with another
    /// projection's text identity by accident.
    pub fn from_projection(projection: &DeliveryProjection) -> Self {
        Self {
            delivery_sha256: projection.delivery_sha256(),
            text_projection_sha256: projection.text().projection_sha256(),
        }
    }

    pub fn new(delivery_sha256: Sha256, text_projection_sha256: Sha256) -> Self {
        Self {
            delivery_sha256,
            text_projection_sha256,
        }
    }

    pub fn delivery_sha256(&self) -> Sha256 {
        self.delivery_sha256
    }

    pub fn text_projection_sha256(&self) -> Sha256 {
        self.text_projection_sha256
    }
}

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
    /// The value rows written before this field was split stored in
    /// `projection_sha256`.
    ///
    /// Its domain meaning is **not** recoverable from the value alone: rows written
    /// by the S5 engine hold the `PublicProjection` identity, and rows written by
    /// the S6.1 delivery path hold the `TextProjection` identity. It is therefore
    /// kept as opaque audit metadata for historical rows, is never written by a
    /// new run, and must not be compared with either identity.
    legacy_projection_sha256: Option<Sha256>,
    /// Identity of the exact text tree `reviewed_tree` was materialized from.
    reviewed_text_projection_sha256: Option<Sha256>,
    /// Identity of the durable delivery projection this run is bound to.
    delivery_projection_sha256: Option<Sha256>,
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
        delivery: &DeliveryProjectionBinding,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
        created_at: TimestampMillis,
    ) -> Result<Self, PublishRunError> {
        if desired_commit.is_some() && commit_spec.is_none() {
            return Err(PublishRunError::DesiredCommitWithoutCommitSpec);
        }
        // The reviewed tree the runtime materialized is the delivery text side; a
        // binding that names another text identity would let the intent point at a
        // projection this review never covered.
        if delivery.text_projection_sha256() != reviewed.projection_sha256() {
            return Err(PublishRunError::DeliveryTextProjectionMismatch {
                reviewed_text_projection_sha256: reviewed.projection_sha256(),
                bound_text_projection_sha256: delivery.text_projection_sha256(),
            });
        }
        Self::from_parts(
            id,
            reviewed.snapshot_id(),
            None,
            Some(delivery.text_projection_sha256()),
            Some(delivery.delivery_sha256()),
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
        legacy_projection_sha256: Option<Sha256>,
        reviewed_text_projection_sha256: Option<Sha256>,
        delivery_projection_sha256: Option<Sha256>,
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
            legacy_projection_sha256,
            reviewed_text_projection_sha256,
            delivery_projection_sha256,
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
        legacy_projection_sha256: Option<Sha256>,
        reviewed_text_projection_sha256: Option<Sha256>,
        delivery_projection_sha256: Option<Sha256>,
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
        // A delivery binding is atomic: a text identity without the projection that
        // contains it (or the reverse) can satisfy nothing.
        if reviewed_text_projection_sha256.is_some() != delivery_projection_sha256.is_some() {
            return Err(PublishRunError::PartialDeliveryProjectionBinding);
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
            legacy_projection_sha256,
            reviewed_text_projection_sha256,
            delivery_projection_sha256,
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
    /// Opaque audit metadata carried by rows written before the projection
    /// identity was split; see the field documentation.
    pub fn legacy_projection_sha256(&self) -> Option<Sha256> {
        self.legacy_projection_sha256
    }

    /// The exact durable delivery projection this run must recover from.
    ///
    /// `None` means the run was written before delivery projections were captured,
    /// so no durable delivery intent exists for it. That is an honest degraded
    /// state, not corruption — but it is also not recoverable by guessing.
    pub fn delivery_projection_binding(&self) -> Option<DeliveryProjectionBinding> {
        match (
            self.delivery_projection_sha256,
            self.reviewed_text_projection_sha256,
        ) {
            (Some(delivery_sha256), Some(text_projection_sha256)) => Some(
                DeliveryProjectionBinding::new(delivery_sha256, text_projection_sha256),
            ),
            _ => None,
        }
    }

    /// Identity of the exact text tree this run reviewed, when it was captured.
    pub fn reviewed_text_projection_sha256(&self) -> Option<Sha256> {
        self.reviewed_text_projection_sha256
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

    /// The same base as a usable Git object identity.
    ///
    /// The constructors reject anything that is not an object ID, so this cannot
    /// fail for an intent that exists.
    pub fn base_commit_oid(&self) -> GitCommitOid {
        GitCommitOid::new(self.base_commit.clone())
            .expect("the publish run constructors reject a base commit that is not an object ID")
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
    /// The bound delivery projection's text side is not the tree that was
    /// materialized for this review.
    DeliveryTextProjectionMismatch {
        reviewed_text_projection_sha256: Sha256,
        bound_text_projection_sha256: Sha256,
    },
    /// Only one half of a delivery binding was stored.
    PartialDeliveryProjectionBinding,
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
            Self::DeliveryTextProjectionMismatch { .. } => formatter.write_str(
                "publish run is bound to a delivery projection whose text side is not the reviewed tree",
            ),
            Self::PartialDeliveryProjectionBinding => formatter.write_str(
                "publish run stores only one half of its delivery projection binding",
            ),
        }
    }
}

impl Error for PublishRunError {}

/// The public scope one publication attempt was decided under, as recorded.
///
/// It is provenance, not identity: it says which source paths the public question
/// was asked about, and it takes no part in what the attempt publishes. It is
/// validated again whenever it is read, so a damaged record is reported rather than
/// quietly meaning "no exclusions".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenPublicScope {
    rules: Vec<String>,
}

impl FrozenPublicScope {
    /// The scope that excludes nothing.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Freezes the validated, canonical rules of one configured scope.
    pub fn of(rules: &PublicExclusionRules) -> Self {
        Self {
            rules: rules.canonical().into_iter().map(str::to_owned).collect(),
        }
    }

    /// Rebuilds a frozen scope from its durable form.
    ///
    /// A stored record is exactly the input that can be damaged, so every rule is
    /// re-validated and the record must already be in canonical form: a hand-edited
    /// row is refused instead of being silently normalized into a different scope.
    pub fn rehydrate(
        rules: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, FrozenPublicScopeError> {
        let rules = rules.into_iter().map(Into::into).collect::<Vec<_>>();
        let scope = PublicExclusionRules::from_canonical(rules.clone())
            .map_err(FrozenPublicScopeError::Rule)?;
        if scope.canonical() != rules.iter().map(String::as_str).collect::<Vec<_>>() {
            return Err(FrozenPublicScopeError::NotCanonical);
        }
        Ok(Self { rules })
    }

    pub fn rules(&self) -> &[String] {
        &self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrozenPublicScopeError {
    /// A stored rule is not a usable public exclusion rule.
    Rule(crate::workflow::PublicExclusionRuleError),
    /// The stored rules are usable but not in canonical form.
    NotCanonical,
}

impl fmt::Display for FrozenPublicScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rule(error) => write!(formatter, "frozen public scope rule is unusable: {error}"),
            Self::NotCanonical => {
                formatter.write_str("frozen public scope is not in canonical form")
            }
        }
    }
}

impl Error for FrozenPublicScopeError {}

pub trait PublishRunStore {
    type Error: Error;

    /// Persists one intent together with the public scope it was decided under.
    ///
    /// It is one call on purpose. A store can commit both facts together, so an
    /// attempt this engine records can never exist without the provenance that
    /// explains which source paths were in scope — and a store that cannot record
    /// that provenance must fail the attempt instead of persisting an
    /// unattributable run.
    fn save(&self, run: &PublishRun, public_scope: &FrozenPublicScope) -> Result<(), Self::Error>;

    fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error>;
    fn list(&self) -> Result<Vec<PublishRun>, Self::Error>;
    fn list_for_target(&self, target: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error>;

    /// The public scope frozen for one attempt, in canonical order.
    ///
    /// `None` means the attempt was recorded before scopes were frozen; it does not
    /// mean the scope was empty.
    fn public_scope(&self, id: PublishRunId) -> Result<Option<FrozenPublicScope>, Self::Error>;
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

    /// The reviewed tree the delivery stage produced: its `projection_sha256` is
    /// the text identity of the delivery projection below.
    fn reviewed(base: &str, tree: &str) -> ReviewedGitTree {
        ReviewedGitTree::from_parts(
            base,
            "9".repeat(40),
            TEXT_SHA256,
            SnapshotId::new(4).unwrap(),
            tree,
            ManagedRoot::new("content").unwrap(),
        )
    }

    const TEXT_SHA256: Sha256 = Sha256::new([7; 32]);
    const DELIVERY_SHA256: Sha256 = Sha256::new([8; 32]);

    /// The binding a new intent must carry: the delivery projection, and the text
    /// side inside it that equals the reviewed tree above.
    fn binding() -> DeliveryProjectionBinding {
        DeliveryProjectionBinding::new(DELIVERY_SHA256, TEXT_SHA256)
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
            &binding(),
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
            Some(Sha256::new([6; 32])),
            Some(TEXT_SHA256),
            Some(DELIVERY_SHA256),
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
        assert_eq!(run.reviewed_text_projection_sha256(), Some(TEXT_SHA256));
        assert_eq!(run.legacy_projection_sha256(), None);
        assert_eq!(run.delivery_projection_binding(), Some(binding()));
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
            original.legacy_projection_sha256(),
            original.reviewed_text_projection_sha256(),
            original
                .delivery_projection_binding()
                .map(|binding| binding.delivery_sha256()),
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
    fn a_new_intent_always_carries_the_delivery_binding_even_for_a_noop() {
        let noop = run(None);
        let commit_ready = run(Some(commit('c')));

        for intent in [&noop, &commit_ready] {
            assert_eq!(
                intent.delivery_projection_binding(),
                Some(DeliveryProjectionBinding::new(DELIVERY_SHA256, TEXT_SHA256)),
                "every new intent must be able to recover its delivery projection"
            );
            assert_eq!(intent.reviewed_text_projection_sha256(), Some(TEXT_SHA256));
            // A new intent never writes the opaque historical column.
            assert_eq!(intent.legacy_projection_sha256(), None);
        }
    }

    #[test]
    fn a_new_intent_bound_to_another_text_projection_is_rejected() {
        let error = PublishRun::from_reviewed_tree(
            PublishRunId::new(1).unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            &reviewed(&BASE.to_string().repeat(40), &TREE.to_string().repeat(40)),
            &DeliveryProjectionBinding::new(DELIVERY_SHA256, Sha256::new([9; 32])),
            None,
            None,
            TimestampMillis::from_unix_millis(TIME),
        )
        .unwrap_err();

        assert_eq!(
            error,
            PublishRunError::DeliveryTextProjectionMismatch {
                reviewed_text_projection_sha256: TEXT_SHA256,
                bound_text_projection_sha256: Sha256::new([9; 32]),
            }
        );
    }

    #[test]
    fn a_historical_intent_has_no_delivery_binding_and_keeps_its_opaque_hash() {
        let historical = PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(4).unwrap(),
            Some(Sha256::new([6; 32])),
            None,
            None,
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            BASE.to_string().repeat(40),
            TREE.to_string().repeat(40),
            Some(commit('c')),
            Some(spec()),
            TIME,
        )
        .unwrap();

        assert_eq!(historical.delivery_projection_binding(), None);
        assert_eq!(historical.reviewed_text_projection_sha256(), None);
        assert_eq!(
            historical.legacy_projection_sha256(),
            Some(Sha256::new([6; 32]))
        );
        // The legacy column is opaque: it is not comparable with either identity.
        assert_ne!(
            historical.legacy_projection_sha256(),
            historical.reviewed_text_projection_sha256()
        );
    }

    #[test]
    fn half_a_delivery_binding_is_never_legal() {
        for (text, delivery) in [(Some(TEXT_SHA256), None), (None, Some(DELIVERY_SHA256))] {
            let error = PublishRun::rehydrate(
                PublishRunId::new(1).unwrap(),
                SnapshotId::new(4).unwrap(),
                None,
                text,
                delivery,
                ManagedRoot::new("content").unwrap(),
                PublishTargetId::new("origin:refs/heads/main").unwrap(),
                RepositoryLocator::new("/srv/public-repo").unwrap(),
                GitRefTarget::new("origin", "refs/heads/main").unwrap(),
                BASE.to_string().repeat(40),
                TREE.to_string().repeat(40),
                None,
                None,
                TIME,
            )
            .unwrap_err();

            assert_eq!(error, PublishRunError::PartialDeliveryProjectionBinding);
        }
    }

    #[test]
    fn unusable_reviewed_identities_are_rejected() {
        let error = PublishRun::from_reviewed_tree(
            PublishRunId::new(1).unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            &reviewed("not-a-commit", &"b".repeat(40)),
            &binding(),
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
            &binding(),
            None,
            None,
            TimestampMillis::UNIX_EPOCH,
        )
        .unwrap_err();
        assert_eq!(error, PublishRunError::InvalidReviewedTree);
    }
}
