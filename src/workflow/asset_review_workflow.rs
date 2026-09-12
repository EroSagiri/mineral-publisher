use std::{error::Error, fmt, time::SystemTime};

use crate::{
    domain::{ContentPath, Snapshot, SnapshotId},
    policy::PolicyIdentity,
    storage::LocalContentStore,
};

use super::bounded::bounded_map;
use super::{
    AssetCheckResult, AssetPolicy, AssetProgramCheck, AssetProgramCheckError, AssetReviewDecision,
    AssetReviewDisposition, AssetReviewOutcome, AssetReviewRun, AssetReviewRunError,
    AssetReviewRunId, AssetReviewRunStore, AssetReviewer, CandidateAssetSet,
};

trait AssetReviewEvaluator<R: AssetReviewer + ?Sized> {
    fn is_bounded(&self) -> bool;
    fn evaluate(
        &self,
        outcomes: Vec<super::AssetPolicyOutcome>,
        reviewer: &R,
    ) -> Vec<AssetReviewOutcome>;
}

struct SequentialAssetReviews;
impl<R: AssetReviewer + ?Sized> AssetReviewEvaluator<R> for SequentialAssetReviews {
    fn is_bounded(&self) -> bool {
        false
    }
    fn evaluate(
        &self,
        outcomes: Vec<super::AssetPolicyOutcome>,
        reviewer: &R,
    ) -> Vec<AssetReviewOutcome> {
        outcomes
            .into_iter()
            .map(|outcome| outcome.review(reviewer))
            .collect()
    }
}

struct BoundedAssetReviews(usize);
impl<R: AssetReviewer + Sync + ?Sized> AssetReviewEvaluator<R> for BoundedAssetReviews {
    fn is_bounded(&self) -> bool {
        true
    }
    fn evaluate(
        &self,
        outcomes: Vec<super::AssetPolicyOutcome>,
        reviewer: &R,
    ) -> Vec<AssetReviewOutcome> {
        bounded_map(outcomes, self.0, &|outcome| outcome.review(reviewer))
    }
}

/// Allocates immutable asset-review audit identities at the workflow boundary.
pub trait AssetReviewRunIdGenerator {
    type Error: Error + Send + Sync + 'static;

    fn next_id(&mut self) -> Result<AssetReviewRunId, Self::Error>;
}

/// A small deterministic allocator for callers that own an asset-review ID range.
#[derive(Clone, Debug)]
pub struct SequentialAssetReviewRunIdGenerator {
    next: Option<u64>,
}

impl SequentialAssetReviewRunIdGenerator {
    pub fn new(first: AssetReviewRunId) -> Self {
        Self {
            next: Some(first.get()),
        }
    }
}

impl AssetReviewRunIdGenerator for SequentialAssetReviewRunIdGenerator {
    type Error = SequentialAssetReviewRunIdGeneratorError;

    fn next_id(&mut self) -> Result<AssetReviewRunId, Self::Error> {
        let value = self
            .next
            .ok_or(SequentialAssetReviewRunIdGeneratorError::Exhausted)?;
        self.next = value.checked_add(1);
        AssetReviewRunId::new(value)
            .map_err(|_| SequentialAssetReviewRunIdGeneratorError::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequentialAssetReviewRunIdGeneratorError {
    Exhausted,
}

impl fmt::Display for SequentialAssetReviewRunIdGeneratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset review run id sequence is exhausted")
    }
}

impl Error for SequentialAssetReviewRunIdGeneratorError {}

/// One durable asset-review outcome, ordered by content path in a successful result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewWorkflowEntry {
    content_path: ContentPath,
    review_run_id: AssetReviewRunId,
    outcome: AssetReviewOutcome,
}

impl AssetReviewWorkflowEntry {
    pub fn content_path(&self) -> &ContentPath {
        &self.content_path
    }

    pub fn review_run_id(&self) -> AssetReviewRunId {
        self.review_run_id
    }

    pub fn outcome(&self) -> &AssetReviewOutcome {
        &self.outcome
    }
}

/// Successful durable prefix of one complete candidate-asset review workflow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewWorkflowResult {
    snapshot_id: SnapshotId,
    entries: Vec<AssetReviewWorkflowEntry>,
    checks: AssetCheckResult,
}

impl AssetReviewWorkflowResult {
    pub fn empty(snapshot_id: SnapshotId) -> Self {
        Self {
            snapshot_id,
            entries: Vec::new(),
            checks: AssetCheckResult::empty(snapshot_id),
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    /// Durably saved outcomes in deterministic `ContentPath` order.
    pub fn entries(&self) -> &[AssetReviewWorkflowEntry] {
        &self.entries
    }

    pub fn checks(&self) -> &AssetCheckResult {
        &self.checks
    }

    /// A convenience view only; it is not a final asset set or publication decision.
    pub fn approved_asset_paths(&self) -> Vec<&ContentPath> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.outcome.disposition(),
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve)
                )
            })
            .map(AssetReviewWorkflowEntry::content_path)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn from_entries_for_test(
        snapshot_id: SnapshotId,
        mut entries: Vec<AssetReviewWorkflowEntry>,
    ) -> Self {
        entries.sort_by(|left, right| left.content_path.cmp(&right.content_path));
        Self {
            snapshot_id,
            entries,
            checks: AssetCheckResult::empty(snapshot_id),
        }
    }
}

#[cfg(test)]
impl AssetReviewWorkflowEntry {
    pub(crate) fn from_parts_for_test(
        content_path: ContentPath,
        review_run_id: AssetReviewRunId,
        outcome: AssetReviewOutcome,
    ) -> Self {
        Self {
            content_path,
            review_run_id,
            outcome,
        }
    }
}

/// The boundary that stopped a workflow before it had a complete durable result.
#[derive(Debug)]
pub enum AssetReviewWorkflowFailure<StoreError, IdError> {
    SnapshotMismatch {
        candidate_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    ProgramCheck(AssetProgramCheckError),
    ReviewCacheLookup(StoreError),
    ReviewRunId {
        path: ContentPath,
        source: IdError,
    },
    ReviewRun {
        path: ContentPath,
        source: AssetReviewRunError,
    },
    Persistence {
        path: ContentPath,
        review_run_id: AssetReviewRunId,
        source: StoreError,
    },
}

/// A failure together with the durable prefix already saved before it.
#[derive(Debug)]
pub struct AssetReviewWorkflowError<StoreError, IdError> {
    partial_result: AssetReviewWorkflowResult,
    failure: Box<AssetReviewWorkflowFailure<StoreError, IdError>>,
}

impl<StoreError, IdError> AssetReviewWorkflowError<StoreError, IdError> {
    pub fn partial_result(&self) -> &AssetReviewWorkflowResult {
        &self.partial_result
    }

    pub fn failure(&self) -> &AssetReviewWorkflowFailure<StoreError, IdError> {
        &self.failure
    }
}

impl<StoreError: fmt::Display, IdError: fmt::Display> fmt::Display
    for AssetReviewWorkflowError<StoreError, IdError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure.as_ref() {
            AssetReviewWorkflowFailure::SnapshotMismatch { .. } => {
                formatter.write_str("candidate asset snapshot does not match review snapshot")
            }
            AssetReviewWorkflowFailure::ProgramCheck(source) => {
                write!(formatter, "asset program check failed: {source}")
            }
            AssetReviewWorkflowFailure::ReviewCacheLookup(source) => {
                write!(
                    formatter,
                    "could not look up reusable Asset ReviewResults: {source}"
                )
            }
            AssetReviewWorkflowFailure::ReviewRunId { path, source } => {
                write!(
                    formatter,
                    "could not allocate AssetReviewRunId for {path}: {source}"
                )
            }
            AssetReviewWorkflowFailure::ReviewRun { path, source } => {
                write!(
                    formatter,
                    "could not create AssetReviewRun for {path}: {source}"
                )
            }
            AssetReviewWorkflowFailure::Persistence { path, source, .. } => {
                write!(
                    formatter,
                    "could not persist AssetReviewRun for {path}: {source}"
                )
            }
        }
    }
}

impl<StoreError, IdError> Error for AssetReviewWorkflowError<StoreError, IdError>
where
    StoreError: Error + Send + Sync + 'static,
    IdError: Error + Send + Sync + 'static,
{
}

/// Composes candidate selection output with existing checking, policy, reviewer, and audit boundaries.
pub struct AssetReviewWorkflow;

/// Immutable inputs shared by every asset reviewed in one workflow run.
pub struct AssetReviewWorkflowInput<'a> {
    candidates: &'a CandidateAssetSet,
    snapshot: &'a Snapshot,
    content_store: &'a LocalContentStore,
    policy: &'a PolicyIdentity,
}

impl<'a> AssetReviewWorkflowInput<'a> {
    pub fn new(
        candidates: &'a CandidateAssetSet,
        snapshot: &'a Snapshot,
        content_store: &'a LocalContentStore,
        policy: &'a PolicyIdentity,
    ) -> Self {
        Self {
            candidates,
            snapshot,
            content_store,
            policy,
        }
    }
}

impl AssetReviewWorkflow {
    pub fn execute<R, S, I>(
        input: AssetReviewWorkflowInput<'_>,
        reviewer: &R,
        review_run_store: &S,
        id_generator: &mut I,
    ) -> Result<AssetReviewWorkflowResult, AssetReviewWorkflowError<S::Error, I::Error>>
    where
        R: AssetReviewer + ?Sized,
        S: AssetReviewRunStore + ?Sized,
        I: AssetReviewRunIdGenerator + ?Sized,
    {
        Self::execute_with_clock(
            input,
            reviewer,
            review_run_store,
            id_generator,
            SystemTime::now,
        )
    }

    pub fn execute_bounded<R, S, I>(
        input: AssetReviewWorkflowInput<'_>,
        reviewer: &R,
        review_run_store: &S,
        id_generator: &mut I,
        concurrency: usize,
    ) -> Result<AssetReviewWorkflowResult, AssetReviewWorkflowError<S::Error, I::Error>>
    where
        R: AssetReviewer + Sync + ?Sized,
        S: AssetReviewRunStore + ?Sized,
        I: AssetReviewRunIdGenerator + ?Sized,
    {
        Self::execute_with_clock_and_evaluator(
            input,
            reviewer,
            review_run_store,
            id_generator,
            SystemTime::now,
            BoundedAssetReviews(concurrency),
        )
    }

    fn execute_with_clock<R, S, I, C>(
        input: AssetReviewWorkflowInput<'_>,
        reviewer: &R,
        review_run_store: &S,
        id_generator: &mut I,
        clock: C,
    ) -> Result<AssetReviewWorkflowResult, AssetReviewWorkflowError<S::Error, I::Error>>
    where
        R: AssetReviewer + ?Sized,
        S: AssetReviewRunStore + ?Sized,
        I: AssetReviewRunIdGenerator + ?Sized,
        C: FnMut() -> SystemTime,
    {
        Self::execute_with_clock_and_evaluator(
            input,
            reviewer,
            review_run_store,
            id_generator,
            clock,
            SequentialAssetReviews,
        )
    }

    fn execute_with_clock_and_evaluator<R, S, I, C, E>(
        input: AssetReviewWorkflowInput<'_>,
        reviewer: &R,
        review_run_store: &S,
        id_generator: &mut I,
        mut clock: C,
        evaluator: E,
    ) -> Result<AssetReviewWorkflowResult, AssetReviewWorkflowError<S::Error, I::Error>>
    where
        R: AssetReviewer + ?Sized,
        S: AssetReviewRunStore + ?Sized,
        I: AssetReviewRunIdGenerator + ?Sized,
        C: FnMut() -> SystemTime,
        E: AssetReviewEvaluator<R>,
    {
        let mut result = AssetReviewWorkflowResult::empty(input.snapshot.id());
        if input.candidates.snapshot_id() != input.snapshot.id() {
            return Err(AssetReviewWorkflowError {
                partial_result: result,
                failure: Box::new(AssetReviewWorkflowFailure::SnapshotMismatch {
                    candidate_snapshot_id: input.candidates.snapshot_id(),
                    snapshot_id: input.snapshot.id(),
                }),
            });
        }

        let checked = AssetProgramCheck::new(input.content_store.clone())
            .run(input.candidates, input.snapshot)
            .map_err(|source| AssetReviewWorkflowError {
                partial_result: result.clone(),
                failure: Box::new(AssetReviewWorkflowFailure::ProgramCheck(source)),
            })?;
        result.checks = checked.clone();
        let policy_result = AssetPolicy::evaluate(&checked);
        let reusable = review_run_store
            .list_by_snapshot(input.snapshot.id())
            .map_err(|source| AssetReviewWorkflowError {
                partial_result: result.clone(),
                failure: Box::new(AssetReviewWorkflowFailure::ReviewCacheLookup(source)),
            })?;

        if !evaluator.is_bounded() {
            for policy_outcome in policy_result.outcomes() {
                let current_sha256 = input
                    .snapshot
                    .files()
                    .iter()
                    .find(|file| file.path() == policy_outcome.path())
                    .map(|file| file.sha256());
                if let Some(previous) = reusable.iter().find(|run| {
                    run.content_path() == policy_outcome.path()
                        && current_sha256.is_some_and(|sha| sha == run.content_sha256())
                        && run.policy() == input.policy
                        && (!run.reviewer_was_called() || run.outcome().reviewer_report().is_some())
                }) {
                    result.entries.push(AssetReviewWorkflowEntry {
                        content_path: previous.content_path().clone(),
                        review_run_id: previous.id(),
                        outcome: previous.outcome().clone(),
                    });
                    continue;
                }
                let outcome = policy_outcome.review(reviewer);
                let path = outcome.path().clone();
                let id = id_generator
                    .next_id()
                    .map_err(|source| AssetReviewWorkflowError {
                        partial_result: result.clone(),
                        failure: Box::new(AssetReviewWorkflowFailure::ReviewRunId {
                            path: path.clone(),
                            source,
                        }),
                    })?;
                let run = AssetReviewRun::from_review_outcome(
                    id,
                    input.snapshot,
                    outcome.clone(),
                    input.policy.clone(),
                    clock(),
                )
                .map_err(|source| AssetReviewWorkflowError {
                    partial_result: result.clone(),
                    failure: Box::new(AssetReviewWorkflowFailure::ReviewRun {
                        path: path.clone(),
                        source,
                    }),
                })?;
                review_run_store
                    .save(&run)
                    .map_err(|source| AssetReviewWorkflowError {
                        partial_result: result.clone(),
                        failure: Box::new(AssetReviewWorkflowFailure::Persistence {
                            path: path.clone(),
                            review_run_id: id,
                            source,
                        }),
                    })?;
                result.entries.push(AssetReviewWorkflowEntry {
                    content_path: path,
                    review_run_id: id,
                    outcome,
                });
            }
            return Ok(result);
        }

        let mut reused = std::collections::BTreeMap::new();
        let mut pending = Vec::new();
        for policy_outcome in policy_result.outcomes() {
            let current_sha256 = input
                .snapshot
                .files()
                .iter()
                .find(|file| file.path() == policy_outcome.path())
                .map(|file| file.sha256());
            if let Some(previous) = reusable.iter().find(|run| {
                run.content_path() == policy_outcome.path()
                    && current_sha256.is_some_and(|sha| sha == run.content_sha256())
                    && run.policy() == input.policy
                    && (!run.reviewer_was_called() || run.outcome().reviewer_report().is_some())
            }) {
                reused.insert(policy_outcome.path().clone(), previous.clone());
            } else {
                pending.push(policy_outcome.clone());
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
                result.entries.push(AssetReviewWorkflowEntry {
                    content_path: previous.content_path().clone(),
                    review_run_id: previous.id(),
                    outcome: previous.outcome().clone(),
                });
                continue;
            }
            let outcome = reviewed
                .remove(&path)
                .expect("reviewed path came from the stable path set");
            let path = outcome.path().clone();
            let id = id_generator
                .next_id()
                .map_err(|source| AssetReviewWorkflowError {
                    partial_result: result.clone(),
                    failure: Box::new(AssetReviewWorkflowFailure::ReviewRunId {
                        path: path.clone(),
                        source,
                    }),
                })?;
            let run = AssetReviewRun::from_review_outcome(
                id,
                input.snapshot,
                outcome.clone(),
                input.policy.clone(),
                clock(),
            )
            .map_err(|source| AssetReviewWorkflowError {
                partial_result: result.clone(),
                failure: Box::new(AssetReviewWorkflowFailure::ReviewRun {
                    path: path.clone(),
                    source,
                }),
            })?;
            review_run_store
                .save(&run)
                .map_err(|source| AssetReviewWorkflowError {
                    partial_result: result.clone(),
                    failure: Box::new(AssetReviewWorkflowFailure::Persistence {
                        path: path.clone(),
                        review_run_id: id,
                        source,
                    }),
                })?;
            result.entries.push(AssetReviewWorkflowEntry {
                content_path: path,
                review_run_id: id,
                outcome,
            });
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

    use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};

    use crate::{
        domain::{Sha256, SnapshotFile, SourceId},
        storage::SqliteAssetReviewRunStore,
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-asset-review-workflow-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn store(&self) -> LocalContentStore {
            LocalContentStore::new(self.0.join("content-store"))
        }

        fn database(&self) -> PathBuf {
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
        calls: RefCell<Vec<ContentPath>>,
        responses:
            BTreeMap<ContentPath, Result<AssetReviewDecision, super::super::AssetReviewerError>>,
    }

    impl RecordingReviewer {
        fn with_responses(
            responses: impl IntoIterator<
                Item = (
                    ContentPath,
                    Result<AssetReviewDecision, super::super::AssetReviewerError>,
                ),
            >,
        ) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: responses.into_iter().collect(),
            }
        }

        fn calls(&self) -> Vec<ContentPath> {
            self.calls.borrow().clone()
        }
    }

    impl AssetReviewer for RecordingReviewer {
        fn review(
            &self,
            candidate: &super::super::AssetReviewCandidate,
        ) -> Result<super::super::AssetReviewerReport, super::super::AssetReviewerError> {
            self.calls.borrow_mut().push(candidate.path().clone());
            self.responses
                .get(candidate.path())
                .cloned()
                .expect("reviewer response is configured")
                .map(|decision| {
                    let reasons = match decision {
                        AssetReviewDecision::Approve => vec![
                            super::super::AssetReviewReasonCode::OrdinaryVisualContent,
                        ],
                        AssetReviewDecision::Reject => vec![
                            super::super::AssetReviewReasonCode::OtherVisualPrivacyRisk,
                        ],
                        AssetReviewDecision::NeedsHumanReview => vec![
                            super::super::AssetReviewReasonCode::UncertainVisualDisclosureAuthorization,
                        ],
                    };
                    super::super::AssetReviewerReport::new(
                        decision,
                        reasons,
                        "test visual classification",
                    )
                    .unwrap()
                })
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestStoreError;

    impl fmt::Display for TestStoreError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected persistence failure")
        }
    }

    impl Error for TestStoreError {}

    #[derive(Default)]
    struct RecordingStore {
        calls: Cell<usize>,
        fail_on: Option<usize>,
        saved: RefCell<Vec<AssetReviewRun>>,
    }

    impl RecordingStore {
        fn failing_on(call: usize) -> Self {
            Self {
                calls: Cell::new(0),
                fail_on: Some(call),
                saved: RefCell::new(Vec::new()),
            }
        }

        fn saved(&self) -> Vec<AssetReviewRun> {
            self.saved.borrow().clone()
        }
    }

    impl AssetReviewRunStore for RecordingStore {
        type Error = TestStoreError;

        fn save(&self, run: &AssetReviewRun) -> Result<(), Self::Error> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if self.fail_on == Some(call) {
                return Err(TestStoreError);
            }
            self.saved.borrow_mut().push(run.clone());
            Ok(())
        }

        fn get(&self, id: AssetReviewRunId) -> Result<Option<AssetReviewRun>, Self::Error> {
            Ok(self
                .saved
                .borrow()
                .iter()
                .find(|run| run.id() == id)
                .cloned())
        }

        fn list_by_snapshot(
            &self,
            snapshot_id: SnapshotId,
        ) -> Result<Vec<AssetReviewRun>, Self::Error> {
            Ok(self
                .saved
                .borrow()
                .iter()
                .filter(|run| run.snapshot_id() == snapshot_id)
                .cloned()
                .collect())
        }

        fn list_pending_human_review(&self) -> Result<Vec<AssetReviewRun>, Self::Error> {
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
        PolicyIdentity::new("asset", "asset-v1", Sha256::new([3; 32])).unwrap()
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&[1, 2, 3, 255], 1, 1, ColorType::Rgba8.into())
            .unwrap();
        bytes
    }

    fn snapshot<'a>(
        store: &LocalContentStore,
        entries: impl IntoIterator<Item = (&'a str, Vec<u8>)>,
    ) -> Snapshot {
        let files = entries
            .into_iter()
            .map(|(content_path, bytes)| {
                let sha256 = store.store(&bytes).unwrap();
                SnapshotFile::new(path(content_path), bytes.len() as u64, sha256, None)
            })
            .collect();
        Snapshot::new(
            SnapshotId::new(7).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files,
        )
        .unwrap()
    }

    fn candidates<'a>(
        id: SnapshotId,
        entries: impl IntoIterator<Item = (&'a str, Vec<&'a str>)>,
    ) -> CandidateAssetSet {
        CandidateAssetSet::from_entries_for_test(
            id,
            entries.into_iter().map(|(asset, dependents)| {
                (path(asset), dependents.into_iter().map(path).collect())
            }),
        )
    }

    fn execute(
        candidates: &CandidateAssetSet,
        snapshot: &Snapshot,
        store: &LocalContentStore,
        reviewer: &RecordingReviewer,
        run_store: &impl AssetReviewRunStore<Error = TestStoreError>,
        first_id: u64,
    ) -> Result<
        AssetReviewWorkflowResult,
        AssetReviewWorkflowError<TestStoreError, SequentialAssetReviewRunIdGeneratorError>,
    > {
        let mut ids =
            SequentialAssetReviewRunIdGenerator::new(AssetReviewRunId::new(first_id).unwrap());
        AssetReviewWorkflow::execute_with_clock(
            AssetReviewWorkflowInput::new(candidates, snapshot, store, &policy()),
            reviewer,
            run_store,
            &mut ids,
            || SystemTime::UNIX_EPOCH + Duration::from_secs(20),
        )
    }

    #[test]
    fn clean_image_is_reviewed_saved_and_reported() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(&store, [("clean.png", png())]);
        let candidates = candidates(snapshot.id(), [("clean.png", vec!["article.md"])]);
        let reviewer = RecordingReviewer::with_responses([(
            path("clean.png"),
            Ok(AssetReviewDecision::Approve),
        )]);
        let run_store = RecordingStore::default();

        let result = execute(&candidates, &snapshot, &store, &reviewer, &run_store, 10).unwrap();

        assert_eq!(reviewer.calls(), [path("clean.png")]);
        assert_eq!(result.entries().len(), 1);
        assert_eq!(result.entries()[0].review_run_id().get(), 10);
        assert_eq!(result.approved_asset_paths(), [&path("clean.png")]);
        assert_eq!(
            run_store.saved()[0].outcome(),
            result.entries()[0].outcome()
        );
    }

    #[test]
    fn policy_blocked_and_needs_human_review_are_saved_without_reviewer_calls() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(
            &store,
            [
                ("blocked.png", b"\x89PNG\r\n\x1a\n".to_vec()),
                ("unknown.bin", b"unknown".to_vec()),
            ],
        );
        let candidates = candidates(
            snapshot.id(),
            [("unknown.bin", vec!["b.md"]), ("blocked.png", vec!["a.md"])],
        );
        let reviewer = RecordingReviewer::with_responses([]);
        let run_store = RecordingStore::default();

        let result = execute(&candidates, &snapshot, &store, &reviewer, &run_store, 20).unwrap();

        assert!(reviewer.calls().is_empty());
        assert!(matches!(
            result.entries()[0].outcome().disposition(),
            AssetReviewDisposition::Blocked
        ));
        assert!(matches!(
            result.entries()[1].outcome().disposition(),
            AssetReviewDisposition::NeedsHumanReview(_)
        ));
        assert_eq!(run_store.saved().len(), 2);
    }

    #[test]
    fn reviewer_reject_needs_human_review_and_error_are_durable_outcomes() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(
            &store,
            [("a.png", png()), ("b.png", png()), ("c.png", png())],
        );
        let candidates = candidates(
            snapshot.id(),
            [
                ("a.png", vec!["a.md"]),
                ("b.png", vec!["b.md"]),
                ("c.png", vec!["c.md"]),
            ],
        );
        let reviewer = RecordingReviewer::with_responses([
            (path("a.png"), Ok(AssetReviewDecision::Reject)),
            (path("b.png"), Ok(AssetReviewDecision::NeedsHumanReview)),
            (
                path("c.png"),
                Err(super::super::AssetReviewerError::new("offline")),
            ),
        ]);
        let run_store = RecordingStore::default();

        let result = execute(&candidates, &snapshot, &store, &reviewer, &run_store, 30).unwrap();

        assert_eq!(reviewer.calls().len(), 3);
        assert!(matches!(
            result.entries()[0].outcome().disposition(),
            AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject)
        ));
        assert!(matches!(
            result.entries()[1].outcome().disposition(),
            AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview)
        ));
        assert!(matches!(
            result.entries()[2].outcome().disposition(),
            AssetReviewDisposition::NeedsHumanReview(
                super::super::AssetHumanReviewReason::ReviewerFailed(_)
            )
        ));
        assert_eq!(run_store.list_pending_human_review().unwrap().len(), 2);
    }

    #[test]
    fn shared_asset_is_reviewed_once_and_keeps_all_dependents() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(&store, [("shared.png", png()), ("orphan.png", png())]);
        let candidates = candidates(snapshot.id(), [("shared.png", vec!["b.md", "a.md"])]);
        let reviewer = RecordingReviewer::with_responses([(
            path("shared.png"),
            Ok(AssetReviewDecision::Approve),
        )]);
        let run_store = RecordingStore::default();

        let result = execute(&candidates, &snapshot, &store, &reviewer, &run_store, 40).unwrap();

        assert_eq!(reviewer.calls(), [path("shared.png")]);
        assert_eq!(
            result.entries()[0].outcome().dependents(),
            [path("a.md"), path("b.md")]
        );
        assert_eq!(run_store.saved().len(), 1);
    }

    #[test]
    fn persistence_failure_returns_durable_prefix_and_stops_processing() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(
            &store,
            [("a.png", png()), ("b.png", png()), ("c.png", png())],
        );
        let candidates = candidates(
            snapshot.id(),
            [
                ("c.png", vec!["c.md"]),
                ("a.png", vec!["a.md"]),
                ("b.png", vec!["b.md"]),
            ],
        );
        let reviewer = RecordingReviewer::with_responses([
            (path("a.png"), Ok(AssetReviewDecision::Approve)),
            (path("b.png"), Ok(AssetReviewDecision::Approve)),
            (path("c.png"), Ok(AssetReviewDecision::Approve)),
        ]);
        let run_store = RecordingStore::failing_on(2);

        let error = execute(&candidates, &snapshot, &store, &reviewer, &run_store, 50).unwrap_err();

        assert_eq!(error.partial_result().entries().len(), 1);
        assert_eq!(
            error.partial_result().entries()[0].content_path(),
            &path("a.png")
        );
        assert_eq!(run_store.saved().len(), 1);
        assert!(
            matches!(error.failure(), AssetReviewWorkflowFailure::Persistence { path: failed_path, review_run_id, .. } if failed_path == &path("b.png") && review_run_id.get() == 51)
        );
        assert_eq!(reviewer.calls(), [path("a.png"), path("b.png")]);
    }

    #[test]
    fn snapshot_mismatch_fails_before_program_check_or_reviewer() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(&store, [("clean.png", png())]);
        let candidates = candidates(SnapshotId::new(8).unwrap(), [("clean.png", vec!["a.md"])]);
        let reviewer = RecordingReviewer::with_responses([]);
        let run_store = RecordingStore::default();

        let error = execute(&candidates, &snapshot, &store, &reviewer, &run_store, 60).unwrap_err();

        assert!(matches!(
            error.failure(),
            AssetReviewWorkflowFailure::SnapshotMismatch { .. }
        ));
        assert!(reviewer.calls().is_empty());
        assert!(run_store.saved().is_empty());
    }

    #[test]
    fn program_check_execution_failure_stops_before_reviewer_or_persistence() {
        let directory = TestDirectory::new();
        let blocked_store_root = directory.0.join("content-store");
        fs::create_dir_all(&blocked_store_root).unwrap();
        let bytes = png();
        let sha256 = Sha256::digest(&bytes);
        fs::create_dir(blocked_store_root.join(sha256.to_string())).unwrap();
        let content_store = LocalContentStore::new(blocked_store_root);
        let snapshot = Snapshot::new(
            SnapshotId::new(7).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![SnapshotFile::new(
                path("clean.png"),
                bytes.len() as u64,
                sha256,
                None,
            )],
        )
        .unwrap();
        let candidates = candidates(snapshot.id(), [("clean.png", vec!["a.md"])]);
        let reviewer = RecordingReviewer::with_responses([]);
        let run_store = RecordingStore::default();

        let error = execute(
            &candidates,
            &snapshot,
            &content_store,
            &reviewer,
            &run_store,
            65,
        )
        .unwrap_err();

        assert!(matches!(
            error.failure(),
            AssetReviewWorkflowFailure::ProgramCheck(AssetProgramCheckError::ContentStore { .. })
        ));
        assert!(reviewer.calls().is_empty());
        assert!(run_store.saved().is_empty());
    }

    #[test]
    fn sqlite_store_recovers_workflow_audit_facts() {
        let directory = TestDirectory::new();
        let store = directory.store();
        let snapshot = snapshot(&store, [("clean.png", png())]);
        let candidates = candidates(snapshot.id(), [("clean.png", vec!["a.md"])]);
        let reviewer = RecordingReviewer::with_responses([(
            path("clean.png"),
            Ok(AssetReviewDecision::Approve),
        )]);
        let sqlite = SqliteAssetReviewRunStore::open(directory.database()).unwrap();
        let mut ids = SequentialAssetReviewRunIdGenerator::new(AssetReviewRunId::new(70).unwrap());

        let result = AssetReviewWorkflow::execute(
            AssetReviewWorkflowInput::new(&candidates, &snapshot, &store, &policy()),
            &reviewer,
            &sqlite,
            &mut ids,
        )
        .unwrap();
        drop(sqlite);
        let reopened = SqliteAssetReviewRunStore::open(directory.database()).unwrap();

        let stored = reopened.list_by_snapshot(snapshot.id()).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id(), result.entries()[0].review_run_id());
        assert_eq!(stored[0].outcome(), result.entries()[0].outcome());
    }
}
