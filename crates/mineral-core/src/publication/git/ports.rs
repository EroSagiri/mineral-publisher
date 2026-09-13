use std::error::Error;

use crate::workflow::{ManagedRoot, TextProjection};

use super::model::{
    CasOutcome, GitCommitOid, GitCommitSpec, GitCurrentTarget, GitRefTarget, LocalCommitState,
    RefUpdate, RemoteRefState, ReviewedGitTree,
};

/// The only way the engine touches a Git remote.
///
/// Every method either reports an observed fact or performs the one side effect
/// it names. Reconciliation, policy, and publication state transitions are the
/// engine's job and are deliberately absent here: a runtime that implements this
/// port can be a local `git` installation or a hosting API, and neither is
/// trusted to decide whether a publication is correct.
///
/// Local commit facts are deliberately *not* here: whether a commit object still
/// exists, and which parent and tree it names, belongs to the object database and
/// therefore to [`GitRepository`].
pub trait GitRemote {
    type Error: Error + 'static;

    /// Reads the current state of `target` from the remote, without fetching and
    /// without consulting local refs.
    fn observe_ref(&self, target: &GitRefTarget) -> Result<RemoteRefState, Self::Error>;

    /// Applies exactly one compare-and-swap update: the ref moves to
    /// `new_commit` only if it currently holds `expected_old`.
    fn compare_and_swap(&self, update: &RefUpdate) -> Result<CasOutcome, Self::Error>;
}

/// The only way the engine reads or creates Git objects.
///
/// The runtime owns the repository and, for materialization, its own blob source:
/// that keeps large media out of the engine (and, later, out of a Worker's
/// memory) while the engine still decides *when* a tree is read, *when* it is
/// materialized, and *which* frozen [`GitCommitSpec`] becomes a commit.
pub trait GitRepository {
    type Error: Error + 'static;

    /// Reads the managed subtree exactly as committed at `base`, without
    /// consulting the worktree or the index.
    fn read_current(
        &self,
        base: &GitCommitOid,
        root: &ManagedRoot,
    ) -> Result<GitCurrentTarget, Self::Error>;

    /// Materializes the complete delivery text side and reports the exact tree it wrote.
    ///
    /// Only [`TextProjection`] crosses this boundary: binary assets are not part
    /// of a Git target at all, so a runtime physically cannot stage one, and the
    /// URL rewriting that produced these document bytes already happened in the
    /// engine's delivery stage.
    fn materialize(
        &self,
        base: &GitCommitOid,
        text: &TextProjection,
    ) -> Result<ReviewedGitTree, Self::Error>;

    /// Creates the commit object described by `spec` and reports its identity.
    ///
    /// The specification is frozen before this call: a runtime must not read a
    /// clock or choose an author here, or one reviewed tree could acquire two
    /// different commit identities.
    fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error>;

    /// Reports the facts of one commit in the runtime's own object database.
    ///
    /// This is a local question, so it belongs to the repository and not to
    /// [`GitRemote`]: whether the object still exists, and which parent and tree it
    /// names. A runtime that cannot read the object at all reports an error; a
    /// runtime that read it and found nothing reports
    /// [`LocalCommitState::Missing`].
    fn inspect_commit(&self, commit: &GitCommitOid) -> Result<LocalCommitState, Self::Error>;
}
