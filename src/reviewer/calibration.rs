use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsStr,
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::policy::{ReviewDecision, ReviewReasonCode, ReviewerError, ReviewerReport};
use crate::workflow::{
    AssetReviewDecision, AssetReviewReasonCode, AssetReviewerError, AssetReviewerReport,
};

fn validate_expectation<D: PartialEq, R: PartialEq>(
    acceptable_decisions: &[D],
    expected_reason_codes: &[R],
    forbidden_reason_codes: &[R],
) -> Result<(), CalibrationExpectationError> {
    if acceptable_decisions.is_empty() {
        return Err(CalibrationExpectationError::NoAcceptableDecision);
    }
    if has_duplicates(acceptable_decisions) {
        return Err(CalibrationExpectationError::DuplicateAcceptableDecision);
    }
    if has_duplicates(expected_reason_codes) {
        return Err(CalibrationExpectationError::DuplicateExpectedReasonCode);
    }
    if has_duplicates(forbidden_reason_codes) {
        return Err(CalibrationExpectationError::DuplicateForbiddenReasonCode);
    }
    if expected_reason_codes
        .iter()
        .any(|reason| forbidden_reason_codes.contains(reason))
    {
        return Err(CalibrationExpectationError::ConflictingReasonCode);
    }
    Ok(())
}

struct CalibrationMatch<R> {
    decision_matches: bool,
    missing_reason_codes: Vec<R>,
    forbidden_reason_codes: Vec<R>,
}

fn match_observation<D: PartialEq, R: Copy + PartialEq>(
    acceptable_decisions: &[D],
    expected_reason_codes: &[R],
    forbidden_reason_codes: &[R],
    actual_decision: &D,
    actual_reason_codes: &[R],
) -> CalibrationMatch<R> {
    CalibrationMatch {
        decision_matches: acceptable_decisions.contains(actual_decision),
        missing_reason_codes: expected_reason_codes
            .iter()
            .copied()
            .filter(|reason| !actual_reason_codes.contains(reason))
            .collect(),
        forbidden_reason_codes: forbidden_reason_codes
            .iter()
            .copied()
            .filter(|reason| actual_reason_codes.contains(reason))
            .collect(),
    }
}

/// Human-authored reference criteria for one Markdown calibration fixture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CalibrationExpectation {
    acceptable_decisions: Vec<ReviewDecision>,
    #[serde(default)]
    expected_reason_codes: Vec<ReviewReasonCode>,
    #[serde(default)]
    forbidden_reason_codes: Vec<ReviewReasonCode>,
}

impl CalibrationExpectation {
    pub fn from_yaml(yaml: &str) -> Result<Self, CalibrationExpectationError> {
        let expectation: Self = serde_yaml_ng::from_str(yaml)
            .map_err(|error| CalibrationExpectationError::Yaml(error.to_string()))?;
        expectation.validate()?;
        Ok(expectation)
    }

    pub fn acceptable_decisions(&self) -> &[ReviewDecision] {
        &self.acceptable_decisions
    }

    pub fn expected_reason_codes(&self) -> &[ReviewReasonCode] {
        &self.expected_reason_codes
    }

    pub fn forbidden_reason_codes(&self) -> &[ReviewReasonCode] {
        &self.forbidden_reason_codes
    }

    fn validate(&self) -> Result<(), CalibrationExpectationError> {
        validate_expectation(
            &self.acceptable_decisions,
            &self.expected_reason_codes,
            &self.forbidden_reason_codes,
        )
    }
}

fn has_duplicates<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalibrationExpectationError {
    Yaml(String),
    NoAcceptableDecision,
    DuplicateAcceptableDecision,
    DuplicateExpectedReasonCode,
    DuplicateForbiddenReasonCode,
    ConflictingReasonCode,
}

impl fmt::Display for CalibrationExpectationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yaml(error) => write!(formatter, "invalid expectation YAML: {error}"),
            Self::NoAcceptableDecision => {
                formatter.write_str("at least one acceptable decision is required")
            }
            Self::DuplicateAcceptableDecision => {
                formatter.write_str("acceptable decisions contain a duplicate")
            }
            Self::DuplicateExpectedReasonCode => {
                formatter.write_str("expected reason codes contain a duplicate")
            }
            Self::DuplicateForbiddenReasonCode => {
                formatter.write_str("forbidden reason codes contain a duplicate")
            }
            Self::ConflictingReasonCode => {
                formatter.write_str("a reason code cannot be both expected and forbidden")
            }
        }
    }
}

impl Error for CalibrationExpectationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalibrationObservation {
    ReviewerReport(ReviewerReport),
    ProviderError(ReviewerError),
    BlockedBeforeReview(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalibrationCaseStatus {
    Pass {
        report: ReviewerReport,
    },
    Mismatch {
        report: ReviewerReport,
        decision_matches: bool,
        missing_reason_codes: Vec<ReviewReasonCode>,
        forbidden_reason_codes: Vec<ReviewReasonCode>,
    },
    ProviderError(ReviewerError),
    BlockedBeforeReview(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalibrationCaseResult {
    fixture: String,
    expectation: CalibrationExpectation,
    status: CalibrationCaseStatus,
}

impl CalibrationCaseResult {
    pub fn evaluate(
        fixture: impl Into<String>,
        expectation: CalibrationExpectation,
        observation: CalibrationObservation,
    ) -> Self {
        let status = match observation {
            CalibrationObservation::ReviewerReport(report) => {
                let matched = match_observation(
                    &expectation.acceptable_decisions,
                    &expectation.expected_reason_codes,
                    &expectation.forbidden_reason_codes,
                    &report.decision(),
                    report.reason_codes(),
                );
                let CalibrationMatch {
                    decision_matches,
                    missing_reason_codes,
                    forbidden_reason_codes,
                } = matched;
                if decision_matches
                    && missing_reason_codes.is_empty()
                    && forbidden_reason_codes.is_empty()
                {
                    CalibrationCaseStatus::Pass { report }
                } else {
                    CalibrationCaseStatus::Mismatch {
                        report,
                        decision_matches,
                        missing_reason_codes,
                        forbidden_reason_codes,
                    }
                }
            }
            CalibrationObservation::ProviderError(error) => {
                CalibrationCaseStatus::ProviderError(error)
            }
            CalibrationObservation::BlockedBeforeReview(reason) => {
                CalibrationCaseStatus::BlockedBeforeReview(reason)
            }
        };
        Self {
            fixture: fixture.into(),
            expectation,
            status,
        }
    }

    pub fn fixture(&self) -> &str {
        &self.fixture
    }

    pub fn status(&self) -> &CalibrationCaseStatus {
        &self.status
    }

    pub fn decision_matches(&self) -> Option<bool> {
        match &self.status {
            CalibrationCaseStatus::Pass { .. } => Some(true),
            CalibrationCaseStatus::Mismatch {
                decision_matches, ..
            } => Some(*decision_matches),
            CalibrationCaseStatus::ProviderError(_)
            | CalibrationCaseStatus::BlockedBeforeReview(_) => None,
        }
    }

    pub fn reason_matches(&self) -> Option<bool> {
        match &self.status {
            CalibrationCaseStatus::Pass { .. } => Some(true),
            CalibrationCaseStatus::Mismatch {
                missing_reason_codes,
                forbidden_reason_codes,
                ..
            } => Some(missing_reason_codes.is_empty() && forbidden_reason_codes.is_empty()),
            CalibrationCaseStatus::ProviderError(_)
            | CalibrationCaseStatus::BlockedBeforeReview(_) => None,
        }
    }

    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        match &self.status {
            CalibrationCaseStatus::Pass { report } => {
                lines.push(format!("[PASS] {}", self.fixture));
                self.render_report(report, &mut lines);
            }
            CalibrationCaseStatus::Mismatch {
                report,
                decision_matches: _,
                missing_reason_codes,
                forbidden_reason_codes,
            } => {
                lines.push(format!("[FAIL] {}", self.fixture));
                self.render_report(report, &mut lines);
                append_reasons("missing_reason", missing_reason_codes, &mut lines);
                append_reasons("forbidden_reason", forbidden_reason_codes, &mut lines);
            }
            CalibrationCaseStatus::ProviderError(error) => {
                lines.push(format!("[ERROR] {}", self.fixture));
                if let Some(status) = error.http_status() {
                    lines.push(format!("  reviewer_error: {:?}({status})", error.kind()));
                } else {
                    lines.push(format!("  reviewer_error: {:?}", error.kind()));
                }
                if let Some(code) = error.provider_error_code() {
                    lines.push(format!("  provider_error_code: {code}"));
                }
                if let Some(message) = error.provider_error_message() {
                    lines.push(format!("  provider_error_message: {message}"));
                }
                lines.push(format!("  message: {}", error.message()));
            }
            CalibrationCaseStatus::BlockedBeforeReview(reason) => {
                lines.push(format!(
                    "[SKIPPED / BLOCKED_BEFORE_REVIEW] {}",
                    self.fixture
                ));
                lines.push(format!("  reason: {reason}"));
            }
        }
        lines.join("\n")
    }

    fn render_report(&self, report: &ReviewerReport, lines: &mut Vec<String>) {
        let expected = self
            .expectation
            .acceptable_decisions
            .iter()
            .map(json_name)
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("  expected: {expected}"));
        lines.push(format!("  actual: {}", json_name(&report.decision())));
        append_reasons("reasons", report.reason_codes(), lines);
        lines.push(format!("  summary: {}", report.summary()));
    }
}

/// Human-authored reference criteria for one visual calibration fixture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AssetCalibrationExpectation {
    acceptable_decisions: Vec<AssetReviewDecision>,
    #[serde(default)]
    expected_reason_codes: Vec<AssetReviewReasonCode>,
    #[serde(default)]
    forbidden_reason_codes: Vec<AssetReviewReasonCode>,
}

impl AssetCalibrationExpectation {
    pub fn from_yaml(yaml: &str) -> Result<Self, CalibrationExpectationError> {
        let expectation: Self = serde_yaml_ng::from_str(yaml)
            .map_err(|error| CalibrationExpectationError::Yaml(error.to_string()))?;
        validate_expectation(
            &expectation.acceptable_decisions,
            &expectation.expected_reason_codes,
            &expectation.forbidden_reason_codes,
        )?;
        Ok(expectation)
    }

    pub fn acceptable_decisions(&self) -> &[AssetReviewDecision] {
        &self.acceptable_decisions
    }

    pub fn expected_reason_codes(&self) -> &[AssetReviewReasonCode] {
        &self.expected_reason_codes
    }

    pub fn forbidden_reason_codes(&self) -> &[AssetReviewReasonCode] {
        &self.forbidden_reason_codes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetCalibrationObservation {
    ReviewerReport(AssetReviewerReport),
    ProviderError(AssetReviewerError),
    BlockedBeforeReview(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetCalibrationCaseStatus {
    Pass {
        report: AssetReviewerReport,
    },
    Mismatch {
        report: AssetReviewerReport,
        decision_matches: bool,
        missing_reason_codes: Vec<AssetReviewReasonCode>,
        forbidden_reason_codes: Vec<AssetReviewReasonCode>,
    },
    ProviderError(AssetReviewerError),
    BlockedBeforeReview(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetCalibrationCaseResult {
    fixture: String,
    expectation: AssetCalibrationExpectation,
    status: AssetCalibrationCaseStatus,
}

impl AssetCalibrationCaseResult {
    pub fn evaluate(
        fixture: impl Into<String>,
        expectation: AssetCalibrationExpectation,
        observation: AssetCalibrationObservation,
    ) -> Self {
        let status = match observation {
            AssetCalibrationObservation::ReviewerReport(report) => {
                let matched = match_observation(
                    &expectation.acceptable_decisions,
                    &expectation.expected_reason_codes,
                    &expectation.forbidden_reason_codes,
                    &report.decision(),
                    report.reason_codes(),
                );
                if matched.decision_matches
                    && matched.missing_reason_codes.is_empty()
                    && matched.forbidden_reason_codes.is_empty()
                {
                    AssetCalibrationCaseStatus::Pass { report }
                } else {
                    AssetCalibrationCaseStatus::Mismatch {
                        report,
                        decision_matches: matched.decision_matches,
                        missing_reason_codes: matched.missing_reason_codes,
                        forbidden_reason_codes: matched.forbidden_reason_codes,
                    }
                }
            }
            AssetCalibrationObservation::ProviderError(error) => {
                AssetCalibrationCaseStatus::ProviderError(error)
            }
            AssetCalibrationObservation::BlockedBeforeReview(reason) => {
                AssetCalibrationCaseStatus::BlockedBeforeReview(reason)
            }
        };
        Self {
            fixture: fixture.into(),
            expectation,
            status,
        }
    }

    pub fn fixture(&self) -> &str {
        &self.fixture
    }

    pub fn status(&self) -> &AssetCalibrationCaseStatus {
        &self.status
    }

    fn decision_matches(&self) -> Option<bool> {
        match &self.status {
            AssetCalibrationCaseStatus::Pass { .. } => Some(true),
            AssetCalibrationCaseStatus::Mismatch {
                decision_matches, ..
            } => Some(*decision_matches),
            AssetCalibrationCaseStatus::ProviderError(_)
            | AssetCalibrationCaseStatus::BlockedBeforeReview(_) => None,
        }
    }

    fn reason_matches(&self) -> Option<bool> {
        match &self.status {
            AssetCalibrationCaseStatus::Pass { .. } => Some(true),
            AssetCalibrationCaseStatus::Mismatch {
                missing_reason_codes,
                forbidden_reason_codes,
                ..
            } => Some(missing_reason_codes.is_empty() && forbidden_reason_codes.is_empty()),
            AssetCalibrationCaseStatus::ProviderError(_)
            | AssetCalibrationCaseStatus::BlockedBeforeReview(_) => None,
        }
    }

    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        match &self.status {
            AssetCalibrationCaseStatus::Pass { report } => {
                lines.push(format!("[PASS] {}", self.fixture));
                self.render_report(report, &mut lines);
            }
            AssetCalibrationCaseStatus::Mismatch {
                report,
                missing_reason_codes,
                forbidden_reason_codes,
                ..
            } => {
                lines.push(format!("[FAIL] {}", self.fixture));
                self.render_report(report, &mut lines);
                append_serialized_reasons("missing_reason", missing_reason_codes, &mut lines);
                append_serialized_reasons("forbidden_reason", forbidden_reason_codes, &mut lines);
            }
            AssetCalibrationCaseStatus::ProviderError(error) => {
                lines.push(format!("[ERROR] {}", self.fixture));
                if let Some(status) = error.http_status() {
                    lines.push(format!("  reviewer_error: {:?}({status})", error.kind()));
                } else {
                    lines.push(format!("  reviewer_error: {:?}", error.kind()));
                }
                if let Some(code) = error.provider_error_code() {
                    lines.push(format!("  provider_error_code: {code}"));
                }
                if let Some(message) = error.provider_error_message() {
                    lines.push(format!("  provider_error_message: {message}"));
                }
                lines.push(format!("  message: {}", error.message()));
            }
            AssetCalibrationCaseStatus::BlockedBeforeReview(reason) => {
                lines.push(format!(
                    "[SKIPPED / BLOCKED_BEFORE_REVIEW] {}",
                    self.fixture
                ));
                lines.push(format!("  reason: {reason}"));
            }
        }
        lines.join("\n")
    }

    fn render_report(&self, report: &AssetReviewerReport, lines: &mut Vec<String>) {
        let expected = self
            .expectation
            .acceptable_decisions
            .iter()
            .map(json_name)
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("  expected: {expected}"));
        lines.push(format!("  actual: {}", json_name(&report.decision())));
        append_serialized_reasons("reasons", report.reason_codes(), lines);
        lines.push(format!("  summary: {}", report.summary()));
    }
}

/// A paired, human-labeled image fixture loaded without reading image bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetCalibrationCase {
    image_path: PathBuf,
    expectation: AssetCalibrationExpectation,
}

impl AssetCalibrationCase {
    pub fn image_path(&self) -> &Path {
        &self.image_path
    }

    pub fn expectation(&self) -> &AssetCalibrationExpectation {
        &self.expectation
    }

    pub fn into_expectation(self) -> AssetCalibrationExpectation {
        self.expectation
    }
}

/// Loads a strictly paired PNG/JPEG + YAML corpus in stable stem order.
pub fn load_asset_calibration_cases(
    directory: &Path,
) -> Result<Vec<AssetCalibrationCase>, AssetCalibrationCorpusError> {
    if !directory.is_dir() {
        return Err(AssetCalibrationCorpusError::DirectoryMissing(
            directory.to_path_buf(),
        ));
    }
    let mut images = BTreeMap::<String, PathBuf>::new();
    let mut expectations = BTreeMap::<String, PathBuf>::new();
    for entry in fs::read_dir(directory).map_err(AssetCalibrationCorpusError::ReadDirectory)? {
        let path = entry
            .map_err(AssetCalibrationCorpusError::ReadDirectory)?
            .path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            return Err(AssetCalibrationCorpusError::NonUnicodePath(path));
        };
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let target = match extension.as_str() {
            "png" | "jpg" | "jpeg" => &mut images,
            "yaml" => &mut expectations,
            _ => continue,
        };
        if target.insert(stem.to_owned(), path.clone()).is_some() {
            return Err(AssetCalibrationCorpusError::DuplicateStem(stem.to_owned()));
        }
    }
    for stem in images.keys() {
        if !expectations.contains_key(stem) {
            return Err(AssetCalibrationCorpusError::MissingExpectation(
                stem.clone(),
            ));
        }
    }
    for stem in expectations.keys() {
        if !images.contains_key(stem) {
            return Err(AssetCalibrationCorpusError::MissingImage(stem.clone()));
        }
    }
    images
        .into_iter()
        .map(|(stem, image_path)| {
            let expectation_path = &expectations[&stem];
            let yaml = fs::read_to_string(expectation_path).map_err(|source| {
                AssetCalibrationCorpusError::ReadExpectation {
                    path: expectation_path.clone(),
                    source,
                }
            })?;
            let expectation = AssetCalibrationExpectation::from_yaml(&yaml).map_err(|source| {
                AssetCalibrationCorpusError::InvalidExpectation {
                    path: expectation_path.clone(),
                    source,
                }
            })?;
            Ok(AssetCalibrationCase {
                image_path,
                expectation,
            })
        })
        .collect()
}

#[derive(Debug)]
pub enum AssetCalibrationCorpusError {
    DirectoryMissing(PathBuf),
    ReadDirectory(std::io::Error),
    NonUnicodePath(PathBuf),
    DuplicateStem(String),
    MissingImage(String),
    MissingExpectation(String),
    ReadExpectation {
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidExpectation {
        path: PathBuf,
        source: CalibrationExpectationError,
    },
}

impl fmt::Display for AssetCalibrationCorpusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectoryMissing(path) => {
                write!(
                    formatter,
                    "fixture directory does not exist: {}",
                    path.display()
                )
            }
            Self::ReadDirectory(error) => {
                write!(formatter, "could not read fixture directory: {error}")
            }
            Self::NonUnicodePath(path) => {
                write!(
                    formatter,
                    "fixture path is not valid Unicode: {}",
                    path.display()
                )
            }
            Self::DuplicateStem(stem) => write!(formatter, "duplicate image or YAML stem: {stem}"),
            Self::MissingImage(stem) => write!(formatter, "expectation has no image: {stem}"),
            Self::MissingExpectation(stem) => write!(formatter, "image has no expectation: {stem}"),
            Self::ReadExpectation { path, source } => write!(
                formatter,
                "could not read expectation {}: {source}",
                path.display()
            ),
            Self::InvalidExpectation { path, source } => write!(
                formatter,
                "invalid expectation {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for AssetCalibrationCorpusError {}

fn append_reasons(label: &str, reasons: &[ReviewReasonCode], lines: &mut Vec<String>) {
    append_serialized_reasons(label, reasons, lines);
}

fn append_serialized_reasons<T: serde::Serialize>(
    label: &str,
    reasons: &[T],
    lines: &mut Vec<String>,
) {
    if reasons.is_empty() {
        return;
    }
    lines.push(format!("  {label}:"));
    lines.extend(
        reasons
            .iter()
            .map(|reason| format!("    - {}", json_name(reason))),
    );
}

fn json_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .expect("calibration enum is serializable")
        .as_str()
        .expect("calibration enum serializes as a string")
        .to_owned()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationExitStatus {
    Success,
    CalibrationMismatch,
    ExecutionError,
}

impl CalibrationExitStatus {
    pub fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::CalibrationMismatch => 1,
            Self::ExecutionError => 2,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CalibrationSummary {
    cases: usize,
    passed: usize,
    failed: usize,
    errors: usize,
    skipped: usize,
    decision_matches: usize,
    decision_evaluated: usize,
    reason_matches: usize,
    reason_evaluated: usize,
}

impl CalibrationSummary {
    pub fn from_results(results: &[CalibrationCaseResult]) -> Self {
        let mut summary = Self {
            cases: results.len(),
            ..Self::default()
        };
        for result in results {
            match result.status() {
                CalibrationCaseStatus::Pass { .. } => summary.passed += 1,
                CalibrationCaseStatus::Mismatch { .. } => summary.failed += 1,
                CalibrationCaseStatus::ProviderError(_) => summary.errors += 1,
                CalibrationCaseStatus::BlockedBeforeReview(_) => summary.skipped += 1,
            }
            if let Some(matches) = result.decision_matches() {
                summary.decision_evaluated += 1;
                summary.decision_matches += usize::from(matches);
            }
            if let Some(matches) = result.reason_matches() {
                summary.reason_evaluated += 1;
                summary.reason_matches += usize::from(matches);
            }
        }
        summary
    }

    pub fn from_asset_results(results: &[AssetCalibrationCaseResult]) -> Self {
        let mut summary = Self {
            cases: results.len(),
            ..Self::default()
        };
        for result in results {
            match result.status() {
                AssetCalibrationCaseStatus::Pass { .. } => summary.passed += 1,
                AssetCalibrationCaseStatus::Mismatch { .. } => summary.failed += 1,
                AssetCalibrationCaseStatus::ProviderError(_) => summary.errors += 1,
                AssetCalibrationCaseStatus::BlockedBeforeReview(_) => summary.skipped += 1,
            }
            if let Some(matches) = result.decision_matches() {
                summary.decision_evaluated += 1;
                summary.decision_matches += usize::from(matches);
            }
            if let Some(matches) = result.reason_matches() {
                summary.reason_evaluated += 1;
                summary.reason_matches += usize::from(matches);
            }
        }
        summary
    }

    pub fn exit_status(&self) -> CalibrationExitStatus {
        if self.errors > 0 || self.skipped > 0 {
            CalibrationExitStatus::ExecutionError
        } else if self.failed > 0 {
            CalibrationExitStatus::CalibrationMismatch
        } else {
            CalibrationExitStatus::Success
        }
    }

    pub fn render(&self) -> String {
        format!(
            "cases: {}\npassed: {}\nfailed: {}\nerrors: {}\nskipped: {}\ndecision_match: {}/{}\nreason_match: {}/{}",
            self.cases,
            self.passed,
            self.failed,
            self.errors,
            self.skipped,
            self.decision_matches,
            self.decision_evaluated,
            self.reason_matches,
            self.reason_evaluated
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        ffi::OsStr,
        fs,
        path::{Path, PathBuf},
        time::SystemTime,
    };

    use crate::{
        content::{AnalyzedMarkdown, MarkdownReferenceParser},
        domain::{ContentPath, Sha256, SnapshotFile},
        policy::{
            MarkdownFrontmatterParser, PrivacyFilter, PublicPolicy, Reviewer, ReviewerErrorKind,
        },
    };

    use super::*;

    fn expectation(yaml: &str) -> CalibrationExpectation {
        CalibrationExpectation::from_yaml(yaml).unwrap()
    }

    fn report(decision: ReviewDecision, reasons: Vec<ReviewReasonCode>) -> ReviewerReport {
        ReviewerReport::new(decision, reasons, "Safe short summary.").unwrap()
    }

    #[test]
    fn parses_strict_human_expectation() {
        let parsed = expectation(
            "acceptable_decisions: [needs_human_review, reject]\n\
             expected_reason_codes: [uncertain_disclosure_authorization]\n\
             forbidden_reason_codes: [credential_secret]\n",
        );

        assert_eq!(
            parsed.acceptable_decisions(),
            [ReviewDecision::NeedsHumanReview, ReviewDecision::Reject]
        );
        assert!(
            CalibrationExpectation::from_yaml("acceptable_decisions: [approve]\nunknown: true\n")
                .is_err()
        );
    }

    #[test]
    fn decision_matches_any_acceptable_value() {
        let result = CalibrationCaseResult::evaluate(
            "boundary.md",
            expectation("acceptable_decisions: [needs_human_review, reject]\n"),
            CalibrationObservation::ReviewerReport(report(
                ReviewDecision::Reject,
                vec![ReviewReasonCode::InternalWorkInformation],
            )),
        );

        assert!(matches!(
            result.status(),
            CalibrationCaseStatus::Pass { .. }
        ));
        assert_eq!(result.decision_matches(), Some(true));
    }

    #[test]
    fn reason_matching_requires_expected_subset_not_exact_set() {
        let result = CalibrationCaseResult::evaluate(
            "work.md",
            expectation(
                "acceptable_decisions: [approve]\n\
                 expected_reason_codes: [ordinary_work_experience]\n",
            ),
            CalibrationObservation::ReviewerReport(report(
                ReviewDecision::Approve,
                vec![
                    ReviewReasonCode::OrdinaryWorkExperience,
                    ReviewReasonCode::PublicTechnicalContent,
                ],
            )),
        );

        assert_eq!(result.reason_matches(), Some(true));
    }

    #[test]
    fn missing_expected_reason_is_reported_as_a_mismatch() {
        let result = CalibrationCaseResult::evaluate(
            "work.md",
            expectation(
                "acceptable_decisions: [needs_human_review]\n\
                 expected_reason_codes: [uncertain_disclosure_authorization]\n",
            ),
            CalibrationObservation::ReviewerReport(report(
                ReviewDecision::NeedsHumanReview,
                vec![ReviewReasonCode::InternalWorkInformation],
            )),
        );

        assert_eq!(result.reason_matches(), Some(false));
        assert!(
            result
                .render()
                .contains("missing_reason:\n    - uncertain_disclosure_authorization")
        );
    }

    #[test]
    fn forbidden_reason_is_a_mismatch_even_when_decision_matches() {
        let result = CalibrationCaseResult::evaluate(
            "run.md",
            expectation(
                "acceptable_decisions: [approve]\n\
                 forbidden_reason_codes: [ordinary_work_experience]\n",
            ),
            CalibrationObservation::ReviewerReport(report(
                ReviewDecision::Approve,
                vec![ReviewReasonCode::OrdinaryWorkExperience],
            )),
        );

        assert_eq!(result.decision_matches(), Some(true));
        assert_eq!(result.reason_matches(), Some(false));
        assert!(result.render().contains("forbidden_reason:"));
    }

    struct FakeReviewer {
        responses: RefCell<VecDeque<Result<ReviewerReport, ReviewerError>>>,
    }

    impl Reviewer for FakeReviewer {
        fn review(
            &self,
            _candidate: &crate::policy::ReviewCandidate,
        ) -> Result<ReviewerReport, ReviewerError> {
            self.responses.borrow_mut().pop_front().unwrap()
        }
    }

    fn public_candidate() -> crate::policy::PublicCandidateMarkdown {
        let markdown = "ordinary note";
        let file = SnapshotFile::new(
            ContentPath::new("case.md").unwrap(),
            markdown.len() as u64,
            Sha256::digest(markdown.as_bytes()),
            None,
        );
        let analysis = AnalyzedMarkdown::with_frontmatter(
            file,
            MarkdownFrontmatterParser::parse(markdown),
            MarkdownReferenceParser::parse(markdown)
                .into_iter()
                .map(|_| unreachable!())
                .collect(),
        );
        PrivacyFilter::filter(vec![analysis])
            .into_public_candidates()
            .pop()
            .unwrap()
    }

    #[test]
    fn fake_reviewer_error_remains_distinct_from_mismatch() {
        let reviewer = FakeReviewer {
            responses: RefCell::new(VecDeque::from([Err(ReviewerError::with_http_status(
                ReviewerErrorKind::HttpStatus,
                429,
                "provider returned an error status",
            ))])),
        };
        let outcome = PublicPolicy::evaluate(vec![public_candidate()], &reviewer)
            .pop()
            .unwrap();
        let error = match outcome.decision() {
            crate::policy::PublicPolicyDecision::NeedsHumanReview(
                crate::policy::HumanReviewReason::ReviewerFailed(error),
            ) => error.clone(),
            other => panic!("unexpected outcome: {other:?}"),
        };
        let result = CalibrationCaseResult::evaluate(
            "case.md",
            expectation("acceptable_decisions: [needs_human_review]\n"),
            CalibrationObservation::ProviderError(error),
        );

        assert!(matches!(
            result.status(),
            CalibrationCaseStatus::ProviderError(_)
        ));
        assert!(result.render().starts_with("[ERROR] case.md"));
        assert!(result.render().contains("reviewer_error: HttpStatus(429)"));
    }

    #[test]
    fn renders_summary_and_derives_distinct_exit_statuses() {
        let pass = CalibrationCaseResult::evaluate(
            "pass.md",
            expectation("acceptable_decisions: [approve]\n"),
            CalibrationObservation::ReviewerReport(report(ReviewDecision::Approve, vec![])),
        );
        let mismatch = CalibrationCaseResult::evaluate(
            "fail.md",
            expectation("acceptable_decisions: [reject]\n"),
            CalibrationObservation::ReviewerReport(report(ReviewDecision::Approve, vec![])),
        );
        let error = CalibrationCaseResult::evaluate(
            "error.md",
            expectation("acceptable_decisions: [approve]\n"),
            CalibrationObservation::ProviderError(ReviewerError::new("offline")),
        );

        let pass_summary = CalibrationSummary::from_results(std::slice::from_ref(&pass));
        assert_eq!(pass_summary.exit_status(), CalibrationExitStatus::Success);
        assert!(pass_summary.render().contains("decision_match: 1/1"));
        assert_eq!(
            CalibrationSummary::from_results(&[pass.clone(), mismatch]).exit_status(),
            CalibrationExitStatus::CalibrationMismatch
        );
        assert_eq!(
            CalibrationSummary::from_results(&[pass, error]).exit_status(),
            CalibrationExitStatus::ExecutionError
        );
        assert_eq!(CalibrationExitStatus::ExecutionError.code(), 2);
    }

    #[test]
    fn checked_in_corpus_has_fourteen_separate_valid_expectations() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("fixtures")
            .join("markdown_reviewer_eval");
        let mut markdown_count = 0;
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension() != Some(OsStr::new("md")) {
                continue;
            }
            markdown_count += 1;
            let yaml = fs::read_to_string(path.with_extension("yaml")).unwrap();
            CalibrationExpectation::from_yaml(&yaml).unwrap();
        }

        assert_eq!(markdown_count, 14);
    }

    fn asset_expectation(yaml: &str) -> AssetCalibrationExpectation {
        AssetCalibrationExpectation::from_yaml(yaml).unwrap()
    }

    fn asset_report(
        decision: AssetReviewDecision,
        reasons: Vec<AssetReviewReasonCode>,
    ) -> AssetReviewerReport {
        AssetReviewerReport::new(decision, reasons, "Safe synthetic visual summary.").unwrap()
    }

    #[test]
    fn parses_and_strictly_validates_asset_expectations() {
        let parsed = asset_expectation(
            "acceptable_decisions: [needs_human_review, reject]\n\
             expected_reason_codes: [internal_work_information]\n\
             forbidden_reason_codes: [ordinary_visual_content]\n",
        );
        assert_eq!(
            parsed.acceptable_decisions(),
            [
                AssetReviewDecision::NeedsHumanReview,
                AssetReviewDecision::Reject
            ]
        );
        assert_eq!(
            parsed.expected_reason_codes(),
            [AssetReviewReasonCode::InternalWorkInformation]
        );
        assert_eq!(
            parsed.forbidden_reason_codes(),
            [AssetReviewReasonCode::OrdinaryVisualContent]
        );

        for invalid in [
            "acceptable_decisions: [approve]\nunknown: true\n",
            "acceptable_decisions: []\n",
            "acceptable_decisions: [approve, approve]\n",
            "acceptable_decisions: [approve]\nexpected_reason_codes: [ordinary_visual_content, ordinary_visual_content]\n",
            "acceptable_decisions: [approve]\nforbidden_reason_codes: [ordinary_visual_content, ordinary_visual_content]\n",
            "acceptable_decisions: [approve]\nexpected_reason_codes: [ordinary_visual_content]\nforbidden_reason_codes: [ordinary_visual_content]\n",
        ] {
            assert!(AssetCalibrationExpectation::from_yaml(invalid).is_err());
        }
    }

    #[test]
    fn asset_matching_uses_any_decision_required_subset_and_forbidden_reasons() {
        let expectation = asset_expectation(
            "acceptable_decisions: [needs_human_review, reject]\n\
             expected_reason_codes: [internal_work_information]\n\
             forbidden_reason_codes: [visible_credential_secret]\n",
        );
        let pass = AssetCalibrationCaseResult::evaluate(
            "pass.png",
            expectation.clone(),
            AssetCalibrationObservation::ReviewerReport(asset_report(
                AssetReviewDecision::Reject,
                vec![
                    AssetReviewReasonCode::InternalWorkInformation,
                    AssetReviewReasonCode::ConfidentialWorkMaterial,
                ],
            )),
        );
        assert!(matches!(
            pass.status(),
            AssetCalibrationCaseStatus::Pass { .. }
        ));

        let mismatch = AssetCalibrationCaseResult::evaluate(
            "fail.png",
            expectation,
            AssetCalibrationObservation::ReviewerReport(asset_report(
                AssetReviewDecision::NeedsHumanReview,
                vec![AssetReviewReasonCode::VisibleCredentialSecret],
            )),
        );
        match mismatch.status() {
            AssetCalibrationCaseStatus::Mismatch {
                decision_matches,
                missing_reason_codes,
                forbidden_reason_codes,
                ..
            } => {
                assert!(*decision_matches);
                assert_eq!(
                    missing_reason_codes,
                    &[AssetReviewReasonCode::InternalWorkInformation]
                );
                assert_eq!(
                    forbidden_reason_codes,
                    &[AssetReviewReasonCode::VisibleCredentialSecret]
                );
            }
            other => panic!("unexpected status: {other:?}"),
        }
        assert!(mismatch.render().starts_with("[FAIL] fail.png"));
        assert!(mismatch.render().contains("missing_reason:"));
        assert!(mismatch.render().contains("forbidden_reason:"));
    }

    #[test]
    fn asset_results_keep_mismatch_error_and_pre_review_block_distinct() {
        let expectation = asset_expectation("acceptable_decisions: [approve]\n");
        let pass = AssetCalibrationCaseResult::evaluate(
            "pass.png",
            expectation.clone(),
            AssetCalibrationObservation::ReviewerReport(asset_report(
                AssetReviewDecision::Approve,
                vec![AssetReviewReasonCode::OrdinaryVisualContent],
            )),
        );
        let fail = AssetCalibrationCaseResult::evaluate(
            "fail.png",
            expectation.clone(),
            AssetCalibrationObservation::ReviewerReport(asset_report(
                AssetReviewDecision::NeedsHumanReview,
                vec![AssetReviewReasonCode::OtherVisualPrivacyRisk],
            )),
        );
        let error = AssetCalibrationCaseResult::evaluate(
            "error.png",
            expectation.clone(),
            AssetCalibrationObservation::ProviderError(AssetReviewerError::with_provider_error(
                crate::workflow::AssetReviewerErrorKind::HttpStatus,
                429,
                "rate limited",
                Some("rate_limit".to_owned()),
                Some("retry later".to_owned()),
            )),
        );
        let skipped = AssetCalibrationCaseResult::evaluate(
            "blocked.png",
            expectation,
            AssetCalibrationObservation::BlockedBeforeReview("unsupported type".to_owned()),
        );

        assert!(pass.render().starts_with("[PASS]"));
        assert!(fail.render().starts_with("[FAIL]"));
        assert!(error.render().starts_with("[ERROR]"));
        assert!(error.render().contains("HttpStatus(429)"));
        assert!(
            skipped
                .render()
                .starts_with("[SKIPPED / BLOCKED_BEFORE_REVIEW]")
        );
        assert_eq!(
            CalibrationSummary::from_asset_results(std::slice::from_ref(&pass)).exit_status(),
            CalibrationExitStatus::Success
        );
        assert_eq!(
            CalibrationSummary::from_asset_results(&[pass.clone(), fail]).exit_status(),
            CalibrationExitStatus::CalibrationMismatch
        );
        let summary = CalibrationSummary::from_asset_results(&[pass, error, skipped]);
        assert_eq!(summary.exit_status(), CalibrationExitStatus::ExecutionError);
        assert_eq!(summary.exit_status().code(), 2);
        assert!(summary.render().contains("errors: 1\nskipped: 1"));
    }

    struct CorpusDirectory(PathBuf);

    impl CorpusDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "mineral-asset-calibration-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.0.join(name), bytes).unwrap();
        }
    }

    impl Drop for CorpusDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn asset_corpus_loader_enforces_pairing_and_duplicate_stems() {
        let valid = CorpusDirectory::new();
        valid.write("a.png", b"fixed image bytes");
        valid.write("a.yaml", b"acceptable_decisions: [approve]\n");
        assert_eq!(load_asset_calibration_cases(&valid.0).unwrap().len(), 1);

        let missing_image = CorpusDirectory::new();
        missing_image.write("a.yaml", b"acceptable_decisions: [approve]\n");
        assert!(matches!(
            load_asset_calibration_cases(&missing_image.0),
            Err(AssetCalibrationCorpusError::MissingImage(stem)) if stem == "a"
        ));

        let missing_expectation = CorpusDirectory::new();
        missing_expectation.write("a.jpg", b"fixed image bytes");
        assert!(matches!(
            load_asset_calibration_cases(&missing_expectation.0),
            Err(AssetCalibrationCorpusError::MissingExpectation(stem)) if stem == "a"
        ));

        let duplicate = CorpusDirectory::new();
        duplicate.write("a.png", b"fixed image bytes");
        duplicate.write("a.jpeg", b"fixed image bytes");
        duplicate.write("a.yaml", b"acceptable_decisions: [approve]\n");
        assert!(matches!(
            load_asset_calibration_cases(&duplicate.0),
            Err(AssetCalibrationCorpusError::DuplicateStem(stem)) if stem == "a"
        ));
    }

    #[test]
    fn checked_in_asset_corpus_has_thirteen_valid_decodable_balanced_cases() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("fixtures")
            .join("asset_reviewer_eval");
        let cases = load_asset_calibration_cases(&directory).unwrap();
        assert_eq!(cases.len(), 13);
        let mut approve = 0;
        let mut human = 0;
        let mut reject = 0;
        for case in cases {
            image::ImageReader::open(case.image_path())
                .unwrap()
                .decode()
                .unwrap();
            approve += usize::from(
                case.expectation()
                    .acceptable_decisions()
                    .contains(&AssetReviewDecision::Approve),
            );
            human += usize::from(
                case.expectation()
                    .acceptable_decisions()
                    .contains(&AssetReviewDecision::NeedsHumanReview),
            );
            reject += usize::from(
                case.expectation()
                    .acceptable_decisions()
                    .contains(&AssetReviewDecision::Reject),
            );
        }
        assert!(approve > 0);
        assert!(human > 0);
        assert!(reject > 0);
    }
}
