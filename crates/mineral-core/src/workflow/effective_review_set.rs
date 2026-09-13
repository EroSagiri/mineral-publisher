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
    #[doc(hidden)]
    pub fn from_assets(snapshot_id: SnapshotId, assets: Vec<EffectiveAssetReview>) -> Self {
        Self::from_parts_for_test(snapshot_id, Vec::new(), assets)
    }

    #[doc(hidden)]
    pub fn from_parts_for_test(
        snapshot_id: SnapshotId,
        documents: Vec<(ContentPath, EffectiveDocumentDecision)>,
        mut assets: Vec<EffectiveAssetReview>,
    ) -> Self {
        let mut documents = documents
            .into_iter()
            .map(|(content_path, decision)| EffectiveDocumentReview {
                content_path,
                review_run_id: None,
                decision,
            })
            .collect::<Vec<_>>();
        documents.sort_by(|left, right| left.content_path.cmp(&right.content_path));
        assets.sort_by(|left, right| left.content_path.cmp(&right.content_path));
        Self {
            snapshot_id,
            documents,
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
    #[doc(hidden)]
    pub fn from_parts_for_test(
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
