use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::{
    domain::{ContentPath, Sha256},
    privacy_filter::PublicCandidateMarkdown,
    program_check::{ProgramCheck, ProgramCheckIssue, ProgramCheckResult},
    snapshot_markdown_analysis::AnalyzedMarkdown,
};

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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
    NeedsHumanReview,
}

/// A failure while executing a reviewer.
///
/// Reviewer failures are distinct from review decisions so callers cannot
/// accidentally treat an unavailable or malformed response as approval.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ReviewerError {
    message: String,
}

impl ReviewerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ReviewerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ReviewerError {}

/// Reviews only documents that crossed the privacy and program-check boundaries.
///
/// Implementations return a decision and have no publishing authority.
pub trait Reviewer {
    fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewDecision, ReviewerError>;
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
                let decision = match ProgramCheck::check(std::slice::from_ref(&document)) {
                    ProgramCheckResult::Issues(issues) => {
                        PublicPolicyDecision::ProgramIssues(issues)
                    }
                    ProgramCheckResult::Pass => {
                        let candidate = ReviewCandidate(document);
                        match reviewer.review(&candidate) {
                            Ok(ReviewDecision::Approve) => PublicPolicyDecision::ReviewApproved,
                            Ok(ReviewDecision::Reject) => PublicPolicyDecision::ReviewRejected,
                            Ok(ReviewDecision::NeedsHumanReview) => {
                                PublicPolicyDecision::NeedsHumanReview(
                                    HumanReviewReason::ReviewerRequested,
                                )
                            }
                            Err(error) => PublicPolicyDecision::NeedsHumanReview(
                                HumanReviewReason::ReviewerFailed(error),
                            ),
                        }
                    }
                };

                PublicPolicyOutcome {
                    path,
                    content_sha256,
                    decision,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap};

    use crate::{
        domain::{
            ContentPath, MarkdownFrontmatterParser, MarkdownReferenceParser, Resolution, Sha256,
            SnapshotFile,
        },
        privacy_filter::PrivacyFilter,
        snapshot_markdown_analysis::{AnalyzedMarkdown, ResolvedReference},
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
        fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewDecision, ReviewerError> {
            self.calls.set(self.calls.get() + 1);
            self.responses
                .get(candidate.path().as_str())
                .cloned()
                .expect("test response for candidate")
        }
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
        assert_eq!(calls, 1);
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
