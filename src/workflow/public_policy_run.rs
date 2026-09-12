use std::{error::Error, fmt, time::SystemTime};

use super::bounded::bounded_map;
use crate::{
    content::{AssetDependencyGraph, SnapshotMarkdownAnalysisError, SnapshotMarkdownAnalyzer},
    domain::{ContentPath, Snapshot, SnapshotId},
    policy::{
        InvalidPrivacyDocument, PolicyIdentity, PrivacyFilter, PrivateDocument, ProgramCheck,
        ProgramCheckIssue, PublicPolicy, PublicPolicyDecision, ReviewRun, ReviewRunError,
        ReviewRunId, ReviewRunStore, Reviewer,
    },
    storage::LocalContentStore,
};

trait MarkdownReviewEvaluator<R: Reviewer + ?Sized> {
    fn is_bounded(&self) -> bool;
    fn evaluate(
        &self,
        candidates: Vec<crate::policy::PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<crate::policy::PublicPolicyOutcome>;
}

struct SequentialMarkdownReviews;
impl<R: Reviewer + ?Sized> MarkdownReviewEvaluator<R> for SequentialMarkdownReviews {
    fn is_bounded(&self) -> bool {
        false
    }
    fn evaluate(
        &self,
        candidates: Vec<crate::policy::PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<crate::policy::PublicPolicyOutcome> {
        PublicPolicy::evaluate(candidates, reviewer)
    }
}

struct BoundedMarkdownReviews(usize);
impl<R: Reviewer + Sync + ?Sized> MarkdownReviewEvaluator<R> for BoundedMarkdownReviews {
    fn is_bounded(&self) -> bool {
        true
    }
    fn evaluate(
        &self,
        candidates: Vec<crate::policy::PublicCandidateMarkdown>,
        reviewer: &R,
    ) -> Vec<crate::policy::PublicPolicyOutcome> {
        bounded_map(candidates, self.0, &|candidate| {
            PublicPolicy::evaluate(vec![candidate], reviewer)
                .pop()
                .expect("one candidate produces one policy outcome")
        })
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

    #[cfg(test)]
    pub(crate) fn from_document_outcomes_for_test(
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

    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
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
pub enum PublicPolicyRunFailure<StoreError, IdError> {
    MarkdownAnalysis(Vec<SnapshotMarkdownAnalysisError>),
    ReviewCacheLookup(StoreError),
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
pub struct PublicPolicyRunError<StoreError, IdError> {
    partial_result: PublicPolicyRunResult,
    failure: Box<PublicPolicyRunFailure<StoreError, IdError>>,
}

impl<StoreError, IdError> PublicPolicyRunError<StoreError, IdError> {
    pub fn partial_result(&self) -> &PublicPolicyRunResult {
        &self.partial_result
    }

    pub fn failure(&self) -> &PublicPolicyRunFailure<StoreError, IdError> {
        &self.failure
    }
}

impl<StoreError: fmt::Display, IdError: fmt::Display> fmt::Display
    for PublicPolicyRunError<StoreError, IdError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure.as_ref() {
            PublicPolicyRunFailure::MarkdownAnalysis(failures) => write!(
                formatter,
                "public policy run failed to analyze {} Markdown document(s)",
                failures.len()
            ),
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

impl<StoreError, IdError> Error for PublicPolicyRunError<StoreError, IdError>
where
    StoreError: Error + Send + Sync + 'static,
    IdError: Error + Send + Sync + 'static,
{
}

/// Orchestrates the existing immutable-content, privacy, policy, and audit boundaries.
pub struct PublicPolicyRun;

impl PublicPolicyRun {
    pub fn execute<R, S, I>(
        snapshot: &Snapshot,
        content_store: &LocalContentStore,
        reviewer: &R,
        review_run_store: &S,
        policy: &PolicyIdentity,
        id_generator: &mut I,
    ) -> Result<PublicPolicyRunResult, PublicPolicyRunError<S::Error, I::Error>>
    where
        R: Reviewer + ?Sized,
        S: ReviewRunStore + ?Sized,
        I: ReviewRunIdGenerator + ?Sized,
    {
        Self::execute_with_clock(
            snapshot,
            content_store,
            reviewer,
            review_run_store,
            policy,
            id_generator,
            SystemTime::now,
        )
    }

    pub fn execute_bounded<R, S, I>(
        snapshot: &Snapshot,
        content_store: &LocalContentStore,
        reviewer: &R,
        review_run_store: &S,
        policy: &PolicyIdentity,
        id_generator: &mut I,
        concurrency: usize,
    ) -> Result<PublicPolicyRunResult, PublicPolicyRunError<S::Error, I::Error>>
    where
        R: Reviewer + Sync + ?Sized,
        S: ReviewRunStore + ?Sized,
        I: ReviewRunIdGenerator + ?Sized,
    {
        Self::execute_with_clock_and_evaluator(
            snapshot,
            content_store,
            reviewer,
            review_run_store,
            policy,
            id_generator,
            SystemTime::now,
            BoundedMarkdownReviews(concurrency),
        )
    }

    fn execute_with_clock<R, S, I, C>(
        snapshot: &Snapshot,
        content_store: &LocalContentStore,
        reviewer: &R,
        review_run_store: &S,
        policy: &PolicyIdentity,
        id_generator: &mut I,
        clock: C,
    ) -> Result<PublicPolicyRunResult, PublicPolicyRunError<S::Error, I::Error>>
    where
        R: Reviewer + ?Sized,
        S: ReviewRunStore + ?Sized,
        I: ReviewRunIdGenerator + ?Sized,
        C: FnMut() -> SystemTime,
    {
        Self::execute_with_clock_and_evaluator(
            snapshot,
            content_store,
            reviewer,
            review_run_store,
            policy,
            id_generator,
            clock,
            SequentialMarkdownReviews,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_with_clock_and_evaluator<R, S, I, C, E>(
        snapshot: &Snapshot,
        content_store: &LocalContentStore,
        reviewer: &R,
        review_run_store: &S,
        policy: &PolicyIdentity,
        id_generator: &mut I,
        mut clock: C,
        evaluator: E,
    ) -> Result<PublicPolicyRunResult, PublicPolicyRunError<S::Error, I::Error>>
    where
        R: Reviewer + ?Sized,
        S: ReviewRunStore + ?Sized,
        I: ReviewRunIdGenerator + ?Sized,
        C: FnMut() -> SystemTime,
        E: MarkdownReviewEvaluator<R>,
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

        if !evaluator.is_bounded() {
            for candidate in candidates {
                let check = ProgramCheck::check(std::slice::from_ref(&candidate));
                result.warnings.extend_from_slice(check.warnings());
                if check.is_pass()
                    && let Some(previous) = reusable.iter().find(|run| {
                        run.content_path() == candidate.path()
                            && run.content_sha256() == candidate.analysis().file().sha256()
                            && run.policy() == policy
                            && run.reviewer_report().is_some()
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
                let review_run =
                    ReviewRun::from_policy_outcome(id, snapshot, &outcome, policy.clone(), clock())
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
                        && run.reviewer_report().is_some()
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
                ReviewRun::from_policy_outcome(id, snapshot, &outcome, policy.clone(), clock())
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

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime},
    };

    use crate::{
        content::Resolution,
        domain::{Sha256, SnapshotFile, SourceId},
        policy::{
            HumanReviewReason, PrivateReason, ReviewCandidate, ReviewDecision, ReviewReasonCode,
            ReviewerError, ReviewerReport,
        },
        source::LocalSource,
        storage::SqliteReviewRunStore,
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-public-policy-run-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn source_path(&self) -> PathBuf {
            self.0.join("source")
        }

        fn store(&self) -> LocalContentStore {
            LocalContentStore::new(self.0.join("content-store"))
        }

        fn database(&self) -> PathBuf {
            self.0.join("reviews.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct RecordingReviewer {
        calls: RefCell<Vec<String>>,
        responses: BTreeMap<String, Result<ReviewDecision, ReviewerError>>,
    }

    impl RecordingReviewer {
        fn approving(paths: impl IntoIterator<Item = &'static str>) -> Self {
            Self::with_responses(
                paths
                    .into_iter()
                    .map(|path| (path, Ok(ReviewDecision::Approve))),
            )
        }

        fn with_responses(
            responses: impl IntoIterator<Item = (&'static str, Result<ReviewDecision, ReviewerError>)>,
        ) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: responses
                    .into_iter()
                    .map(|(path, response)| (path.to_owned(), response))
                    .collect(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Reviewer for RecordingReviewer {
        fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError> {
            let path = candidate.path().as_str().to_owned();
            self.calls.borrow_mut().push(path.clone());
            match self
                .responses
                .get(&path)
                .cloned()
                .expect("configured reviewer response")
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

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestStoreError;

    impl fmt::Display for TestStoreError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected persistence failure")
        }
    }

    impl Error for TestStoreError {}

    #[derive(Debug, Default)]
    struct RecordingStore {
        save_calls: Cell<usize>,
        fail_on_save: Option<usize>,
        saved: RefCell<Vec<ReviewRun>>,
    }

    impl RecordingStore {
        fn failing_on(save_call: usize) -> Self {
            Self {
                save_calls: Cell::new(0),
                fail_on_save: Some(save_call),
                saved: RefCell::new(Vec::new()),
            }
        }

        fn saved(&self) -> Vec<ReviewRun> {
            self.saved.borrow().clone()
        }
    }

    impl ReviewRunStore for RecordingStore {
        type Error = TestStoreError;

        fn save(&self, run: &ReviewRun) -> Result<(), Self::Error> {
            let call = self.save_calls.get() + 1;
            self.save_calls.set(call);
            if self.fail_on_save == Some(call) {
                return Err(TestStoreError);
            }
            self.saved.borrow_mut().push(run.clone());
            Ok(())
        }

        fn get(&self, id: ReviewRunId) -> Result<Option<ReviewRun>, Self::Error> {
            Ok(self
                .saved
                .borrow()
                .iter()
                .find(|run| run.id() == id)
                .cloned())
        }

        fn list_by_snapshot(&self, snapshot_id: SnapshotId) -> Result<Vec<ReviewRun>, Self::Error> {
            let mut runs = self
                .saved
                .borrow()
                .iter()
                .filter(|run| run.snapshot_id() == snapshot_id)
                .cloned()
                .collect::<Vec<_>>();
            runs.sort_by(|left, right| left.content_path().cmp(right.content_path()));
            Ok(runs)
        }

        fn list_pending_human_review(&self) -> Result<Vec<ReviewRun>, Self::Error> {
            Ok(self
                .saved
                .borrow()
                .iter()
                .filter(|run| run.needs_human_review())
                .cloned()
                .collect())
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn policy() -> PolicyIdentity {
        PolicyIdentity::new("public", "public-v1", Sha256::new([42; 32])).unwrap()
    }

    fn snapshot_with_content(
        store: &LocalContentStore,
        entries: impl IntoIterator<Item = (&'static str, &'static [u8])>,
    ) -> Snapshot {
        let files = entries
            .into_iter()
            .map(|(file_path, content)| {
                let sha256 = store.store(content).unwrap();
                SnapshotFile::new(path(file_path), content.len() as u64, sha256, None)
            })
            .collect();
        Snapshot::new(
            SnapshotId::new(7).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-vault").unwrap(),
            files,
        )
        .unwrap()
    }

    fn execute_with_fixed_clock<R: Reviewer + ?Sized, S: ReviewRunStore + ?Sized>(
        snapshot: &Snapshot,
        content_store: &LocalContentStore,
        reviewer: &R,
        review_run_store: &S,
        first_id: u64,
    ) -> Result<
        PublicPolicyRunResult,
        PublicPolicyRunError<S::Error, SequentialReviewRunIdGeneratorError>,
    > {
        let mut ids = SequentialReviewRunIdGenerator::new(ReviewRunId::new(first_id).unwrap());
        PublicPolicyRun::execute_with_clock(
            snapshot,
            content_store,
            reviewer,
            review_run_store,
            &policy(),
            &mut ids,
            || SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        )
    }

    #[test]
    fn complete_snapshot_is_filtered_checked_reviewed_persisted_and_reported() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot_with_content(
            &store,
            [
                ("z-rejected.md", b"body" as &[u8]),
                ("private.md", b"![[secret.png]]"),
                ("secret.png", b"secret bytes"),
                ("c-program-issue.md", b"![[missing.png]]"),
                ("b-invalid.md", b"---\nprivate: maybe\n---\nbody"),
                ("a-approved.md", b"body"),
                ("unused.bin", b"not markdown"),
            ],
        );
        let reviewer = RecordingReviewer::with_responses([
            ("a-approved.md", Ok(ReviewDecision::Approve)),
            ("z-rejected.md", Ok(ReviewDecision::Reject)),
        ]);
        let review_store = RecordingStore::default();

        let result = execute_with_fixed_clock(&snapshot, &store, &reviewer, &review_store, 10)
            .expect("run succeeds");

        assert_eq!(result.snapshot_id(), snapshot.id());
        assert_eq!(result.private_documents().len(), 1);
        assert_eq!(result.private_documents()[0].path(), &path("private.md"));
        assert_eq!(
            result.private_documents()[0].reasons(),
            [PrivateReason::PathContainsPrivateMarker]
        );
        assert_eq!(result.invalid_privacy_documents().len(), 1);
        assert_eq!(
            result.invalid_privacy_documents()[0].path(),
            &path("b-invalid.md")
        );
        assert!(
            result.invalid_privacy_documents()[0]
                .reason()
                .contains("private")
        );
        assert_eq!(
            reviewer.calls(),
            ["a-approved.md".to_owned(), "z-rejected.md".to_owned()]
        );
        assert_eq!(
            result
                .document_outcomes()
                .iter()
                .map(|run| (run.content_path().as_str(), run.id().get()))
                .collect::<Vec<_>>(),
            [
                ("a-approved.md", 10),
                ("c-program-issue.md", 11),
                ("z-rejected.md", 12),
            ]
        );
        assert!(matches!(
            result.document_outcomes()[1].decision(),
            PublicPolicyDecision::ProgramIssues(issues)
                if issues.len() == 1 && issues[0].document_path() == &path("c-program-issue.md")
        ));
        assert_eq!(result.approved_markdown_paths(), [&path("a-approved.md")]);
        assert_eq!(review_store.saved(), result.document_outcomes());
    }

    #[test]
    fn reviewer_error_is_saved_as_fail_closed_human_review() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot_with_content(&store, [("article.md", b"body" as &[u8])]);
        let reviewer = RecordingReviewer::with_responses([(
            "article.md",
            Err(ReviewerError::new("provider unavailable")),
        )]);
        let review_store = RecordingStore::default();

        let result = execute_with_fixed_clock(&snapshot, &store, &reviewer, &review_store, 20)
            .expect("reviewer failure is a policy decision, not a run failure");

        assert!(matches!(
            result.document_outcomes()[0].decision(),
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(error))
                if error.message() == "provider unavailable"
        ));
        assert!(result.approved_markdown_paths().is_empty());
        assert_eq!(review_store.saved(), result.document_outcomes());
    }

    #[test]
    fn complete_run_persists_each_outcome_through_sqlite_store() {
        let directory = TestDirectory::new();
        let content_store = directory.store();
        let snapshot = snapshot_with_content(
            &content_store,
            [
                ("approved.md", b"body" as &[u8]),
                ("issue.md", b"![[missing.png]]"),
            ],
        );
        let reviewer = RecordingReviewer::approving(["approved.md"]);
        let review_store = SqliteReviewRunStore::open(directory.database()).unwrap();

        let result =
            execute_with_fixed_clock(&snapshot, &content_store, &reviewer, &review_store, 25)
                .expect("run succeeds");

        assert_eq!(
            review_store.list_by_snapshot(snapshot.id()).unwrap(),
            result.document_outcomes()
        );
    }

    #[test]
    fn validated_semantic_result_is_reused_for_the_same_snapshot_and_contract() {
        let directory = TestDirectory::new();
        let content_store = directory.store();
        let snapshot = snapshot_with_content(&content_store, [("article.md", b"body" as &[u8])]);
        let review_store = SqliteReviewRunStore::open(directory.database()).unwrap();
        let first_reviewer = RecordingReviewer::approving(["article.md"]);
        let first = execute_with_fixed_clock(
            &snapshot,
            &content_store,
            &first_reviewer,
            &review_store,
            40,
        )
        .unwrap();
        assert_eq!(first_reviewer.calls(), ["article.md"]);

        let second_reviewer = RecordingReviewer::with_responses([]);
        let second = execute_with_fixed_clock(
            &snapshot,
            &content_store,
            &second_reviewer,
            &review_store,
            41,
        )
        .unwrap();

        assert!(second_reviewer.calls().is_empty());
        assert_eq!(second.document_outcomes(), first.document_outcomes());
        assert_eq!(
            review_store.list_by_snapshot(snapshot.id()).unwrap().len(),
            1
        );
    }

    #[test]
    fn all_analysis_failures_abort_before_filter_policy_or_persistence() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let invalid_utf8_sha = store.store(&[0xff, 0xfe]).unwrap();
        let missing_sha = Sha256::digest(b"missing");
        let snapshot = Snapshot::new(
            SnapshotId::new(8).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-vault").unwrap(),
            vec![
                SnapshotFile::new(path("invalid.md"), 2, invalid_utf8_sha, None),
                SnapshotFile::new(path("missing.md"), 7, missing_sha, None),
            ],
        )
        .unwrap();
        let reviewer = RecordingReviewer::with_responses([]);
        let review_store = RecordingStore::default();

        let error = execute_with_fixed_clock(&snapshot, &store, &reviewer, &review_store, 30)
            .expect_err("analysis must fail closed");

        assert!(matches!(
            error.failure(),
            PublicPolicyRunFailure::MarkdownAnalysis(failures)
                if failures.len() == 2
                    && matches!(&failures[0], SnapshotMarkdownAnalysisError::InvalidUtf8 { path: actual, .. } if actual == &path("invalid.md"))
                    && matches!(&failures[1], SnapshotMarkdownAnalysisError::ContentStore { path: actual, .. } if actual == &path("missing.md"))
        ));
        assert!(error.partial_result().document_outcomes().is_empty());
        assert!(reviewer.calls().is_empty());
        assert!(review_store.saved().is_empty());
    }

    #[test]
    fn persistence_failure_returns_only_previously_saved_partial_completion() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot_with_content(
            &store,
            [
                ("private.md", b"body" as &[u8]),
                ("a.md", b"body"),
                ("b.md", b"body"),
                ("c.md", b"body"),
            ],
        );
        let reviewer = RecordingReviewer::approving(["a.md", "b.md", "c.md"]);
        let review_store = RecordingStore::failing_on(2);

        let error = execute_with_fixed_clock(&snapshot, &store, &reviewer, &review_store, 100)
            .expect_err("second save fails the run");

        assert!(matches!(
            error.failure(),
            PublicPolicyRunFailure::Persistence {
                path: failed_path,
                review_run_id,
                source: TestStoreError,
            } if failed_path == &path("b.md") && review_run_id.get() == 101
        ));
        assert_eq!(error.partial_result().private_documents().len(), 1);
        assert_eq!(
            error
                .partial_result()
                .document_outcomes()
                .iter()
                .map(|run| (run.content_path().as_str(), run.id().get()))
                .collect::<Vec<_>>(),
            [("a.md", 100)]
        );
        assert_eq!(
            review_store.saved(),
            error.partial_result().document_outcomes()
        );
        assert_eq!(reviewer.calls(), ["a.md".to_owned(), "b.md".to_owned()]);
    }

    #[test]
    fn run_reads_immutable_snapshot_content_after_source_is_changed_and_deleted() {
        let directory = TestDirectory::new();
        fs::create_dir_all(directory.source_path()).unwrap();
        fs::write(directory.source_path().join("public.md"), b"snapshot body").unwrap();
        fs::write(directory.source_path().join("deleted.md"), b"snapshot body").unwrap();
        let store = directory.store();
        let snapshot = LocalSource::new(
            directory.source_path(),
            SourceId::new("local-vault").unwrap(),
            store.clone(),
        )
        .snapshot(SnapshotId::new(9).unwrap(), SystemTime::UNIX_EPOCH)
        .unwrap();

        fs::write(
            directory.source_path().join("public.md"),
            b"---\nprivate: true\n---\nchanged body",
        )
        .unwrap();
        fs::remove_file(directory.source_path().join("deleted.md")).unwrap();
        let reviewer = RecordingReviewer::approving(["deleted.md", "public.md"]);
        let review_store = RecordingStore::default();

        let result = execute_with_fixed_clock(&snapshot, &store, &reviewer, &review_store, 200)
            .expect("CAS content remains available");

        assert_eq!(
            reviewer.calls(),
            ["deleted.md".to_owned(), "public.md".to_owned()]
        );
        assert!(result.private_documents().is_empty());
        assert_eq!(result.document_outcomes().len(), 2);
        assert!(
            result
                .document_outcomes()
                .iter()
                .all(|run| run.content_sha256() == Sha256::digest(b"snapshot body"))
        );
    }

    #[test]
    fn sequential_generator_reports_exhaustion_without_reusing_an_id() {
        let mut generator =
            SequentialReviewRunIdGenerator::new(ReviewRunId::new(u64::MAX).unwrap());

        assert_eq!(generator.next_id().unwrap().get(), u64::MAX);
        assert_eq!(
            generator.next_id().unwrap_err(),
            SequentialReviewRunIdGeneratorError::Exhausted
        );
    }

    #[test]
    fn private_asset_reference_never_reaches_reviewer_or_document_outcomes() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot_with_content(
            &store,
            [
                ("private.md", b"![[secret.png]]" as &[u8]),
                ("secret.png", b"secret"),
            ],
        );
        let reviewer = RecordingReviewer::with_responses([]);
        let review_store = RecordingStore::default();

        let result = execute_with_fixed_clock(&snapshot, &store, &reviewer, &review_store, 300)
            .expect("private-only snapshot completes without review");

        assert_eq!(result.private_documents().len(), 1);
        assert!(result.document_outcomes().is_empty());
        assert!(reviewer.calls().is_empty());
        assert!(review_store.saved().is_empty());

        let analysis = SnapshotMarkdownAnalyzer::new(store)
            .analyze(&snapshot, &path("private.md"))
            .unwrap();
        assert!(matches!(
            analysis.references()[0].resolution(),
            Resolution::ResolvedAsset { path: asset } if asset == &path("secret.png")
        ));
    }
}
