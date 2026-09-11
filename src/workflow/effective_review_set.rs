use std::{error::Error, fmt};

use crate::{
    domain::{ContentPath, SnapshotId},
    policy::{ReviewRunId, ReviewRunStore},
};

use super::{
    AssetReviewRunId, AssetReviewRunStore, AssetReviewWorkflowResult, EffectiveReviewDecision,
    EffectiveReviewDecisionError, HumanReviewResolution, HumanReviewStore, HumanReviewSubject,
    PublicPolicyRunResult,
};

/// A Snapshot-bound view derived only from workflow-selected automatic review attempts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveReviewSet {
    snapshot_id: SnapshotId,
    documents: Vec<EffectiveDocumentReview>,
    assets: Vec<EffectiveAssetReview>,
}

pub type EffectiveReviewSetBuildResult<D, A, H> = Result<
    EffectiveReviewSet,
    EffectiveReviewSetError<
        <D as ReviewRunStore>::Error,
        <A as AssetReviewRunStore>::Error,
        <H as HumanReviewStore>::Error,
    >,
>;

impl EffectiveReviewSet {
    #[cfg(test)]
    pub(crate) fn from_assets_for_test(
        snapshot_id: SnapshotId,
        mut assets: Vec<EffectiveAssetReview>,
    ) -> Self {
        assets.sort_by(|left, right| left.content_path.cmp(&right.content_path));
        Self {
            snapshot_id,
            documents: Vec::new(),
            assets,
        }
    }

    pub fn build<D, A, H>(
        documents: &PublicPolicyRunResult,
        assets: &AssetReviewWorkflowResult,
        document_runs: &D,
        asset_runs: &A,
        human_reviews: &H,
    ) -> EffectiveReviewSetBuildResult<D, A, H>
    where
        D: ReviewRunStore + ?Sized,
        A: AssetReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
    {
        if documents.snapshot_id() != assets.snapshot_id() {
            return Err(EffectiveReviewSetError::SnapshotMismatch {
                document_snapshot_id: documents.snapshot_id(),
                asset_snapshot_id: assets.snapshot_id(),
            });
        }

        let snapshot_id = documents.snapshot_id();
        let mut effective_documents = Vec::new();
        for document in documents.private_documents() {
            effective_documents.push(EffectiveDocumentReview::private(document.path().clone()));
        }
        for document in documents.invalid_privacy_documents() {
            effective_documents.push(EffectiveDocumentReview::invalid_privacy(
                document.path().clone(),
            ));
        }
        for selected in documents.document_outcomes() {
            let run = document_runs
                .get(selected.id())
                .map_err(EffectiveReviewSetError::DocumentStore)?
                .ok_or_else(|| EffectiveReviewSetError::DocumentRunNotFound {
                    path: selected.content_path().clone(),
                    review_run_id: selected.id(),
                })?;
            if run != *selected
                || run.snapshot_id() != snapshot_id
                || run.content_path() != selected.content_path()
            {
                return Err(EffectiveReviewSetError::DocumentRunMismatch {
                    path: selected.content_path().clone(),
                    review_run_id: selected.id(),
                    actual_path: run.content_path().clone(),
                    actual_snapshot_id: run.snapshot_id(),
                });
            }
            let resolution = human_reviews
                .get_for_subject(HumanReviewSubject::Document(run.id()))
                .map_err(EffectiveReviewSetError::HumanStore)?;
            let decision = HumanReviewResolution::effective_document(&run, resolution.as_ref())
                .map_err(EffectiveReviewSetError::DocumentHumanSubjectMismatch)?;
            effective_documents.push(EffectiveDocumentReview::reviewed(
                run.content_path().clone(),
                run.id(),
                decision,
            ));
        }
        effective_documents.sort_by(|left, right| left.content_path.cmp(&right.content_path));

        let mut effective_assets = Vec::new();
        for selected in assets.entries() {
            let run = asset_runs
                .get(selected.review_run_id())
                .map_err(EffectiveReviewSetError::AssetStore)?
                .ok_or_else(|| EffectiveReviewSetError::AssetRunNotFound {
                    path: selected.content_path().clone(),
                    review_run_id: selected.review_run_id(),
                })?;
            if run.snapshot_id() != snapshot_id
                || run.content_path() != selected.content_path()
                || run.outcome() != selected.outcome()
            {
                return Err(EffectiveReviewSetError::AssetRunMismatch {
                    path: selected.content_path().clone(),
                    review_run_id: selected.review_run_id(),
                    actual_path: run.content_path().clone(),
                    actual_snapshot_id: run.snapshot_id(),
                });
            }
            let resolution = human_reviews
                .get_for_subject(HumanReviewSubject::Asset(run.id()))
                .map_err(EffectiveReviewSetError::HumanStore)?;
            let decision = HumanReviewResolution::effective_asset(&run, resolution.as_ref())
                .map_err(EffectiveReviewSetError::AssetHumanSubjectMismatch)?;
            effective_assets.push(EffectiveAssetReview {
                content_path: run.content_path().clone(),
                review_run_id: run.id(),
                decision,
                dependents: run.outcome().dependents().to_vec(),
            });
        }
        effective_assets.sort_by(|left, right| left.content_path.cmp(&right.content_path));

        Ok(Self {
            snapshot_id,
            documents: effective_documents,
            assets: effective_assets,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }
    pub fn documents(&self) -> &[EffectiveDocumentReview] {
        &self.documents
    }
    pub fn assets(&self) -> &[EffectiveAssetReview] {
        &self.assets
    }
    pub fn approved_markdown_paths(&self) -> Vec<&ContentPath> {
        self.documents
            .iter()
            .filter(|entry| entry.decision == EffectiveDocumentDecision::Approved)
            .map(|entry| &entry.content_path)
            .collect()
    }
    pub fn approved_asset_paths(&self) -> Vec<&ContentPath> {
        self.assets
            .iter()
            .filter(|entry| entry.decision == EffectiveReviewDecision::Approved)
            .map(|entry| &entry.content_path)
            .collect()
    }
    pub fn pending_documents(&self) -> Vec<&EffectiveDocumentReview> {
        self.documents
            .iter()
            .filter(|entry| entry.decision == EffectiveDocumentDecision::PendingHumanReview)
            .collect()
    }
    pub fn pending_assets(&self) -> Vec<&EffectiveAssetReview> {
        self.assets
            .iter()
            .filter(|entry| entry.decision == EffectiveReviewDecision::PendingHumanReview)
            .collect()
    }
    pub fn has_pending_review(&self) -> bool {
        !self.pending_documents().is_empty() || !self.pending_assets().is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveDocumentDecision {
    Private,
    InvalidPrivacy,
    Approved,
    Rejected,
    PendingHumanReview,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveDocumentReview {
    content_path: ContentPath,
    review_run_id: Option<ReviewRunId>,
    decision: EffectiveDocumentDecision,
}
impl EffectiveDocumentReview {
    fn private(content_path: ContentPath) -> Self {
        Self {
            content_path,
            review_run_id: None,
            decision: EffectiveDocumentDecision::Private,
        }
    }
    fn invalid_privacy(content_path: ContentPath) -> Self {
        Self {
            content_path,
            review_run_id: None,
            decision: EffectiveDocumentDecision::InvalidPrivacy,
        }
    }
    fn reviewed(
        content_path: ContentPath,
        review_run_id: ReviewRunId,
        decision: EffectiveReviewDecision,
    ) -> Self {
        Self {
            content_path,
            review_run_id: Some(review_run_id),
            decision: match decision {
                EffectiveReviewDecision::Approved => EffectiveDocumentDecision::Approved,
                EffectiveReviewDecision::Rejected => EffectiveDocumentDecision::Rejected,
                EffectiveReviewDecision::PendingHumanReview => {
                    EffectiveDocumentDecision::PendingHumanReview
                }
            },
        }
    }
    pub fn content_path(&self) -> &ContentPath {
        &self.content_path
    }
    pub fn review_run_id(&self) -> Option<ReviewRunId> {
        self.review_run_id
    }
    pub fn decision(&self) -> EffectiveDocumentDecision {
        self.decision
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveAssetReview {
    content_path: ContentPath,
    review_run_id: AssetReviewRunId,
    decision: EffectiveReviewDecision,
    dependents: Vec<ContentPath>,
}
impl EffectiveAssetReview {
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        content_path: ContentPath,
        review_run_id: AssetReviewRunId,
        decision: EffectiveReviewDecision,
        dependents: Vec<ContentPath>,
    ) -> Self {
        Self {
            content_path,
            review_run_id,
            decision,
            dependents,
        }
    }

    pub fn content_path(&self) -> &ContentPath {
        &self.content_path
    }
    pub fn review_run_id(&self) -> AssetReviewRunId {
        self.review_run_id
    }
    pub fn decision(&self) -> EffectiveReviewDecision {
        self.decision
    }
    pub fn dependents(&self) -> &[ContentPath] {
        &self.dependents
    }
}

#[derive(Debug)]
pub enum EffectiveReviewSetError<D, A, H> {
    SnapshotMismatch {
        document_snapshot_id: SnapshotId,
        asset_snapshot_id: SnapshotId,
    },
    DocumentStore(D),
    AssetStore(A),
    HumanStore(H),
    DocumentRunNotFound {
        path: ContentPath,
        review_run_id: ReviewRunId,
    },
    AssetRunNotFound {
        path: ContentPath,
        review_run_id: AssetReviewRunId,
    },
    DocumentRunMismatch {
        path: ContentPath,
        review_run_id: ReviewRunId,
        actual_path: ContentPath,
        actual_snapshot_id: SnapshotId,
    },
    AssetRunMismatch {
        path: ContentPath,
        review_run_id: AssetReviewRunId,
        actual_path: ContentPath,
        actual_snapshot_id: SnapshotId,
    },
    DocumentHumanSubjectMismatch(EffectiveReviewDecisionError),
    AssetHumanSubjectMismatch(EffectiveReviewDecisionError),
}
impl<D: fmt::Display, A: fmt::Display, H: fmt::Display> fmt::Display
    for EffectiveReviewSetError<D, A, H>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch { .. } => {
                f.write_str("document and asset workflow results have different snapshots")
            }
            Self::DocumentStore(e) => write!(f, "document review lookup failed: {e}"),
            Self::AssetStore(e) => write!(f, "asset review lookup failed: {e}"),
            Self::HumanStore(e) => write!(f, "human review lookup failed: {e}"),
            Self::DocumentRunNotFound {
                path,
                review_run_id,
            } => write!(
                f,
                "selected document review {review_run_id:?} for {path} was not found"
            ),
            Self::AssetRunNotFound {
                path,
                review_run_id,
            } => write!(
                f,
                "selected asset review {review_run_id:?} for {path} was not found"
            ),
            Self::DocumentRunMismatch {
                path,
                review_run_id,
                actual_path,
                actual_snapshot_id,
            } => write!(
                f,
                "selected document review {review_run_id:?} for {path} instead belongs to {actual_path} in snapshot {actual_snapshot_id:?}"
            ),
            Self::AssetRunMismatch {
                path,
                review_run_id,
                actual_path,
                actual_snapshot_id,
            } => write!(
                f,
                "selected asset review {review_run_id:?} for {path} instead belongs to {actual_path} in snapshot {actual_snapshot_id:?}"
            ),
            Self::DocumentHumanSubjectMismatch(e) | Self::AssetHumanSubjectMismatch(e) => e.fmt(f),
        }
    }
}
impl<D: Error + 'static, A: Error + 'static, H: Error + 'static> Error
    for EffectiveReviewSetError<D, A, H>
{
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fmt};

    use crate::{
        domain::{Sha256, SnapshotId},
        policy::{
            HumanReviewReason, PolicyIdentity, PrivateDocument, PrivateReason,
            PublicPolicyDecision, ReviewRun,
        },
        storage::{SqliteAssetReviewRunStore, SqliteHumanReviewStore, SqliteReviewRunStore},
    };

    use super::*;
    use crate::workflow::{
        AssetHumanReviewReason, AssetReviewDecision, AssetReviewDisposition, AssetReviewOutcome,
        AssetReviewRun, AssetReviewWorkflowEntry, HumanReviewDecision, HumanReviewId,
        HumanReviewRecord,
    };

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }
    fn snapshot() -> SnapshotId {
        SnapshotId::new(1).unwrap()
    }
    fn policy() -> PolicyIdentity {
        PolicyIdentity::new("test", "v1", Sha256::new([7; 32])).unwrap()
    }
    fn document(id: u64, path_value: &str, decision: PublicPolicyDecision) -> ReviewRun {
        ReviewRun::rehydrate(
            ReviewRunId::new(id).unwrap(),
            snapshot(),
            path(path_value),
            Sha256::new([id as u8; 32]),
            policy(),
            decision,
            id,
        )
    }
    fn asset_outcome(
        path_value: &str,
        dependents: &[&str],
        disposition: AssetReviewDisposition,
    ) -> AssetReviewOutcome {
        AssetReviewOutcome::from_parts_for_test(
            path(path_value),
            dependents.iter().map(|value| path(value)).collect(),
            Some(Sha256::new([3; 32])),
            Vec::new(),
            disposition,
        )
    }
    fn asset(id: u64, outcome: AssetReviewOutcome) -> AssetReviewRun {
        AssetReviewRun::rehydrate(
            AssetReviewRunId::new(id).unwrap(),
            snapshot(),
            outcome.path().clone(),
            Sha256::new([3; 32]),
            policy(),
            outcome,
            id,
        )
    }
    fn document_result(runs: Vec<ReviewRun>) -> PublicPolicyRunResult {
        PublicPolicyRunResult::from_document_outcomes_for_test(snapshot(), runs)
    }
    fn asset_result(runs: &[AssetReviewRun]) -> AssetReviewWorkflowResult {
        AssetReviewWorkflowResult::from_entries_for_test(
            snapshot(),
            runs.iter()
                .map(|run| {
                    AssetReviewWorkflowEntry::from_parts_for_test(
                        run.content_path().clone(),
                        run.id(),
                        run.outcome().clone(),
                    )
                })
                .collect(),
        )
    }
    fn resolution(
        id: u64,
        subject: HumanReviewSubject,
        decision: HumanReviewDecision,
    ) -> HumanReviewRecord {
        HumanReviewRecord::rehydrate(
            HumanReviewId::new(id).unwrap(),
            subject,
            decision,
            id,
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn documents_keep_privacy_distinct_and_merge_explicit_human_resolutions() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let runs = vec![
            document(1, "approved.md", PublicPolicyDecision::ReviewApproved),
            document(2, "rejected.md", PublicPolicyDecision::ReviewRejected),
            document(
                3,
                "program.md",
                PublicPolicyDecision::ProgramIssues(Vec::new()),
            ),
            document(
                4,
                "pending.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document(
                5,
                "human-approved.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document(
                6,
                "human-rejected.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document(
                7,
                "failed.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(
                    crate::policy::ReviewerError::new("offline"),
                )),
            ),
        ];
        for run in &runs {
            documents.save(run).unwrap();
        }
        human
            .save(&resolution(
                1,
                HumanReviewSubject::Document(runs[4].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&resolution(
                2,
                HumanReviewSubject::Document(runs[5].id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                3,
                HumanReviewSubject::Document(runs[6].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        let result = PublicPolicyRunResult::from_parts_for_test(
            snapshot(),
            vec![PrivateDocument::from_parts_for_test(
                path("private.md"),
                vec![PrivateReason::FrontmatterPrivate],
            )],
            vec![crate::policy::InvalidPrivacyDocument::from_parts_for_test(
                path("invalid.md"),
                "bad yaml",
            )],
            runs,
        );
        let set =
            EffectiveReviewSet::build(&result, &asset_result(&[]), &documents, &assets, &human)
                .unwrap();
        assert_eq!(
            set.approved_markdown_paths(),
            [
                path("approved.md"),
                path("failed.md"),
                path("human-approved.md")
            ]
            .iter()
            .collect::<Vec<_>>()
        );
        assert_eq!(set.pending_documents().len(), 1);
        assert!(set.has_pending_review());
        assert!(
            set.documents()
                .iter()
                .any(|entry| entry.content_path() == &path("private.md")
                    && entry.decision() == EffectiveDocumentDecision::Private
                    && entry.review_run_id().is_none())
        );
        assert!(
            set.documents()
                .iter()
                .any(|entry| entry.content_path() == &path("invalid.md")
                    && entry.decision() == EffectiveDocumentDecision::InvalidPrivacy)
        );
        assert!(
            set.documents()
                .iter()
                .any(|entry| entry.content_path() == &path("program.md")
                    && entry.decision() == EffectiveDocumentDecision::Rejected)
        );
    }

    #[test]
    fn assets_merge_all_automatic_and_human_effective_decisions() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let runs = vec![
            asset(
                1,
                asset_outcome(
                    "approved.png",
                    &["a.md"],
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
                ),
            ),
            asset(
                2,
                asset_outcome(
                    "rejected.png",
                    &["a.md"],
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject),
                ),
            ),
            asset(
                3,
                asset_outcome("blocked.png", &["a.md"], AssetReviewDisposition::Blocked),
            ),
            asset(
                4,
                asset_outcome(
                    "pending.png",
                    &["a.md"],
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview),
                ),
            ),
            asset(
                5,
                asset_outcome(
                    "human-approved.png",
                    &["a.md"],
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::PolicyFindings,
                    ),
                ),
            ),
            asset(
                6,
                asset_outcome(
                    "human-rejected.png",
                    &["a.md"],
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::PolicyFindings,
                    ),
                ),
            ),
            asset(
                7,
                asset_outcome(
                    "failed-approved.png",
                    &["a.md"],
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::ReviewerFailed(
                            crate::workflow::AssetReviewerError::new("offline"),
                        ),
                    ),
                ),
            ),
        ];
        for run in &runs {
            assets.save(run).unwrap();
        }
        human
            .save(&resolution(
                1,
                HumanReviewSubject::Asset(runs[4].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&resolution(
                2,
                HumanReviewSubject::Asset(runs[5].id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                3,
                HumanReviewSubject::Asset(runs[6].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(Vec::new()),
            &asset_result(&runs),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.approved_asset_paths(),
            [
                path("approved.png"),
                path("failed-approved.png"),
                path("human-approved.png")
            ]
            .iter()
            .collect::<Vec<_>>()
        );
        assert_eq!(set.pending_assets().len(), 1);
        assert!(
            set.assets()
                .iter()
                .any(|entry| entry.content_path() == &path("blocked.png")
                    && entry.decision() == EffectiveReviewDecision::Rejected)
        );
    }

    #[test]
    fn selected_attempts_win_over_newer_attempts_for_documents_and_assets() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let old_document = document(
            1,
            "a.md",
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        let new_document = document(
            2,
            "a.md",
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        let old_asset = asset(
            1,
            asset_outcome(
                "a.png",
                &["a.md"],
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            ),
        );
        let new_asset = asset(
            2,
            asset_outcome(
                "a.png",
                &["a.md"],
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            ),
        );
        for run in [&old_document, &new_document] {
            documents.save(run).unwrap();
        }
        for run in [&old_asset, &new_asset] {
            assets.save(run).unwrap();
        }
        human
            .save(&resolution(
                1,
                HumanReviewSubject::Document(old_document.id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                2,
                HumanReviewSubject::Document(new_document.id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&resolution(
                3,
                HumanReviewSubject::Asset(old_asset.id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                4,
                HumanReviewSubject::Asset(new_asset.id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(vec![old_document]),
            &asset_result(&[old_asset]),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.documents()[0].decision(),
            EffectiveDocumentDecision::Rejected
        );
        assert_eq!(
            set.assets()[0].decision(),
            EffectiveReviewDecision::Rejected
        );
    }

    #[test]
    fn run_path_and_snapshot_mismatches_fail_closed() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let stored = document(1, "actual.md", PublicPolicyDecision::ReviewApproved);
        documents.save(&stored).unwrap();
        let selected = document(1, "claimed.md", PublicPolicyDecision::ReviewApproved);
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(vec![selected]),
                &asset_result(&[]),
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::DocumentRunMismatch { .. })
        ));
        let asset_stored = asset(
            1,
            asset_outcome("actual.png", &["a.md"], AssetReviewDisposition::Blocked),
        );
        assets.save(&asset_stored).unwrap();
        let selected_asset = AssetReviewWorkflowResult::from_entries_for_test(
            snapshot(),
            vec![AssetReviewWorkflowEntry::from_parts_for_test(
                path("claimed.png"),
                asset_stored.id(),
                asset_stored.outcome().clone(),
            )],
        );
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(Vec::new()),
                &selected_asset,
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::AssetRunMismatch { .. })
        ));
        let other_snapshot = AssetReviewWorkflowResult::from_entries_for_test(
            SnapshotId::new(2).unwrap(),
            Vec::new(),
        );
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(Vec::new()),
                &other_snapshot,
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::SnapshotMismatch { .. })
        ));
    }

    #[derive(Debug)]
    struct TestError;
    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test")
        }
    }
    impl Error for TestError {}
    struct WrongSubjectStore(HumanReviewRecord);
    impl HumanReviewStore for WrongSubjectStore {
        type Error = TestError;
        fn save(&self, _: &HumanReviewRecord) -> Result<(), Self::Error> {
            Ok(())
        }
        fn get(&self, _: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(None)
        }
        fn get_for_subject(
            &self,
            _: HumanReviewSubject,
        ) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(Some(self.0.clone()))
        }
        fn list(&self) -> Result<Vec<HumanReviewRecord>, Self::Error> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn wrong_human_subject_cannot_resolve_a_selected_run() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let run = document(
            1,
            "a.md",
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        documents.save(&run).unwrap();
        let human = WrongSubjectStore(resolution(
            1,
            HumanReviewSubject::Asset(AssetReviewRunId::new(1).unwrap()),
            HumanReviewDecision::Approve,
        ));
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(vec![run]),
                &asset_result(&[]),
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::DocumentHumanSubjectMismatch(_))
        ));
    }

    #[test]
    fn asset_order_and_shared_dependents_are_preserved() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let late = asset(
            2,
            asset_outcome(
                "z.png",
                &["a.md"],
                AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            ),
        );
        let shared = asset(
            1,
            asset_outcome(
                "shared.png",
                &["a.md", "b.md"],
                AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            ),
        );
        assets.save(&late).unwrap();
        assets.save(&shared).unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(Vec::new()),
            &asset_result(&[late, shared]),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.assets()
                .iter()
                .map(|entry| entry.content_path().as_str())
                .collect::<Vec<_>>(),
            ["shared.png", "z.png"]
        );
        assert_eq!(set.assets()[0].dependents(), [path("a.md"), path("b.md")]);
    }
}
