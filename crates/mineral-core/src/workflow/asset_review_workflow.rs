use std::{error::Error, fmt};

use crate::domain::{ContentPath, SnapshotId};

use super::{
    AssetCheckResult, AssetPolicyOutcome, AssetReviewDecision, AssetReviewDisposition,
    AssetReviewOutcome, AssetReviewRunId, AssetReviewer,
};

/// Decides how a batch of reviewed assets is executed.
///
/// Like markdown review, the engine only needs "evaluate these assets"; the
/// execution strategy belongs to the runtime.
pub trait AssetReviewEvaluator<R: AssetReviewer + ?Sized> {
    fn is_bounded(&self) -> bool;
    fn evaluate(&self, outcomes: Vec<AssetPolicyOutcome>, reviewer: &R) -> Vec<AssetReviewOutcome>;
}

/// The portable default: review every asset in order on the caller's thread.
pub struct SequentialAssetReviews;
impl<R: AssetReviewer + ?Sized> AssetReviewEvaluator<R> for SequentialAssetReviews {
    fn is_bounded(&self) -> bool {
        false
    }
    fn evaluate(&self, outcomes: Vec<AssetPolicyOutcome>, reviewer: &R) -> Vec<AssetReviewOutcome> {
        outcomes
            .into_iter()
            .map(|outcome| outcome.review(reviewer))
            .collect()
    }
}

/// Allocates immutable asset-review audit identities at the workflow boundary.
pub trait AssetReviewRunIdGenerator {
    type Error: Error + Send + Sync + 'static;

    fn next_id(&mut self) -> Result<AssetReviewRunId, Self::Error>;
}

/// A small deterministic allocator for callers that own an asset-review ID range.
#[derive(Clone, Debug)]
pub struct SequentialAssetReviewRunIdGenerator {
    next: Option<u64>,
}

impl SequentialAssetReviewRunIdGenerator {
    pub fn new(first: AssetReviewRunId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl AssetReviewRunIdGenerator for SequentialAssetReviewRunIdGenerator {
    type Error = SequentialAssetReviewRunIdGeneratorError;

    fn next_id(&mut self) -> Result<AssetReviewRunId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialAssetReviewRunIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        AssetReviewRunId::new(value)
            .map_err(|_| SequentialAssetReviewRunIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialAssetReviewRunIdGeneratorError {
    Exhausted,
}

impl fmt::Display for SequentialAssetReviewRunIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset review run id sequence is exhausted")
    }
}

impl Error for SequentialAssetReviewRunIdGeneratorError {}

/// One durable asset-review outcome, ordered by content path in a successful result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewWorkflowEntry {
    content_path: ContentPath,
    review_run_id: AssetReviewRunId,
    outcome: AssetReviewOutcome,
}

impl AssetReviewWorkflowEntry {
    pub fn content_path(&self) -> &ContentPath {
        &self.content_path
    }

    pub fn review_run_id(&self) -> AssetReviewRunId {
        self.review_run_id
    }

    pub fn outcome(&self) -> &AssetReviewOutcome {
        &self.outcome
    }

    /// Builds one durable asset-review entry.
    ///
    /// Runtime-side constructor: the engine defines the entry, and the executor
    /// that persists review runs fills it in.
    pub fn from_parts(
        content_path: ContentPath,
        review_run_id: AssetReviewRunId,
        outcome: AssetReviewOutcome,
    ) -> Self {
        Self {
            content_path,
            review_run_id,
            outcome,
        }
    }
}

/// Successful durable prefix of one complete candidate-asset review workflow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewWorkflowResult {
    snapshot_id: SnapshotId,
    entries: Vec<AssetReviewWorkflowEntry>,
    checks: AssetCheckResult,
}

impl AssetReviewWorkflowResult {
    pub fn empty(snapshot_id: SnapshotId) -> Self {
        Self {
            snapshot_id,
            entries: Vec::new(),
            checks: AssetCheckResult::empty(snapshot_id),
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    /// Durably saved outcomes in deterministic `ContentPath` order.
    pub fn entries(&self) -> &[AssetReviewWorkflowEntry] {
        &self.entries
    }

    pub fn checks(&self) -> &AssetCheckResult {
        &self.checks
    }

    /// A convenience view only; it is not a final asset set or publication decision.
    pub fn approved_asset_paths(&self) -> Vec<&ContentPath> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.outcome.disposition(),
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve)
                )
            })
            .map(AssetReviewWorkflowEntry::content_path)
            .collect()
    }

    /// Attaches the completed platform check for this run.
    ///
    /// Runtime-side builder: the executor that runs the checks owns the result,
    /// while the engine owns its shape and accessors.
    pub fn set_checks(&mut self, checks: AssetCheckResult) {
        self.checks = checks;
    }

    /// Appends one durable outcome, preserving the order the executor produced.
    pub fn push_entry(&mut self, entry: AssetReviewWorkflowEntry) {
        self.entries.push(entry);
    }

    #[doc(hidden)]
    pub fn from_entries_for_test(
        snapshot_id: SnapshotId,
        mut entries: Vec<AssetReviewWorkflowEntry>,
    ) -> Self {
        entries.sort_by(|left, right| left.content_path.cmp(&right.content_path));
        Self {
            snapshot_id,
            entries,
            checks: AssetCheckResult::empty(snapshot_id),
        }
    }
}
