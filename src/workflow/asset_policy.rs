use std::{cell::RefCell, collections::BTreeMap, error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{ContentPath, Sha256, SnapshotId};

use super::{ActualAssetType, AssetCheckFinding, AssetCheckResult, CheckedAsset, ImageDimensions};

/// The deterministic Asset Policy result before any semantic asset review.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetPolicyDecision {
    Blocked,
    NeedsHumanReview,
    ReadyForAssetReview,
}

/// A structurally safe asset that may cross the AssetReviewer boundary.
///
/// This type has no public constructor. Asset Policy creates it only from a
/// completed `AssetCheckResult`; reviewers cannot receive unchecked assets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewCandidate {
    path: ContentPath,
    dependents: Vec<ContentPath>,
    sha256: Sha256,
    actual_type: ActualAssetType,
    size: u64,
    image_dimensions: Option<ImageDimensions>,
    findings: Vec<AssetCheckFinding>,
}

impl AssetReviewCandidate {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn dependents(&self) -> &[ContentPath] {
        &self.dependents
    }

    pub fn sha256(&self) -> Sha256 {
        self.sha256
    }

    pub fn actual_type(&self) -> &ActualAssetType {
        &self.actual_type
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn image_dimensions(&self) -> Option<ImageDimensions> {
        self.image_dimensions
    }

    /// Includes non-structural metadata findings such as EXIF, GPS, and XMP.
    pub fn findings(&self) -> &[AssetCheckFinding] {
        &self.findings
    }
}

/// Asset Policy result for one checked asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPolicyOutcome {
    checked_asset: CheckedAsset,
    decision: AssetPolicyDecision,
    review_candidate: Option<AssetReviewCandidate>,
}

impl AssetPolicyOutcome {
    pub fn path(&self) -> &ContentPath {
        self.checked_asset.path()
    }

    pub fn dependents(&self) -> &[ContentPath] {
        self.checked_asset.dependents()
    }

    pub fn findings(&self) -> &[AssetCheckFinding] {
        self.checked_asset.findings()
    }

    pub fn decision(&self) -> AssetPolicyDecision {
        self.decision
    }

    pub fn review_candidate(&self) -> Option<&AssetReviewCandidate> {
        self.review_candidate.as_ref()
    }

    /// Applies the already-made policy decision and calls a reviewer only for a
    /// policy-created candidate. This keeps the reviewer boundary impossible to
    /// bypass while allowing orchestration to durably persist each outcome
    /// before moving to the next asset.
    pub fn review<R: AssetReviewer + ?Sized>(&self, reviewer: &R) -> AssetReviewOutcome {
        let mut reviewer_report = None;
        let disposition = match self.decision {
            AssetPolicyDecision::Blocked => AssetReviewDisposition::Blocked,
            AssetPolicyDecision::NeedsHumanReview => {
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings)
            }
            AssetPolicyDecision::ReadyForAssetReview => {
                let candidate = self
                    .review_candidate
                    .as_ref()
                    .expect("ready asset policy outcome must contain a candidate");
                match reviewer.review(candidate) {
                    Ok(report) => {
                        let decision = report.decision();
                        reviewer_report = Some(report);
                        AssetReviewDisposition::Reviewed(decision)
                    }
                    Err(error) => AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::ReviewerFailed(error),
                    ),
                }
            }
        };

        AssetReviewOutcome {
            path: self.path().clone(),
            dependents: self.dependents().to_vec(),
            sha256: self.checked_asset.sha256(),
            actual_type: self.checked_asset.actual_type().clone(),
            size: self.checked_asset.size(),
            image_dimensions: self.checked_asset.image_dimensions(),
            findings: self.findings().to_vec(),
            disposition,
            reviewer_report,
        }
    }
}

/// Complete deterministic Asset Policy output for one immutable Snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPolicyResult {
    snapshot_id: SnapshotId,
    outcomes: Vec<AssetPolicyOutcome>,
}

impl AssetPolicyResult {
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn outcomes(&self) -> &[AssetPolicyOutcome] {
        &self.outcomes
    }

    /// Sends only `ReadyForAssetReview` candidates to the reviewer.
    pub fn review<R: AssetReviewer + ?Sized>(&self, reviewer: &R) -> AssetReviewResult {
        let outcomes = self
            .outcomes
            .iter()
            .map(|outcome| outcome.review(reviewer))
            .collect();

        AssetReviewResult {
            snapshot_id: self.snapshot_id,
            outcomes,
        }
    }
}

/// Classifies completed deterministic checks without re-running them.
pub struct AssetPolicy;

impl AssetPolicy {
    pub fn evaluate(check_result: &AssetCheckResult) -> AssetPolicyResult {
        let mut checked_assets = check_result.assets().to_vec();
        checked_assets.sort_by(|left, right| left.path().cmp(right.path()));

        let outcomes =
            checked_assets
                .into_iter()
                .map(|checked_asset| {
                    let decision = classify(&checked_asset);
                    let review_candidate = (decision == AssetPolicyDecision::ReadyForAssetReview)
                        .then(|| AssetReviewCandidate {
                            path: checked_asset.path().clone(),
                            dependents: checked_asset.dependents().to_vec(),
                            sha256: checked_asset
                                .sha256()
                                .expect("ready asset must have a content identity"),
                            actual_type: checked_asset.actual_type().clone(),
                            size: checked_asset.size().expect("ready asset must have a size"),
                            image_dimensions: checked_asset.image_dimensions(),
                            findings: checked_asset.findings().to_vec(),
                        });

                    AssetPolicyOutcome {
                        checked_asset,
                        decision,
                        review_candidate,
                    }
                })
                .collect();

        AssetPolicyResult {
            snapshot_id: check_result.snapshot_id(),
            outcomes,
        }
    }
}

fn classify(asset: &CheckedAsset) -> AssetPolicyDecision {
    if asset
        .findings()
        .iter()
        .any(|finding| finding_effect(finding) == FindingEffect::Block)
        || asset.sha256().is_none()
        || asset.size().is_none()
    {
        return AssetPolicyDecision::Blocked;
    }

    if asset
        .findings()
        .iter()
        .any(|finding| finding_effect(finding) == FindingEffect::NeedsHumanReview)
    {
        return AssetPolicyDecision::NeedsHumanReview;
    }

    match asset.actual_type() {
        ActualAssetType::Image { media_type, .. }
            if asset.image_dimensions().is_some()
                && matches!(media_type.as_str(), "image/jpeg" | "image/png") =>
        {
            AssetPolicyDecision::ReadyForAssetReview
        }
        ActualAssetType::Pdf => AssetPolicyDecision::NeedsHumanReview,
        ActualAssetType::Image { .. }
        | ActualAssetType::OtherBinary { .. }
        | ActualAssetType::Unknown => AssetPolicyDecision::NeedsHumanReview,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FindingEffect {
    Block,
    NeedsHumanReview,
    PreserveForReview,
}

/// Deliberately exhaustive so a new program-check finding cannot silently
/// become reviewable without an explicit policy decision.
fn finding_effect(finding: &AssetCheckFinding) -> FindingEffect {
    match finding {
        AssetCheckFinding::SnapshotFileMissing
        | AssetCheckFinding::SnapshotFileIsMarkdown
        | AssetCheckFinding::MissingBlob { .. }
        | AssetCheckFinding::CorruptBlob { .. }
        | AssetCheckFinding::SizeMismatch { .. }
        | AssetCheckFinding::DecodeFailed => FindingEffect::Block,
        AssetCheckFinding::ExtensionContentMismatch { .. }
        | AssetCheckFinding::MetadataInspectionFailed
        | AssetCheckFinding::UnsupportedType
        | AssetCheckFinding::UnknownType => FindingEffect::NeedsHumanReview,
        AssetCheckFinding::ExifMetadataPresent
        | AssetCheckFinding::GpsMetadataPresent
        | AssetCheckFinding::XmpMetadataPresent => FindingEffect::PreserveForReview,
    }
}

/// Provider-independent semantic review decision.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetReviewDecision {
    Approve,
    Reject,
    NeedsHumanReview,
}

impl<'de> Deserialize<'de> for AssetReviewDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "approve" | "Approve" => Ok(Self::Approve),
            "reject" | "Reject" => Ok(Self::Reject),
            "needs_human_review" | "NeedsHumanReview" => Ok(Self::NeedsHumanReview),
            _ => Err(serde::de::Error::custom("unknown asset review decision")),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetReviewReasonCode {
    OrdinaryVisualContent,
    OrdinaryPersonalPhoto,
    PublicTechnicalVisual,
    VisibleCredentialSecret,
    VisiblePrivateContactInformation,
    VisiblePrivateIdentityInformation,
    VisiblePrivateCorrespondence,
    VisibleThirdPartyPrivateInformation,
    VisibleHomeOrPreciseLocationInformation,
    InternalWorkInformation,
    ConfidentialWorkMaterial,
    SecuritySensitiveInformation,
    UncertainVisualDisclosureAuthorization,
    OtherVisualPrivacyRisk,
}

impl AssetReviewReasonCode {
    pub fn is_risk(self) -> bool {
        !matches!(
            self,
            Self::OrdinaryVisualContent | Self::OrdinaryPersonalPhoto | Self::PublicTechnicalVisual
        )
    }
}

pub const MAX_ASSET_REVIEW_SUMMARY_CHARS: usize = crate::policy::MAX_REVIEW_SUMMARY_CHARS;

/// Structured visual-review result. Explanatory fields never grant publication authority.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetReviewerReport {
    decision: AssetReviewDecision,
    reason_codes: Vec<AssetReviewReasonCode>,
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedAssetReviewerReport {
    decision: AssetReviewDecision,
    reason_codes: Vec<AssetReviewReasonCode>,
    summary: String,
}

impl<'de> Deserialize<'de> for AssetReviewerReport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let report = UncheckedAssetReviewerReport::deserialize(deserializer)?;
        Self::new(report.decision, report.reason_codes, report.summary)
            .map_err(serde::de::Error::custom)
    }
}

impl AssetReviewerReport {
    pub fn new(
        decision: AssetReviewDecision,
        reason_codes: Vec<AssetReviewReasonCode>,
        summary: impl Into<String>,
    ) -> Result<Self, AssetReviewerReportError> {
        let report = Self {
            decision,
            reason_codes,
            summary: summary.into(),
        };
        report.validate()?;
        Ok(report)
    }

    pub fn decision(&self) -> AssetReviewDecision {
        self.decision
    }

    pub fn reason_codes(&self) -> &[AssetReviewReasonCode] {
        &self.reason_codes
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn validate(&self) -> Result<(), AssetReviewerReportError> {
        if self.reason_codes.len() > 8 {
            return Err(AssetReviewerReportError::TooManyReasonCodes);
        }
        for (index, reason) in self.reason_codes.iter().enumerate() {
            if self.reason_codes[..index].contains(reason) {
                return Err(AssetReviewerReportError::DuplicateReasonCode);
            }
        }
        let has_risk = self.reason_codes.iter().any(|reason| reason.is_risk());
        match self.decision {
            AssetReviewDecision::Approve if has_risk => {
                return Err(AssetReviewerReportError::ApproveWithRiskReason);
            }
            AssetReviewDecision::Reject | AssetReviewDecision::NeedsHumanReview if !has_risk => {
                return Err(AssetReviewerReportError::RiskDecisionWithoutRiskReason);
            }
            _ => {}
        }
        crate::policy::validate_review_summary(&self.summary)
            .map_err(AssetReviewerReportError::UnsafeSummary)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetReviewerReportError {
    TooManyReasonCodes,
    DuplicateReasonCode,
    ApproveWithRiskReason,
    RiskDecisionWithoutRiskReason,
    UnsafeSummary(crate::policy::ReviewSummaryError),
}

impl fmt::Display for AssetReviewerReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyReasonCodes => {
                formatter.write_str("asset reviewer report contains too many reason codes")
            }
            Self::DuplicateReasonCode => {
                formatter.write_str("asset reviewer report contains a duplicate reason code")
            }
            Self::ApproveWithRiskReason => {
                formatter.write_str("asset approve report contains a risk reason")
            }
            Self::RiskDecisionWithoutRiskReason => {
                formatter.write_str("asset reject or human-review report requires a risk reason")
            }
            Self::UnsafeSummary(error) => {
                write!(formatter, "invalid asset review summary: {error}")
            }
        }
    }
}

impl Error for AssetReviewerReportError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AssetReviewerError {
    kind: AssetReviewerErrorKind,
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
enum AssetReviewerErrorRepresentation {
    Legacy(String),
    Structured {
        #[serde(default)]
        kind: AssetReviewerErrorKind,
        message: String,
        #[serde(default)]
        http_status: Option<u16>,
        #[serde(default)]
        provider_error_code: Option<String>,
        #[serde(default)]
        provider_error_message: Option<String>,
    },
}

impl<'de> Deserialize<'de> for AssetReviewerError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match AssetReviewerErrorRepresentation::deserialize(deserializer)? {
                AssetReviewerErrorRepresentation::Legacy(message) => Self::new(message),
                AssetReviewerErrorRepresentation::Structured {
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

impl AssetReviewerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_kind(AssetReviewerErrorKind::Other, message)
    }

    pub fn with_kind(kind: AssetReviewerErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
            provider_error_code: None,
            provider_error_message: None,
        }
    }

    pub fn with_provider_error(
        kind: AssetReviewerErrorKind,
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

    pub fn kind(&self) -> AssetReviewerErrorKind {
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

impl fmt::Display for AssetReviewerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AssetReviewerError {}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetReviewerErrorKind {
    ContentStore,
    InputTooLarge,
    UnsupportedFormat,
    Transport,
    Timeout,
    Authentication,
    HttpStatus,
    ResponseTooLarge,
    EmptyResponse,
    MalformedResponse,
    InvalidReport,
    TruncatedResponse,
    #[default]
    Other,
}

/// Reviews only assets that crossed the deterministic Asset Policy boundary.
pub trait AssetReviewer {
    fn review(
        &self,
        candidate: &AssetReviewCandidate,
    ) -> Result<AssetReviewerReport, AssetReviewerError>;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AssetHumanReviewReason {
    PolicyFindings,
    ReviewerFailed(AssetReviewerError),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AssetReviewDisposition {
    Blocked,
    Reviewed(AssetReviewDecision),
    NeedsHumanReview(AssetHumanReviewReason),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AssetReviewOutcome {
    path: ContentPath,
    dependents: Vec<ContentPath>,
    sha256: Option<Sha256>,
    actual_type: ActualAssetType,
    size: Option<u64>,
    image_dimensions: Option<ImageDimensions>,
    findings: Vec<AssetCheckFinding>,
    disposition: AssetReviewDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reviewer_report: Option<AssetReviewerReport>,
}

impl AssetReviewOutcome {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn dependents(&self) -> &[ContentPath] {
        &self.dependents
    }

    pub fn sha256(&self) -> Option<Sha256> {
        self.sha256
    }

    pub fn actual_type(&self) -> &ActualAssetType {
        &self.actual_type
    }

    pub fn size(&self) -> Option<u64> {
        self.size
    }

    pub fn image_dimensions(&self) -> Option<ImageDimensions> {
        self.image_dimensions
    }

    pub fn findings(&self) -> &[AssetCheckFinding] {
        &self.findings
    }

    pub fn disposition(&self) -> &AssetReviewDisposition {
        &self.disposition
    }

    pub fn reviewer_report(&self) -> Option<&AssetReviewerReport> {
        self.reviewer_report.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        path: ContentPath,
        dependents: Vec<ContentPath>,
        sha256: Option<Sha256>,
        findings: Vec<AssetCheckFinding>,
        disposition: AssetReviewDisposition,
    ) -> Self {
        Self {
            path,
            dependents,
            sha256,
            actual_type: ActualAssetType::Unknown,
            size: None,
            image_dimensions: None,
            findings,
            disposition,
            reviewer_report: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewResult {
    snapshot_id: SnapshotId,
    outcomes: Vec<AssetReviewOutcome>,
}

impl AssetReviewResult {
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn outcomes(&self) -> &[AssetReviewOutcome] {
        &self.outcomes
    }
}

/// Deterministic reviewer adapter for tests and local workflow development.
pub struct MockAssetReviewer {
    default_response: Result<AssetReviewDecision, AssetReviewerError>,
    responses: BTreeMap<ContentPath, Result<AssetReviewDecision, AssetReviewerError>>,
    reviewed_paths: RefCell<Vec<ContentPath>>,
}

impl MockAssetReviewer {
    pub fn returning(response: Result<AssetReviewDecision, AssetReviewerError>) -> Self {
        Self {
            default_response: response,
            responses: BTreeMap::new(),
            reviewed_paths: RefCell::new(Vec::new()),
        }
    }

    pub fn with_responses(
        default_response: Result<AssetReviewDecision, AssetReviewerError>,
        responses: impl IntoIterator<
            Item = (ContentPath, Result<AssetReviewDecision, AssetReviewerError>),
        >,
    ) -> Self {
        Self {
            default_response,
            responses: responses.into_iter().collect(),
            reviewed_paths: RefCell::new(Vec::new()),
        }
    }

    pub fn reviewed_paths(&self) -> Vec<ContentPath> {
        self.reviewed_paths.borrow().clone()
    }
}

impl AssetReviewer for MockAssetReviewer {
    fn review(
        &self,
        candidate: &AssetReviewCandidate,
    ) -> Result<AssetReviewerReport, AssetReviewerError> {
        self.reviewed_paths
            .borrow_mut()
            .push(candidate.path.clone());
        self.responses
            .get(candidate.path())
            .unwrap_or(&self.default_response)
            .clone()
            .map(test_report)
    }
}

fn test_report(decision: AssetReviewDecision) -> AssetReviewerReport {
    let reasons = match decision {
        AssetReviewDecision::Approve => vec![AssetReviewReasonCode::OrdinaryVisualContent],
        AssetReviewDecision::Reject => vec![AssetReviewReasonCode::OtherVisualPrivacyRisk],
        AssetReviewDecision::NeedsHumanReview => {
            vec![AssetReviewReasonCode::UncertainVisualDisclosureAuthorization]
        }
    };
    AssetReviewerReport::new(decision, reasons, "test visual classification").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    #[test]
    fn asset_reviewer_report_json_is_strict_and_semantically_validated() {
        let valid = [
            r#"{"decision":"approve","reason_codes":["ordinary_visual_content"],"summary":"Ordinary public scene."}"#,
            r#"{"decision":"needs_human_review","reason_codes":["uncertain_visual_disclosure_authorization"],"summary":"A concrete authorization question remains."}"#,
            r#"{"decision":"reject","reason_codes":["visible_credential_secret"],"summary":"A likely access credential is visible."}"#,
        ];
        for value in valid {
            assert!(serde_json::from_str::<AssetReviewerReport>(value).is_ok());
        }

        let invalid = [
            r#"{"decision":"unknown","reason_codes":["ordinary_visual_content"],"summary":"x"}"#,
            r#"{"decision":"approve","reason_codes":["unknown_reason"],"summary":"x"}"#,
            r#"{"decision":"approve","reason_codes":["ordinary_visual_content","ordinary_visual_content"],"summary":"x"}"#,
            r#"{"decision":"approve","reason_codes":["ordinary_visual_content"],"summary":"x","extra":true}"#,
            r#"{"decision":"approve","reason_codes":["ordinary_visual_content"]}"#,
            r#"{"decision":"approve","reason_codes":["visible_credential_secret"],"summary":"x"}"#,
            r#"{"decision":"reject","reason_codes":[],"summary":"x"}"#,
            r#"{"decision":"needs_human_review","reason_codes":[],"summary":"x"}"#,
            r#"{"decision":"reject","reason_codes":["visible_credential_secret"],"summary":"The token is sk-prod-123456789."}"#,
        ];
        for value in invalid {
            assert!(
                serde_json::from_str::<AssetReviewerReport>(value).is_err(),
                "{value}"
            );
        }
        let too_long = "a".repeat(MAX_ASSET_REVIEW_SUMMARY_CHARS + 1);
        assert!(
            AssetReviewerReport::new(
                AssetReviewDecision::Approve,
                vec![AssetReviewReasonCode::OrdinaryVisualContent],
                too_long,
            )
            .is_err()
        );
    }

    #[test]
    fn provider_capability_does_not_expand_jpeg_png_publication_boundary() {
        for (media_type, extension) in [("image/gif", "gif"), ("image/webp", "webp")] {
            let asset = CheckedAsset::new(
                path(&format!("image.{extension}")),
                vec![path("a.md")],
                Some(Sha256::digest(b"image")),
                ActualAssetType::Image {
                    media_type: media_type.to_owned(),
                    extension: extension.to_owned(),
                },
                Some(5),
                Some(ImageDimensions::new(1, 1)),
                vec![],
            );
            let policy = AssetPolicy::evaluate(&result(vec![asset]));
            let reviewer = MockAssetReviewer::returning(Ok(AssetReviewDecision::Approve));
            assert_eq!(
                policy.outcomes()[0].decision(),
                AssetPolicyDecision::NeedsHumanReview
            );
            assert!(policy.outcomes()[0].review_candidate().is_none());
            let _ = policy.review(&reviewer);
            assert!(reviewer.reviewed_paths().is_empty());
        }

        let pdf = CheckedAsset::new(
            path("document.pdf"),
            vec![path("a.md")],
            Some(Sha256::digest(b"%PDF-1.7")),
            ActualAssetType::Pdf,
            Some(8),
            None,
            vec![],
        );
        let policy = AssetPolicy::evaluate(&result(vec![pdf]));
        let reviewer = MockAssetReviewer::returning(Ok(AssetReviewDecision::Approve));
        let _ = policy.review(&reviewer);
        assert_eq!(
            policy.outcomes()[0].decision(),
            AssetPolicyDecision::NeedsHumanReview
        );
        assert!(reviewer.reviewed_paths().is_empty());
    }

    fn image(path_value: &str, findings: Vec<AssetCheckFinding>) -> CheckedAsset {
        image_with_dependents(path_value, vec![path("a.md")], findings)
    }

    fn image_with_dependents(
        path_value: &str,
        dependents: Vec<ContentPath>,
        findings: Vec<AssetCheckFinding>,
    ) -> CheckedAsset {
        CheckedAsset::new(
            path(path_value),
            dependents,
            Some(Sha256::digest(path_value.as_bytes())),
            ActualAssetType::Image {
                media_type: "image/png".to_owned(),
                extension: "png".to_owned(),
            },
            Some(10),
            Some(ImageDimensions::new(2, 3)),
            findings,
        )
    }

    fn result(assets: Vec<CheckedAsset>) -> AssetCheckResult {
        AssetCheckResult::from_assets_for_test(SnapshotId::new(7).unwrap(), assets)
    }

    #[test]
    fn clean_image_calls_reviewer() {
        let policy = AssetPolicy::evaluate(&result(vec![image("clean.png", vec![])]));
        let reviewer = MockAssetReviewer::returning(Ok(AssetReviewDecision::Approve));
        let reviewed = policy.review(&reviewer);

        assert_eq!(
            policy.outcomes()[0].decision(),
            AssetPolicyDecision::ReadyForAssetReview
        );
        assert_eq!(reviewer.reviewed_paths(), vec![path("clean.png")]);
        assert_eq!(
            reviewed.outcomes()[0].disposition(),
            &AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve)
        );
    }

    #[test]
    fn broken_image_is_blocked_without_calling_reviewer() {
        let policy = AssetPolicy::evaluate(&result(vec![image(
            "broken.png",
            vec![AssetCheckFinding::DecodeFailed],
        )]));
        let reviewer = MockAssetReviewer::returning(Ok(AssetReviewDecision::Approve));
        let reviewed = policy.review(&reviewer);

        assert_eq!(
            policy.outcomes()[0].decision(),
            AssetPolicyDecision::Blocked
        );
        assert!(reviewer.reviewed_paths().is_empty());
        assert_eq!(
            reviewed.outcomes()[0].disposition(),
            &AssetReviewDisposition::Blocked
        );
    }

    #[test]
    fn unsupported_and_unknown_types_fail_closed_without_review() {
        let unsupported = CheckedAsset::new(
            path("archive.zip"),
            vec![path("a.md")],
            Some(Sha256::digest(b"zip")),
            ActualAssetType::OtherBinary {
                media_type: "application/zip".to_owned(),
                extension: "zip".to_owned(),
            },
            Some(3),
            None,
            vec![AssetCheckFinding::UnsupportedType],
        );
        let unknown = CheckedAsset::new(
            path("unknown.bin"),
            vec![path("b.md")],
            Some(Sha256::digest(b"unknown")),
            ActualAssetType::Unknown,
            Some(7),
            None,
            vec![AssetCheckFinding::UnknownType],
        );
        let policy = AssetPolicy::evaluate(&result(vec![unsupported, unknown]));
        let reviewer = MockAssetReviewer::returning(Ok(AssetReviewDecision::Approve));
        let reviewed = policy.review(&reviewer);

        assert!(policy.outcomes().iter().all(|outcome| {
            outcome.decision() == AssetPolicyDecision::NeedsHumanReview
                && outcome.review_candidate().is_none()
        }));
        assert!(reviewer.reviewed_paths().is_empty());
        assert!(reviewed.outcomes().iter().all(|outcome| matches!(
            outcome.disposition(),
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings)
        )));
    }

    #[test]
    fn metadata_findings_are_preserved_through_candidate_and_outcome() {
        let findings = vec![
            AssetCheckFinding::ExifMetadataPresent,
            AssetCheckFinding::GpsMetadataPresent,
            AssetCheckFinding::XmpMetadataPresent,
        ];
        let policy = AssetPolicy::evaluate(&result(vec![image("metadata.png", findings.clone())]));
        let candidate = policy.outcomes()[0].review_candidate().unwrap();
        assert_eq!(candidate.findings(), findings);

        let reviewed = policy.review(&MockAssetReviewer::returning(Ok(
            AssetReviewDecision::NeedsHumanReview,
        )));
        assert_eq!(reviewed.outcomes()[0].findings(), findings);
    }

    #[test]
    fn reviewer_approve_reject_human_and_error_are_explicit() {
        let assets = vec![
            image("approve.png", vec![]),
            image("error.png", vec![]),
            image("human.png", vec![]),
            image("reject.png", vec![]),
        ];
        let reviewer_error = AssetReviewerError::new("mock unavailable");
        let reviewer = MockAssetReviewer::with_responses(
            Ok(AssetReviewDecision::Approve),
            [
                (path("error.png"), Err(reviewer_error.clone())),
                (path("human.png"), Ok(AssetReviewDecision::NeedsHumanReview)),
                (path("reject.png"), Ok(AssetReviewDecision::Reject)),
            ],
        );
        let reviewed = AssetPolicy::evaluate(&result(assets)).review(&reviewer);
        let dispositions = reviewed
            .outcomes()
            .iter()
            .map(|outcome| (outcome.path().as_str(), outcome.disposition().clone()))
            .collect::<Vec<_>>();

        assert_eq!(
            dispositions,
            vec![
                (
                    "approve.png",
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
                ),
                (
                    "error.png",
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::ReviewerFailed(reviewer_error),
                    ),
                ),
                (
                    "human.png",
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview),
                ),
                (
                    "reject.png",
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject),
                ),
            ]
        );
    }

    #[test]
    fn shared_asset_is_reviewed_once_and_retains_all_dependents() {
        let shared = image_with_dependents(
            "shared.png",
            vec![path("a.md"), path("b.md"), path("z.md")],
            vec![],
        );
        let reviewer = MockAssetReviewer::returning(Ok(AssetReviewDecision::Approve));
        let reviewed = AssetPolicy::evaluate(&result(vec![shared])).review(&reviewer);

        assert_eq!(reviewer.reviewed_paths(), vec![path("shared.png")]);
        assert_eq!(
            reviewed.outcomes()[0].dependents(),
            &[path("a.md"), path("b.md"), path("z.md")]
        );
    }

    #[test]
    fn ordering_is_deterministic_independent_of_check_input() {
        let first = AssetPolicy::evaluate(&result(vec![
            image("z.png", vec![]),
            image("a.png", vec![]),
        ]));
        let second = AssetPolicy::evaluate(&result(vec![
            image("a.png", vec![]),
            image("z.png", vec![]),
        ]));

        assert_eq!(first, second);
        assert_eq!(
            first
                .outcomes()
                .iter()
                .map(|outcome| outcome.path().as_str())
                .collect::<Vec<_>>(),
            ["a.png", "z.png"]
        );
    }

    #[test]
    fn every_integrity_finding_blocks_review() {
        let findings = [
            AssetCheckFinding::SnapshotFileMissing,
            AssetCheckFinding::SnapshotFileIsMarkdown,
            AssetCheckFinding::MissingBlob {
                sha256: Sha256::digest(b"missing"),
            },
            AssetCheckFinding::CorruptBlob {
                expected: Sha256::digest(b"expected"),
                actual: Sha256::digest(b"actual"),
            },
            AssetCheckFinding::SizeMismatch {
                expected: 10,
                actual: 9,
            },
            AssetCheckFinding::DecodeFailed,
        ];

        for finding in findings {
            let policy = AssetPolicy::evaluate(&result(vec![image("asset.png", vec![finding])]));
            assert_eq!(
                policy.outcomes()[0].decision(),
                AssetPolicyDecision::Blocked
            );
            assert!(policy.outcomes()[0].review_candidate().is_none());
        }
    }
}
