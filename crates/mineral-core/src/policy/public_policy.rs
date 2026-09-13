use std::{error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    content::AnalyzedMarkdown,
    domain::{ContentPath, Sha256},
};

use super::{ProgramCheck, ProgramCheckIssue, ProgramCheckResult, PublicCandidateMarkdown};

/// A public Markdown candidate that passed all deterministic program checks.
///
/// This type has no public constructor. The public policy is the only normal
/// entrypoint that can create one, after observing `ProgramCheckResult::Pass`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewCandidate(PublicCandidateMarkdown);

impl ReviewCandidate {
    pub fn path(&self) -> &ContentPath {
        self.0.path()
    }

    /// Exposes the immutable, already-computed analysis to a reviewer.
    pub fn analysis(&self) -> &AnalyzedMarkdown {
        self.0.analysis()
    }
}

/// Provider-independent semantic review result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
    NeedsHumanReview,
}

/// Stable, provider-independent reasons that explain a semantic review decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReasonCode {
    CredentialSecret,
    PrivateContactInformation,
    PrivateIdentityInformation,
    PrivateCorrespondence,
    ThirdPartyPrivateInformation,
    InternalWorkInformation,
    ConfidentialWorkMaterial,
    UnpublishedProductOrTechnicalPlan,
    CustomerOrPartnerInformation,
    SecuritySensitiveInformation,
    UncertainDisclosureAuthorization,
    OrdinaryPersonalContent,
    OrdinaryWorkExperience,
    PublicTechnicalContent,
    OtherPrivacyRisk,
}

impl ReviewReasonCode {
    pub fn is_risk(self) -> bool {
        !matches!(
            self,
            Self::OrdinaryPersonalContent
                | Self::OrdinaryWorkExperience
                | Self::PublicTechnicalContent
        )
    }
}

/// Structured semantic-review result. Explanatory fields are audit context only.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerReport {
    decision: ReviewDecision,
    reason_codes: Vec<ReviewReasonCode>,
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedReviewerReport {
    decision: ReviewDecision,
    reason_codes: Vec<ReviewReasonCode>,
    summary: String,
}

impl<'de> Deserialize<'de> for ReviewerReport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let report = UncheckedReviewerReport::deserialize(deserializer)?;
        Self::new(report.decision, report.reason_codes, report.summary)
            .map_err(serde::de::Error::custom)
    }
}

impl ReviewerReport {
    pub fn new(
        decision: ReviewDecision,
        reason_codes: Vec<ReviewReasonCode>,
        summary: impl Into<String>,
    ) -> Result<Self, ReviewerReportError> {
        let report = Self {
            decision,
            reason_codes,
            summary: summary.into(),
        };
        report.validate()?;
        Ok(report)
    }

    pub fn decision(&self) -> ReviewDecision {
        self.decision
    }

    pub fn reason_codes(&self) -> &[ReviewReasonCode] {
        &self.reason_codes
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn validate(&self) -> Result<(), ReviewerReportError> {
        if self.reason_codes.len() > 8 {
            return Err(ReviewerReportError::TooManyReasonCodes);
        }
        for (index, reason) in self.reason_codes.iter().enumerate() {
            if self.reason_codes[..index].contains(reason) {
                return Err(ReviewerReportError::DuplicateReasonCode);
            }
        }

        let has_risk_reason = self.reason_codes.iter().any(|reason| reason.is_risk());
        match self.decision {
            ReviewDecision::Approve if has_risk_reason => {
                return Err(ReviewerReportError::ApproveWithRiskReason);
            }
            ReviewDecision::Reject | ReviewDecision::NeedsHumanReview if !has_risk_reason => {
                return Err(ReviewerReportError::RiskDecisionWithoutRiskReason);
            }
            _ => {}
        }

        crate::policy::validate_review_summary(&self.summary).map_err(|error| match error {
            crate::policy::ReviewSummaryError::Empty => ReviewerReportError::EmptySummary,
            crate::policy::ReviewSummaryError::TooLong => ReviewerReportError::SummaryTooLong,
            crate::policy::ReviewSummaryError::NotBrief => ReviewerReportError::SummaryNotBrief,
            crate::policy::ReviewSummaryError::NotSafe => ReviewerReportError::SummaryNotSafe,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewerReportError {
    TooManyReasonCodes,
    DuplicateReasonCode,
    ApproveWithRiskReason,
    RiskDecisionWithoutRiskReason,
    EmptySummary,
    SummaryTooLong,
    SummaryNotBrief,
    SummaryNotSafe,
}

impl fmt::Display for ReviewerReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyReasonCodes => "reviewer report contains too many reason codes",
            Self::DuplicateReasonCode => "reviewer report contains a duplicate reason code",
            Self::ApproveWithRiskReason => "approve report contains a risk reason",
            Self::RiskDecisionWithoutRiskReason => {
                "reject or human-review report requires a risk reason"
            }
            Self::EmptySummary => "reviewer report summary cannot be empty",
            Self::SummaryTooLong => "reviewer report summary is too long",
            Self::SummaryNotBrief => "reviewer report summary must be one or two short sentences",
            Self::SummaryNotSafe => "reviewer report summary may contain sensitive raw values",
        })
    }
}

impl Error for ReviewerReportError {}

/// A failure while executing a reviewer.
///
/// Reviewer failures are distinct from review decisions so callers cannot
/// accidentally treat an unavailable or malformed response as approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReviewerError {
    kind: ReviewerErrorKind,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_error_message: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReviewerErrorRepresentation {
    Legacy(String),
    Structured {
        #[serde(default)]
        kind: ReviewerErrorKind,
        message: String,
        #[serde(default)]
        http_status: Option<u16>,
        #[serde(default)]
        provider_error_code: Option<String>,
        #[serde(default)]
        provider_error_message: Option<String>,
    },
}

impl<'de> Deserialize<'de> for ReviewerError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match ReviewerErrorRepresentation::deserialize(deserializer)? {
                ReviewerErrorRepresentation::Legacy(message) => Self::new(message),
                ReviewerErrorRepresentation::Structured {
                    kind,
                    message,
                    http_status,
                    provider_error_code,
                    provider_error_message,
                } => Self {
                    kind,
                    message,
                    http_status,
                    provider_error_code,
                    provider_error_message,
                },
            },
        )
    }
}

impl ReviewerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_kind(ReviewerErrorKind::Other, message)
    }

    pub fn with_kind(kind: ReviewerErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
            provider_error_code: None,
            provider_error_message: None,
        }
    }

    /// Records a status-only provider failure without retaining the response body.
    pub fn with_http_status(
        kind: ReviewerErrorKind,
        status: u16,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: Some(status),
            provider_error_code: None,
            provider_error_message: None,
        }
    }

    /// The provider fields must already be bounded and redacted by the adapter.
    pub fn with_provider_error(
        kind: ReviewerErrorKind,
        status: u16,
        message: impl Into<String>,
        provider_error_code: Option<String>,
        provider_error_message: Option<String>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: Some(status),
            provider_error_code,
            provider_error_message,
        }
    }

    pub fn kind(&self) -> ReviewerErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    pub fn provider_error_code(&self) -> Option<&str> {
        self.provider_error_code.as_deref()
    }

    pub fn provider_error_message(&self) -> Option<&str> {
        self.provider_error_message.as_deref()
    }
}

impl fmt::Display for ReviewerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ReviewerError {}

/// Stable failure categories used by reviewer adapters and fail-closed audit records.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerErrorKind {
    ContentStore,
    InvalidUtf8,
    InputTooLarge,
    Transport,
    Timeout,
    Authentication,
    HttpStatus,
    ResponseTooLarge,
    EmptyResponse,
    MalformedResponse,
    TruncatedResponse,
    #[default]
    Other,
}

/// Reviews only documents that crossed the privacy and program-check boundaries.
///
/// Implementations return a decision and have no publishing authority.
pub trait Reviewer {
    fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError>;
}

/// Why public policy requires a human decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanReviewReason {
    ReviewerRequested,
    ReviewerFailed(ReviewerError),
}

/// The typed reason a document did not advance automatically through public policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", content = "details", rename_all = "snake_case")]
pub enum PublicPolicyDecision {
    ProgramIssues(Vec<ProgramCheckIssue>),
    ReviewApproved,
    ReviewRejected,
    NeedsHumanReview(HumanReviewReason),
}

/// Auditable policy result for one Markdown document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicPolicyOutcome {
    path: ContentPath,
    content_sha256: Sha256,
    decision: PublicPolicyDecision,
    reviewer_report: Option<ReviewerReport>,
}

impl PublicPolicyOutcome {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn decision(&self) -> &PublicPolicyDecision {
        &self.decision
    }

    pub fn content_sha256(&self) -> Sha256 {
        self.content_sha256
    }

    pub fn reviewer_report(&self) -> Option<&ReviewerReport> {
        self.reviewer_report.as_ref()
    }
}

/// Applies deterministic checks and semantic review without publishing anything.
pub struct PublicPolicy;

impl PublicPolicy {
    /// Evaluates privacy-filtered Markdown in deterministic `ContentPath` order.
    ///
    /// Program issues stop the document before the reviewer. Reviewer errors are
    /// retained as an explicit fail-closed state for later human review handling.
    pub fn evaluate<R: Reviewer + ?Sized>(
        mut documents: Vec<PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<PublicPolicyOutcome> {
        documents.sort_by(|left, right| left.path().cmp(right.path()));

        documents
            .into_iter()
            .map(|document| {
                let path = document.path().clone();
                let content_sha256 = document.analysis().file().sha256();
                let (decision, reviewer_report) =
                    match ProgramCheck::check(std::slice::from_ref(&document)) {
                        ProgramCheckResult::Issues(issues) => {
                            (PublicPolicyDecision::ProgramIssues(issues), None)
                        }
                        ProgramCheckResult::Pass | ProgramCheckResult::PassWithWarnings(_) => {
                            let candidate = ReviewCandidate(document);
                            match reviewer.review(&candidate) {
                                Ok(report) => {
                                    let decision = match report.decision() {
                                        ReviewDecision::Approve => {
                                            PublicPolicyDecision::ReviewApproved
                                        }
                                        ReviewDecision::Reject => {
                                            PublicPolicyDecision::ReviewRejected
                                        }
                                        ReviewDecision::NeedsHumanReview => {
                                            PublicPolicyDecision::NeedsHumanReview(
                                                HumanReviewReason::ReviewerRequested,
                                            )
                                        }
                                    };
                                    (decision, Some(report))
                                }
                                Err(error) => (
                                    PublicPolicyDecision::NeedsHumanReview(
                                        HumanReviewReason::ReviewerFailed(error),
                                    ),
                                    None,
                                ),
                            }
                        }
                    };

                PublicPolicyOutcome {
                    path,
                    content_sha256,
                    decision,
                    reviewer_report,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap};

    use crate::{
        content::{AnalyzedMarkdown, MarkdownReferenceParser, Resolution, ResolvedReference},
        domain::{ContentPath, Sha256, SnapshotFile},
        policy::{MarkdownFrontmatterParser, PrivacyFilter},
    };

    use super::*;

    struct MockReviewer {
        calls: Cell<usize>,
        responses: BTreeMap<String, Result<ReviewDecision, ReviewerError>>,
    }

    impl MockReviewer {
        fn returning(response: Result<ReviewDecision, ReviewerError>) -> Self {
            Self {
                calls: Cell::new(0),
                responses: BTreeMap::from([("article.md".to_owned(), response)]),
            }
        }

        fn with_responses(
            responses: impl IntoIterator<Item = (&'static str, Result<ReviewDecision, ReviewerError>)>,
        ) -> Self {
            Self {
                calls: Cell::new(0),
                responses: responses
                    .into_iter()
                    .map(|(path, response)| (path.to_owned(), response))
                    .collect(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    impl Reviewer for MockReviewer {
        fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError> {
            self.calls.set(self.calls.get() + 1);
            match self
                .responses
                .get(candidate.path().as_str())
                .cloned()
                .expect("test response for candidate")
            {
                Ok(decision) => Ok(test_report(decision)),
                Err(error) => Err(error),
            }
        }
    }

    fn test_report(decision: ReviewDecision) -> ReviewerReport {
        let reasons = match decision {
            ReviewDecision::Approve => vec![ReviewReasonCode::PublicTechnicalContent],
            ReviewDecision::Reject | ReviewDecision::NeedsHumanReview => {
                vec![ReviewReasonCode::OtherPrivacyRisk]
            }
        };
        ReviewerReport::new(decision, reasons, "test summary").unwrap()
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn analyzed(
        document_path: &str,
        markdown: &str,
        resolutions: Vec<Resolution>,
    ) -> AnalyzedMarkdown {
        let references = MarkdownReferenceParser::parse(markdown);
        assert_eq!(references.len(), resolutions.len());
        let references = references
            .into_iter()
            .zip(resolutions)
            .map(|(reference, resolution)| ResolvedReference::new(reference, resolution))
            .collect();
        let file = SnapshotFile::new(
            path(document_path),
            markdown.len() as u64,
            Sha256::digest(markdown.as_bytes()),
            None,
        );

        AnalyzedMarkdown::with_frontmatter(
            file,
            MarkdownFrontmatterParser::parse(markdown),
            references,
        )
    }

    fn public_candidates(documents: Vec<AnalyzedMarkdown>) -> Vec<PublicCandidateMarkdown> {
        let filtered = PrivacyFilter::filter(documents);
        assert!(filtered.private_documents().is_empty());
        assert!(filtered.invalid_documents().is_empty());
        filtered.into_public_candidates()
    }

    fn evaluate_one(
        response: Result<ReviewDecision, ReviewerError>,
    ) -> (PublicPolicyOutcome, usize) {
        let reviewer = MockReviewer::returning(response);
        let outcome = PublicPolicy::evaluate(
            public_candidates(vec![analyzed("article.md", "body", vec![])]),
            &reviewer,
        )
        .pop()
        .unwrap();
        (outcome, reviewer.calls())
    }

    #[test]
    fn program_pass_and_approve_produces_approved_outcome() {
        let (outcome, calls) = evaluate_one(Ok(ReviewDecision::Approve));

        assert_eq!(outcome.decision(), &PublicPolicyDecision::ReviewApproved);
        assert_eq!(
            outcome.reviewer_report().unwrap().decision(),
            ReviewDecision::Approve
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn reviewer_report_json_is_strict_and_semantically_validated() {
        for valid in [
            r#"{"decision":"approve","reason_codes":["public_technical_content"],"summary":"公开技术内容。"}"#,
            r#"{"decision":"reject","reason_codes":["credential_secret"],"summary":"文档包含疑似真实访问凭据。"}"#,
            r#"{"decision":"needs_human_review","reason_codes":["uncertain_disclosure_authorization"],"summary":"公开授权无法确认。"}"#,
        ] {
            assert!(serde_json::from_str::<ReviewerReport>(valid).is_ok());
        }

        let too_long = "a".repeat(crate::policy::MAX_REVIEW_SUMMARY_CHARS + 1);
        let invalid = [
            r#"{"decision":"probably_safe","reason_codes":[],"summary":"unknown decision"}"#.to_owned(),
            r#"{"decision":"approve","reason_codes":["unknown_reason"],"summary":"unknown reason"}"#.to_owned(),
            r#"{"decision":"approve","reason_codes":[],"summary":"safe","extra":true}"#.to_owned(),
            r#"{"decision":"approve","summary":"missing field"}"#.to_owned(),
            format!(r#"{{"decision":"approve","reason_codes":[],"summary":"{too_long}"}}"#),
            r#"{"decision":"reject","reason_codes":[],"summary":"missing risk reason"}"#.to_owned(),
            r#"{"decision":"approve","reason_codes":["credential_secret"],"summary":"contradiction"}"#.to_owned(),
            r#"{"decision":"needs_human_review","reason_codes":["ordinary_work_experience"],"summary":"no risk reason"}"#.to_owned(),
            r#"{"decision":"approve","reason_codes":[],"summary":"detected sk-prod-abc123"}"#.to_owned(),
            r#"{"decision":"approve","reason_codes":[],"summary":"联系电话 13800138000"}"#.to_owned(),
        ];
        for json in invalid {
            assert!(
                serde_json::from_str::<ReviewerReport>(&json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn reviewer_report_schema_exposes_the_strict_core_contract() {
        let schema = serde_json::to_value(schemars::schema_for!(ReviewerReport)).unwrap();
        let root = schema.as_object().unwrap();
        assert_eq!(
            root.get("additionalProperties"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            root.get("required"),
            Some(&serde_json::json!(["decision", "reason_codes", "summary"]))
        );
        let definitions = root
            .get("$defs")
            .and_then(|value| value.as_object())
            .unwrap();
        assert_eq!(
            definitions["ReviewDecision"]["enum"],
            serde_json::json!(["approve", "reject", "needs_human_review"])
        );
        assert!(
            definitions["ReviewReasonCode"]["enum"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("credential_secret"))
        );
    }

    #[test]
    fn program_pass_and_reject_produces_rejected_outcome() {
        let (outcome, calls) = evaluate_one(Ok(ReviewDecision::Reject));

        assert_eq!(outcome.decision(), &PublicPolicyDecision::ReviewRejected);
        assert_eq!(calls, 1);
    }

    #[test]
    fn program_pass_can_require_human_review() {
        let (outcome, calls) = evaluate_one(Ok(ReviewDecision::NeedsHumanReview));

        assert_eq!(
            outcome.decision(),
            &PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested)
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn reviewer_error_fails_closed() {
        let error = ReviewerError::new("provider unavailable");
        let (outcome, calls) = evaluate_one(Err(error.clone()));

        assert_eq!(
            outcome.decision(),
            &PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(error))
        );
        assert_ne!(outcome.decision(), &PublicPolicyDecision::ReviewApproved);
        assert_eq!(calls, 1);
    }

    #[test]
    fn reviewer_error_keeps_typed_categories_and_reads_legacy_string_records() {
        let typed = ReviewerError::with_kind(ReviewerErrorKind::Timeout, "provider timed out");
        let round_trip: ReviewerError =
            serde_json::from_str(&serde_json::to_string(&typed).unwrap()).unwrap();
        let legacy: ReviewerError = serde_json::from_str(r#""provider unavailable""#).unwrap();

        assert_eq!(round_trip, typed);
        assert_eq!(legacy.kind(), ReviewerErrorKind::Other);
        assert_eq!(legacy.message(), "provider unavailable");
    }

    #[test]
    fn reviewer_error_round_trips_safe_http_status_details() {
        let error = ReviewerError::with_provider_error(
            ReviewerErrorKind::HttpStatus,
            400,
            "provider returned an HTTP status",
            Some("invalid_model".to_owned()),
            Some("The requested model is unavailable.".to_owned()),
        );

        let round_trip: ReviewerError =
            serde_json::from_str(&serde_json::to_string(&error).unwrap()).unwrap();

        assert_eq!(round_trip, error);
        assert_eq!(round_trip.http_status(), Some(400));
        assert_eq!(round_trip.provider_error_code(), Some("invalid_model"));
    }

    #[test]
    fn program_issues_do_not_call_reviewer() {
        let reviewer = MockReviewer::with_responses([]);
        let outcomes = PublicPolicy::evaluate(
            public_candidates(vec![analyzed(
                "article.md",
                "![[missing.png]]",
                vec![Resolution::Missing {
                    target: "missing.png".to_owned(),
                }],
            )]),
            &reviewer,
        );

        assert!(matches!(
            outcomes[0].decision(),
            PublicPolicyDecision::ProgramIssues(issues)
                if issues.len() == 1 && issues[0].document_path() == &path("article.md")
        ));
        assert_eq!(reviewer.calls(), 0);
    }

    #[test]
    fn private_and_invalid_privacy_documents_never_reach_reviewer() {
        let filtered = PrivacyFilter::filter(vec![
            analyzed("private.md", "body", vec![]),
            analyzed("invalid.md", "---\nprivate: maybe\n---\nbody", vec![]),
            analyzed("article.md", "body", vec![]),
        ]);
        assert_eq!(filtered.private_documents().len(), 1);
        assert_eq!(filtered.invalid_documents().len(), 1);

        let reviewer = MockReviewer::returning(Ok(ReviewDecision::Approve));
        let outcomes = PublicPolicy::evaluate(filtered.into_public_candidates(), &reviewer);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].path(), &path("article.md"));
        assert_eq!(reviewer.calls(), 1);
    }

    #[test]
    fn outcomes_are_sorted_by_content_path_independent_of_input_order() {
        fn candidates(reverse: bool) -> Vec<PublicCandidateMarkdown> {
            let mut documents = public_candidates(vec![
                analyzed("c.md", "body", vec![]),
                analyzed("a.md", "body", vec![]),
                analyzed("b.md", "body", vec![]),
            ]);
            if reverse {
                documents.reverse();
            }
            documents
        }

        let responses = || {
            [
                ("a.md", Ok(ReviewDecision::Approve)),
                ("b.md", Ok(ReviewDecision::Reject)),
                ("c.md", Ok(ReviewDecision::NeedsHumanReview)),
            ]
        };
        let first = PublicPolicy::evaluate(
            candidates(false),
            &MockReviewer::with_responses(responses()),
        );
        let second =
            PublicPolicy::evaluate(candidates(true), &MockReviewer::with_responses(responses()));

        assert_eq!(first, second);
        assert_eq!(
            first
                .iter()
                .map(|outcome| outcome.path().as_str())
                .collect::<Vec<_>>(),
            ["a.md", "b.md", "c.md"]
        );
    }
}
