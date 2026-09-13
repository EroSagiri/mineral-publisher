use std::{error::Error, fmt, time::SystemTime};

use crate::{
    content::{AssetDependencyGraph, SnapshotMarkdownAnalysisError, SnapshotMarkdownAnalyzer},
    domain::{ContentPath, Snapshot, SnapshotId},
    policy::{
        InvalidPrivacyDocument, PolicyIdentity, PrivacyFilter, PrivateDocument, ProgramCheck,
        ProgramCheckIssue, PublicCandidateMarkdown, PublicPolicy, PublicPolicyDecision,
        PublicPolicyOutcome, ReviewRun, ReviewRunError, ReviewRunId, ReviewRunStore, Reviewer,
    },
    ports::BlobStore,
};

use super::{HumanReviewKind, HumanReviewResolution, HumanReviewStore};

/// Decides how a batch of review candidates is executed.
///
/// The engine only needs "evaluate these candidates"; whether that happens
/// sequentially or with bounded parallelism is a runtime concern. Runtimes
/// implement this trait with their own execution strategy.
pub trait MarkdownReviewEvaluator<R: Reviewer + ?Sized> {
    fn is_bounded(&self) -> bool;
    fn evaluate(
        &self,
        candidates: Vec<PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<PublicPolicyOutcome>;
}

/// The portable default: evaluate every candidate in order on the caller's thread.
pub struct SequentialMarkdownReviews;
impl<R: Reviewer + ?Sized> MarkdownReviewEvaluator<R> for SequentialMarkdownReviews {
    fn is_bounded(&self) -> bool {
        false
    }
    fn evaluate(
        &self,
        candidates: Vec<PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<PublicPolicyOutcome> {
        PublicPolicy::evaluate(candidates, reviewer)
    }
}

/// Allocates the identity of an audit attempt at the orchestration boundary.
pub trait ReviewRunIdGenerator {
    type Error: Error + Send + Sync + 'static;

    fn next_id(&mut self) -> Result<ReviewRunId, Self::Error>;
}

/// A minimal process-local allocator suitable when its starting value is owned by the caller.
#[derive(Clone, Debug)]
pub struct SequentialReviewRunIdGenerator {
    next: Option<u64>,
}

impl SequentialReviewRunIdGenerator {
    pub fn new(first: ReviewRunId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl ReviewRunIdGenerator for SequentialReviewRunIdGenerator {
    type Error = SequentialReviewRunIdGeneratorError;

    fn next_id(&mut self) -> Result<ReviewRunId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialReviewRunIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        ReviewRunId::new(value).map_err(|_| SequentialReviewRunIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialReviewRunIdGeneratorError {
    Exhausted,
}

impl fmt::Display for SequentialReviewRunIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("review run id sequence is exhausted")
    }
}

impl Error for SequentialReviewRunIdGeneratorError {}

/// Successful, deterministically ordered output of one complete Snapshot policy run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicPolicyRunResult {
    snapshot_id: SnapshotId,
    private_documents: Vec<PrivateDocument>,
    invalid_privacy_documents: Vec<InvalidPrivacyDocument>,
    warnings: Vec<ProgramCheckIssue>,
    document_outcomes: Vec<ReviewRun>,
    dependency_graph: Option<Box<AssetDependencyGraph>>,
}

impl PublicPolicyRunResult {
    fn empty(snapshot_id: SnapshotId) -> Self {
        Self {
            snapshot_id,
            private_documents: Vec::new(),
            invalid_privacy_documents: Vec::new(),
            warnings: Vec::new(),
            document_outcomes: Vec::new(),
            dependency_graph: None,
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn private_documents(&self) -> &[PrivateDocument] {
        &self.private_documents
    }

    pub fn invalid_privacy_documents(&self) -> &[InvalidPrivacyDocument] {
        &self.invalid_privacy_documents
    }

    pub fn warnings(&self) -> &[ProgramCheckIssue] {
        &self.warnings
    }

    /// Review Runs which were durably saved, ordered by `ContentPath`.
    pub fn document_outcomes(&self) -> &[ReviewRun] {
        &self.document_outcomes
    }

    /// Dependency analysis produced from the same immutable Snapshot as this
    /// policy result. It is absent only on an unsuccessful partial result.
    pub fn dependency_graph(&self) -> Option<&AssetDependencyGraph> {
        self.dependency_graph.as_deref()
    }

    /// The Markdown set eligible to contribute edges to candidate asset selection.
    pub fn approved_markdown_paths(&self) -> Vec<&ContentPath> {
        self.document_outcomes
            .iter()
            .filter(|run| matches!(run.decision(), PublicPolicyDecision::ReviewApproved))
            .map(ReviewRun::content_path)
            .collect()
    }

    #[doc(hidden)]
    pub fn from_document_outcomes_for_test(
        snapshot_id: SnapshotId,
        document_outcomes: Vec<ReviewRun>,
    ) -> Self {
        Self {
            snapshot_id,
            private_documents: Vec::new(),
            invalid_privacy_documents: Vec::new(),
            warnings: Vec::new(),
            document_outcomes,
            dependency_graph: None,
        }
    }

    #[doc(hidden)]
    pub fn from_parts_for_test(
        snapshot_id: SnapshotId,
        private_documents: Vec<PrivateDocument>,
        invalid_privacy_documents: Vec<InvalidPrivacyDocument>,
        document_outcomes: Vec<ReviewRun>,
    ) -> Self {
        Self {
            snapshot_id,
            private_documents,
            invalid_privacy_documents,
            warnings: Vec::new(),
            document_outcomes,
            dependency_graph: None,
        }
    }
}

/// The stage that prevented a run from completing successfully.
#[derive(Debug)]
pub enum PublicPolicyRunFailure<StoreError, IdError, HumanError> {
    MarkdownAnalysis(Vec<SnapshotMarkdownAnalysisError>),
    ReviewCacheLookup(StoreError),
    HumanReviewLookup(HumanError),
    ReviewRunId {
        path: ContentPath,
        source: IdError,
    },
    ReviewRun {
        path: ContentPath,
        source: ReviewRunError,
    },
    Persistence {
        path: ContentPath,
        review_run_id: ReviewRunId,
        source: StoreError,
    },
}

/// A failed run together with every Review Run that was saved before the failure.
#[derive(Debug)]
pub struct PublicPolicyRunError<StoreError, IdError, HumanError> {
    partial_result: PublicPolicyRunResult,
    failure: Box<PublicPolicyRunFailure<StoreError, IdError, HumanError>>,
}

impl<StoreError, IdError, HumanError> PublicPolicyRunError<StoreError, IdError, HumanError> {
    pub fn partial_result(&self) -> &PublicPolicyRunResult {
        &self.partial_result
    }

    pub fn failure(&self) -> &PublicPolicyRunFailure<StoreError, IdError, HumanError> {
        &self.failure
    }
}

impl<StoreError: fmt::Display, IdError: fmt::Display, HumanError: fmt::Display> fmt::Display
    for PublicPolicyRunError<StoreError, IdError, HumanError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure.as_ref() {
            PublicPolicyRunFailure::MarkdownAnalysis(failures) => write!(
                formatter,
                "public policy run failed to analyze {} Markdown document(s)",
                failures.len()
            ),
            PublicPolicyRunFailure::HumanReviewLookup(source) => {
                write!(
                    formatter,
                    "could not look up human review decisions: {source}"
                )
            }
            PublicPolicyRunFailure::ReviewCacheLookup(source) => {
                write!(
                    formatter,
                    "could not look up reusable ReviewResults: {source}"
                )
            }
            PublicPolicyRunFailure::ReviewRunId { path, source } => {
                write!(
                    formatter,
                    "could not allocate ReviewRunId for {path}: {source}"
                )
            }
            PublicPolicyRunFailure::ReviewRun { path, source } => {
                write!(formatter, "could not create ReviewRun for {path}: {source}")
            }
            PublicPolicyRunFailure::Persistence { path, source, .. } => {
                write!(
                    formatter,
                    "could not persist ReviewRun for {path}: {source}"
                )
            }
        }
    }
}

impl<StoreError, IdError, HumanError> Error
    for PublicPolicyRunError<StoreError, IdError, HumanError>
where
    StoreError: Error + Send + Sync + 'static,
    IdError: Error + Send + Sync + 'static,
    HumanError: Error + Send + Sync + 'static,
{
}

/// Orchestrates the existing immutable-content, privacy, policy, and audit boundaries.
pub struct PublicPolicyRun;

impl PublicPolicyRun {
    /// Runs one complete policy run at the caller-supplied time.
    ///
    /// The engine never reads a clock itself: `created_at` is the run's audit
    /// timestamp for every ReviewRun it persists, and `evaluator` decides how a
    /// batch of candidates is executed.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn execute_at<R, S, I, E, B, H>(
        snapshot: &Snapshot,
        content_store: &B,
        reviewer: &R,
        review_run_store: &S,
        human_reviews: &H,
        policy: &PolicyIdentity,
        id_generator: &mut I,
        created_at: SystemTime,
        evaluator: &E,
    ) -> Result<PublicPolicyRunResult, PublicPolicyRunError<S::Error, I::Error, H::Error>>
    where
        R: Reviewer + ?Sized,
        S: ReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
        I: ReviewRunIdGenerator + ?Sized,
        E: MarkdownReviewEvaluator<R> + ?Sized,
        B: BlobStore + Clone,
    {
        let mut result = PublicPolicyRunResult::empty(snapshot.id());
        let analyzer = SnapshotMarkdownAnalyzer::new(content_store.clone());
        let mut analyses = Vec::new();
        let mut analysis_failures = Vec::new();

        for file in snapshot
            .files()
            .iter()
            .filter(|file| file.path().as_str().ends_with(".md"))
        {
            match analyzer.analyze(snapshot, file.path()) {
                Ok(analysis) => analyses.push(analysis),
                Err(error) => analysis_failures.push(error),
            }
        }

        if !analysis_failures.is_empty() {
            return Err(PublicPolicyRunError {
                partial_result: result,
                failure: Box::new(PublicPolicyRunFailure::MarkdownAnalysis(analysis_failures)),
            });
        }

        result.dependency_graph = Some(Box::new(AssetDependencyGraph::build(
            snapshot.id(),
            &analyses,
        )));
        let (candidates, private_documents, invalid_privacy_documents) =
            PrivacyFilter::filter(analyses).into_parts();
        result.private_documents = private_documents;
        result.invalid_privacy_documents = invalid_privacy_documents;

        let reusable = review_run_store
            .list_by_snapshot(snapshot.id())
            .map_err(|source| PublicPolicyRunError {
                partial_result: result.clone(),
                failure: Box::new(PublicPolicyRunFailure::ReviewCacheLookup(source)),
            })?;
        // A subject a human already decided is a final answer: the provider is not
        // asked about it again, whatever the previous attempt recorded. That is what
        // lets an approval survive the re-run after a provider outage instead of
        // being invalidated by the next failed attempt.
        let human_answered = HumanReviewResolution::answered_paths(
            human_reviews,
            HumanReviewKind::Document,
            policy,
            candidates
                .iter()
                .map(|candidate| (candidate.path(), candidate.analysis().file().sha256())),
        )
        .map_err(|source| PublicPolicyRunError {
            partial_result: result.clone(),
            failure: Box::new(PublicPolicyRunFailure::HumanReviewLookup(source)),
        })?;

        if !evaluator.is_bounded() {
            for candidate in candidates {
                let check = ProgramCheck::check(std::slice::from_ref(&candidate));
                result.warnings.extend_from_slice(check.warnings());
                if check.is_pass()
                    && let Some(previous) = reusable.iter().find(|run| {
                        run.content_path() == candidate.path()
                            && run.content_sha256() == candidate.analysis().file().sha256()
                            && run.policy() == policy
                            && (run.reviewer_report().is_some()
                                || human_answered.contains(candidate.path()))
                    })
                {
                    result.document_outcomes.push(previous.clone());
                    continue;
                }
                let outcome = PublicPolicy::evaluate(vec![candidate], reviewer)
                    .pop()
                    .expect("one public candidate always produces one policy outcome");
                let path = outcome.path().clone();
                let id = id_generator
                    .next_id()
                    .map_err(|source| PublicPolicyRunError {
                        partial_result: result.clone(),
                        failure: Box::new(PublicPolicyRunFailure::ReviewRunId {
                            path: path.clone(),
                            source,
                        }),
                    })?;
                let review_run = ReviewRun::from_policy_outcome(
                    id,
                    snapshot,
                    &outcome,
                    policy.clone(),
                    created_at,
                )
                .map_err(|source| PublicPolicyRunError {
                    partial_result: result.clone(),
                    failure: Box::new(PublicPolicyRunFailure::ReviewRun {
                        path: path.clone(),
                        source,
                    }),
                })?;
                review_run_store
                    .save(&review_run)
                    .map_err(|source| PublicPolicyRunError {
                        partial_result: result.clone(),
                        failure: Box::new(PublicPolicyRunFailure::Persistence {
                            path,
                            review_run_id: id,
                            source,
                        }),
                    })?;
                result.document_outcomes.push(review_run);
            }
            return Ok(result);
        }

        let mut reused = std::collections::BTreeMap::new();
        let mut pending = Vec::new();
        for candidate in candidates {
            // Deterministic checks always run for the current Snapshot. Only a
            // successfully validated semantic result is reusable.
            let check = ProgramCheck::check(std::slice::from_ref(&candidate));
            result.warnings.extend_from_slice(check.warnings());
            let checks_pass = check.is_pass();
            if checks_pass
                && let Some(previous) = reusable.iter().find(|run| {
                    run.content_path() == candidate.path()
                        && run.content_sha256() == candidate.analysis().file().sha256()
                        && run.policy() == policy
                        && (run.reviewer_report().is_some()
                            || human_answered.contains(candidate.path()))
                })
            {
                reused.insert(candidate.path().clone(), previous.clone());
            } else {
                pending.push(candidate);
            }
        }

        let mut reviewed = evaluator
            .evaluate(pending, reviewer)
            .into_iter()
            .map(|outcome| (outcome.path().clone(), outcome))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut paths = reused
            .keys()
            .chain(reviewed.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            if let Some(previous) = reused.remove(&path) {
                result.document_outcomes.push(previous);
                continue;
            }
            let outcome = reviewed
                .remove(&path)
                .expect("reviewed path came from the stable path set");
            let path = outcome.path().clone();
            let id = id_generator
                .next_id()
                .map_err(|source| PublicPolicyRunError {
                    partial_result: result.clone(),
                    failure: Box::new(PublicPolicyRunFailure::ReviewRunId {
                        path: path.clone(),
                        source,
                    }),
                })?;
            let review_run =
                ReviewRun::from_policy_outcome(id, snapshot, &outcome, policy.clone(), created_at)
                    .map_err(|source| PublicPolicyRunError {
                        partial_result: result.clone(),
                        failure: Box::new(PublicPolicyRunFailure::ReviewRun {
                            path: path.clone(),
                            source,
                        }),
                    })?;

            review_run_store
                .save(&review_run)
                .map_err(|source| PublicPolicyRunError {
                    partial_result: result.clone(),
                    failure: Box::new(PublicPolicyRunFailure::Persistence {
                        path,
                        review_run_id: id,
                        source,
                    }),
                })?;
            result.document_outcomes.push(review_run);
        }

        Ok(result)
    }
}
