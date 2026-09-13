mod asset_program_check;
mod asset_review_workflow;
mod asset_sanitization;
mod publication_application;

pub use asset_program_check::{AssetProgramCheck, AssetProgramCheckError};
pub use asset_review_workflow::{
    AssetReviewWorkflow, AssetReviewWorkflowError, AssetReviewWorkflowFailure,
    AssetReviewWorkflowInput,
};
pub use asset_sanitization::{AssetSanitizationError, AssetSanitizer};
pub use mineral_core::workflow::{
    ASSET_OBJECT_KEY_PREFIX, ActualAssetType, AssetCheckFinding, AssetCheckResult,
    AssetContentType, AssetContentTypeError, AssetDeliveryConfig, AssetDeliveryConfigError,
    AssetHumanReviewReason, AssetInspector, AssetObjectKey, AssetPolicy, AssetPolicyDecision,
    AssetPolicyOutcome, AssetPolicyResult, AssetProjection, AssetPublicBaseUrl, AssetPublicUrl,
    AssetPublicationFacts, AssetReviewCandidate, AssetReviewDecision, AssetReviewDisposition,
    AssetReviewEvaluator, AssetReviewOutcome, AssetReviewReasonCode, AssetReviewResult,
    AssetReviewRun, AssetReviewRunError, AssetReviewRunId, AssetReviewRunIdGenerator,
    AssetReviewRunStore, AssetReviewWorkflowEntry, AssetReviewWorkflowResult, AssetReviewer,
    AssetReviewerError, AssetReviewerErrorKind, AssetReviewerReport, AssetReviewerReportError,
    BlockedMarkdown, CandidateAsset, CandidateAssetSelectionError, CandidateAssetSet, CheckedAsset,
    CurrentTargetEntry, CurrentTargetState, CurrentTargetStateError, DeliveryProjection,
    DeliveryProjectionBuilder, DeliveryProjectionError, DeliveryProjectionStore,
    DeliveryProjectionWire, DeliveryProjectionWireError, EffectiveAssetReview,
    EffectiveDocumentDecision, EffectiveDocumentReview, EffectiveReviewDecision,
    EffectiveReviewDecisionError, EffectiveReviewSet, EffectiveReviewSetError,
    FinalDependencyClosureError, FinalPublicationSet, HumanReviewAttempt, HumanReviewBinding,
    HumanReviewDecision, HumanReviewId, HumanReviewKind, HumanReviewRecord, HumanReviewRecordError,
    HumanReviewResolution, HumanReviewResolutionError, HumanReviewStore, HumanReviewSubject,
    ImageDimensions, ImageSanitizationFormat, MAX_ASSET_REVIEW_SUMMARY_CHARS, ManagedRoot,
    MarkdownBlockingReason, MarkdownReviewEvaluator, MockAssetReviewer, ProjectionEntry,
    ProjectionEntryKind, ProjectionTargetPath, ProjectionTargetPathError, PublicExclusionRule,
    PublicExclusionRuleError, PublicExclusionRules, PublicPolicyRun, PublicPolicyRunError,
    PublicPolicyRunFailure, PublicPolicyRunResult, PublicProjection, PublicProjectionError,
    PublicScopeDecision, PublicScopeIdentity, PublicationFileMode, PublishOperation, PublishPlan,
    PublishPlanError, PublishedAsset, ReviewReuseError, ReviewRunIdGenerator,
    ReviewSubjectIdentity, SanitizationTransformation, SanitizedAsset, SanitizedAssetSet,
    SequentialAssetReviewRunIdGenerator, SequentialAssetReviewRunIdGeneratorError,
    SequentialAssetReviews, SequentialMarkdownReviews, SequentialReviewRunIdGenerator,
    SequentialReviewRunIdGeneratorError, TextProjection, TextProjectionFile,
    is_reusable_asset_conclusion, reuse_asset_review, reuse_document_review,
};
pub use publication_application::{
    CompletedPublication, ExplicitHumanReviewSelection, PublicationApplication,
    PublicationApplicationError, PublicationApplicationOutcome, PublicationApplicationRequest,
    PublicationTrace,
};

/// A human review store with no decisions at all.
///
/// For runtimes that only evaluate reviewers and never consult a human decision
/// (the reviewer calibration and smoke examples). A composition root that decides
/// publications must bind the real store: this one answers "no decision exists" for
/// every subject, so an approval recorded elsewhere would not be seen.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHumanReviews;

impl HumanReviewStore for NoHumanReviews {
    type Error = std::convert::Infallible;

    fn save(&self, _: &HumanReviewRecord) -> Result<(), Self::Error> {
        Ok(())
    }

    fn get(&self, _: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error> {
        Ok(None)
    }

    fn get_for_subject(
        &self,
        _: &HumanReviewSubject,
    ) -> Result<Option<HumanReviewRecord>, Self::Error> {
        Ok(None)
    }

    fn get_for_attempt(
        &self,
        _: HumanReviewAttempt,
    ) -> Result<Option<HumanReviewRecord>, Self::Error> {
        Ok(None)
    }

    fn list(&self) -> Result<Vec<HumanReviewRecord>, Self::Error> {
        Ok(Vec::new())
    }
}
