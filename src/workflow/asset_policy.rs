use std::{cell::RefCell, collections::BTreeMap, error::Error, fmt};

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
            .map(|outcome| {
                let disposition = match outcome.decision {
                    AssetPolicyDecision::Blocked => AssetReviewDisposition::Blocked,
                    AssetPolicyDecision::NeedsHumanReview => {
                        AssetReviewDisposition::NeedsHumanReview(
                            AssetHumanReviewReason::PolicyFindings,
                        )
                    }
                    AssetPolicyDecision::ReadyForAssetReview => {
                        let candidate = outcome
                            .review_candidate
                            .as_ref()
                            .expect("ready asset policy outcome must contain a candidate");
                        match reviewer.review(candidate) {
                            Ok(decision) => AssetReviewDisposition::Reviewed(decision),
                            Err(error) => AssetReviewDisposition::NeedsHumanReview(
                                AssetHumanReviewReason::ReviewerFailed(error),
                            ),
                        }
                    }
                };

                AssetReviewOutcome {
                    path: outcome.path().clone(),
                    dependents: outcome.dependents().to_vec(),
                    sha256: outcome.checked_asset.sha256(),
                    actual_type: outcome.checked_asset.actual_type().clone(),
                    size: outcome.checked_asset.size(),
                    image_dimensions: outcome.checked_asset.image_dimensions(),
                    findings: outcome.findings().to_vec(),
                    disposition,
                }
            })
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
        ActualAssetType::Image { .. } if asset.image_dimensions().is_some() => {
            AssetPolicyDecision::ReadyForAssetReview
        }
        ActualAssetType::Pdf => AssetPolicyDecision::ReadyForAssetReview,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetReviewDecision {
    Approve,
    Reject,
    NeedsHumanReview,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewerError {
    message: String,
}

impl AssetReviewerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AssetReviewerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AssetReviewerError {}

/// Reviews only assets that crossed the deterministic Asset Policy boundary.
pub trait AssetReviewer {
    fn review(
        &self,
        candidate: &AssetReviewCandidate,
    ) -> Result<AssetReviewDecision, AssetReviewerError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetHumanReviewReason {
    PolicyFindings,
    ReviewerFailed(AssetReviewerError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetReviewDisposition {
    Blocked,
    Reviewed(AssetReviewDecision),
    NeedsHumanReview(AssetHumanReviewReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewOutcome {
    path: ContentPath,
    dependents: Vec<ContentPath>,
    sha256: Option<Sha256>,
    actual_type: ActualAssetType,
    size: Option<u64>,
    image_dimensions: Option<ImageDimensions>,
    findings: Vec<AssetCheckFinding>,
    disposition: AssetReviewDisposition,
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
    ) -> Result<AssetReviewDecision, AssetReviewerError> {
        self.reviewed_paths
            .borrow_mut()
            .push(candidate.path.clone());
        self.responses
            .get(candidate.path())
            .unwrap_or(&self.default_response)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
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
