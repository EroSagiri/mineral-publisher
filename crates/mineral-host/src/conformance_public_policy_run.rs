//! Adapter conformance tests for the portable public policy run engine.
//!
//! These tests exercise the engine against the real native filesystem and
//! SQLite adapters and temporary directories, so they live in the host crate.

use std::{error::Error, fmt};

use crate::content::{SnapshotMarkdownAnalysisError, SnapshotMarkdownAnalyzer};
use crate::domain::{ContentPath, Snapshot, SnapshotId};
use crate::policy::{
    PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId, ReviewRunStore, Reviewer,
};
use crate::workflow::*;

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
        storage::{SqliteAssetReviewRunStore, SqliteHumanReviewStore, SqliteReviewRunStore},
    };

    use super::*;
    use crate::storage::LocalContentStore;

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

        fn human_database(&self) -> PathBuf {
            self.0.join("human-reviews.sqlite3")
        }

        fn asset_database(&self) -> PathBuf {
            self.0.join("asset-reviews.sqlite3")
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

    fn execute_with_fixed_clock<
        R: Reviewer + ?Sized,
        S: ReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
    >(
        snapshot: &Snapshot,
        content_store: &LocalContentStore,
        reviewer: &R,
        review_run_store: &S,
        human_reviews: &H,
        first_id: u64,
    ) -> Result<
        PublicPolicyRunResult,
        PublicPolicyRunError<S::Error, SequentialReviewRunIdGeneratorError, H::Error>,
    > {
        let mut ids = SequentialReviewRunIdGenerator::new(ReviewRunId::new(first_id).unwrap());
        PublicPolicyRun::execute_at(
            snapshot,
            content_store,
            reviewer,
            review_run_store,
            human_reviews,
            &policy(),
            &mut ids,
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            &SequentialMarkdownReviews,
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

        let result = execute_with_fixed_clock(
            &snapshot,
            &store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            10,
        )
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

        let result = execute_with_fixed_clock(
            &snapshot,
            &store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            20,
        )
        .expect("reviewer failure is a policy decision, not a run failure");

        assert!(matches!(
            result.document_outcomes()[0].decision(),
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(error))
                if error.message() == "provider unavailable"
        ));
        assert!(result.approved_markdown_paths().is_empty());
        assert_eq!(review_store.saved(), result.document_outcomes());
    }

    /// S6.5 acceptance: a provider outage asks a human, and that human's answer is
    /// reused by every later attempt of the same content under the same policy,
    /// without calling the provider again.
    ///
    /// This is the chain the old attempt-bound decision could not complete: each
    /// re-run minted a fresh attempt identity, so the approval could never be
    /// recognised and the provider — the very thing that was down — was asked
    /// again on every publication.
    #[test]
    fn an_approved_subject_is_never_sent_to_the_provider_again() {
        let directory = TestDirectory::new();
        let content_store = directory.store();
        let snapshot = snapshot_with_content(&content_store, [("article.md", b"body" as &[u8])]);
        let documents = SqliteReviewRunStore::open(directory.database()).unwrap();
        let human = SqliteHumanReviewStore::open(directory.human_database()).unwrap();
        let assets = SqliteAssetReviewRunStore::open(directory.asset_database()).unwrap();
        let reviewer = RecordingReviewer::with_responses([(
            "article.md",
            Err(ReviewerError::new("provider unavailable")),
        )]);

        // The provider is unavailable: the attempt records that fact, and the
        // question is asked by a human instead.
        let first =
            execute_with_fixed_clock(&snapshot, &content_store, &reviewer, &documents, &human, 1)
                .unwrap();
        assert_eq!(reviewer.calls(), ["article.md"]);
        let attempt = first.document_outcomes()[0].clone();
        assert!(attempt.needs_human_review());
        assert!(attempt.reviewer_report().is_none());
        assert_eq!(
            HumanReviewResolution::list_pending_documents(&documents, &human)
                .unwrap()
                .len(),
            1
        );

        // The operator answers the attempt that raised the question. The decision is
        // recorded against the reviewed content and the policy, which is what a
        // later attempt can recognise.
        HumanReviewResolution::resolve_document(
            &documents,
            &human,
            HumanReviewId::new(1).unwrap(),
            attempt.id(),
            HumanReviewDecision::Approve,
            SystemTime::UNIX_EPOCH + Duration::from_secs(30),
            None,
            None,
        )
        .unwrap();
        assert!(
            HumanReviewResolution::list_pending_documents(&documents, &human)
                .unwrap()
                .is_empty()
        );

        // The next publication reuses the same attempt and never calls the provider,
        // so a provider that stays down cannot block the delivery.
        let second =
            execute_with_fixed_clock(&snapshot, &content_store, &reviewer, &documents, &human, 2)
                .unwrap();
        assert_eq!(
            reviewer.calls(),
            ["article.md"],
            "an approved subject must not reach the provider again"
        );
        assert_eq!(
            second.document_outcomes()[0].id(),
            attempt.id(),
            "the durable attempt is reused rather than re-minted"
        );

        // And the delivery is no longer waiting on a human.
        let effective = EffectiveReviewSet::build(
            &second,
            &AssetReviewWorkflowResult::empty(snapshot.id()),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            effective.documents()[0].decision(),
            EffectiveDocumentDecision::Approved
        );
        assert!(!effective.has_pending_review());
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

        let result = execute_with_fixed_clock(
            &snapshot,
            &content_store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            25,
        )
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
            &NoHumanReviews,
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
            &NoHumanReviews,
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

        let error = execute_with_fixed_clock(
            &snapshot,
            &store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            30,
        )
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

        let error = execute_with_fixed_clock(
            &snapshot,
            &store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            100,
        )
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

        let result = execute_with_fixed_clock(
            &snapshot,
            &store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            200,
        )
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

        let result = execute_with_fixed_clock(
            &snapshot,
            &store,
            &reviewer,
            &review_store,
            &NoHumanReviews,
            300,
        )
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
