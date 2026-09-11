use std::{error::Error, fmt};

use serde::Deserialize;

use crate::policy::{ReviewDecision, ReviewReasonCode, ReviewerError, ReviewerReport};

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
        if self.acceptable_decisions.is_empty() {
            return Err(CalibrationExpectationError::NoAcceptableDecision);
        }
        if has_duplicates(&self.acceptable_decisions) {
            return Err(CalibrationExpectationError::DuplicateAcceptableDecision);
        }
        if has_duplicates(&self.expected_reason_codes) {
            return Err(CalibrationExpectationError::DuplicateExpectedReasonCode);
        }
        if has_duplicates(&self.forbidden_reason_codes) {
            return Err(CalibrationExpectationError::DuplicateForbiddenReasonCode);
        }
        if self
            .expected_reason_codes
            .iter()
            .any(|reason| self.forbidden_reason_codes.contains(reason))
        {
            return Err(CalibrationExpectationError::ConflictingReasonCode);
        }
        Ok(())
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
                let decision_matches = expectation
                    .acceptable_decisions
                    .contains(&report.decision());
                let missing_reason_codes = expectation
                    .expected_reason_codes
                    .iter()
                    .copied()
                    .filter(|reason| !report.reason_codes().contains(reason))
                    .collect::<Vec<_>>();
                let forbidden_reason_codes = expectation
                    .forbidden_reason_codes
                    .iter()
                    .copied()
                    .filter(|reason| report.reason_codes().contains(reason))
                    .collect::<Vec<_>>();
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

fn append_reasons(label: &str, reasons: &[ReviewReasonCode], lines: &mut Vec<String>) {
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
    use std::{cell::RefCell, collections::VecDeque, ffi::OsStr, fs, path::Path};

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
}
