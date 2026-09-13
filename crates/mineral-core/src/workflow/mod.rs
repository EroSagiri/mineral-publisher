//! Portable review and publication workflow model.
//!
//! Every module here is free of platform capabilities: the native and
//! Cloudflare hosts re-export these items and supply their own executors,
//! clocks, stores and network adapters.

mod asset_check;
mod asset_policy;
mod asset_review_run;
mod asset_review_workflow;
mod asset_sanitization;
mod candidate_asset;
mod effective_review_set;
mod final_dependency_closure;
mod human_review;
mod public_policy_run;
mod public_projection;
mod publish_plan;

pub use asset_check::{
    ActualAssetType, AssetCheckFinding, AssetCheckResult, AssetInspector, CheckedAsset,
    ImageDimensions,
};
pub use asset_policy::{
    AssetHumanReviewReason, AssetPolicy, AssetPolicyDecision, AssetPolicyOutcome,
    AssetPolicyResult, AssetReviewCandidate, AssetReviewDecision, AssetReviewDisposition,
    AssetReviewOutcome, AssetReviewReasonCode, AssetReviewResult, AssetReviewer,
    AssetReviewerError, AssetReviewerErrorKind, AssetReviewerReport, AssetReviewerReportError,
    MAX_ASSET_REVIEW_SUMMARY_CHARS, MockAssetReviewer,
};
pub use asset_review_run::{
    AssetReviewRun, AssetReviewRunError, AssetReviewRunId, AssetReviewRunStore,
};
pub use asset_review_workflow::{
    AssetReviewEvaluator, AssetReviewRunIdGenerator, AssetReviewWorkflowEntry,
    AssetReviewWorkflowResult, SequentialAssetReviewRunIdGenerator,
    SequentialAssetReviewRunIdGeneratorError, SequentialAssetReviews,
};
pub use asset_sanitization::{
    ImageSanitizationFormat, SanitizationTransformation, SanitizedAsset, SanitizedAssetSet,
};
pub use candidate_asset::{CandidateAsset, CandidateAssetSelectionError, CandidateAssetSet};
pub use effective_review_set::{
    EffectiveAssetReview, EffectiveDocumentDecision, EffectiveDocumentReview, EffectiveReviewSet,
    EffectiveReviewSetError,
};
pub use final_dependency_closure::{
    BlockedMarkdown, FinalDependencyClosureError, FinalPublicationSet, MarkdownBlockingReason,
};
pub use human_review::{
    EffectiveReviewDecision, EffectiveReviewDecisionError, HumanReviewDecision, HumanReviewId,
    HumanReviewRecord, HumanReviewRecordError, HumanReviewResolution, HumanReviewResolutionError,
    HumanReviewStore, HumanReviewSubject,
};
pub use public_policy_run::{
    MarkdownReviewEvaluator, PublicPolicyRun, PublicPolicyRunError, PublicPolicyRunFailure,
    PublicPolicyRunResult, ReviewRunIdGenerator, SequentialMarkdownReviews,
    SequentialReviewRunIdGenerator, SequentialReviewRunIdGeneratorError,
};
pub use public_projection::{
    ManagedRoot, ProjectionEntry, ProjectionEntryKind, ProjectionTargetPath,
    ProjectionTargetPathError, PublicProjection, PublicProjectionError, PublicationFileMode,
};
pub use publish_plan::{
    CurrentTargetEntry, CurrentTargetState, CurrentTargetStateError, PublishOperation, PublishPlan,
    PublishPlanError,
};
