use std::{error::Error, fmt, time::SystemTime};

use crate::policy::{PublicPolicyDecision, ReviewRun, ReviewRunId, ReviewRunStore};

use super::{
    AssetReviewDecision, AssetReviewDisposition, AssetReviewRun, AssetReviewRunId,
    AssetReviewRunStore,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HumanReviewId(u64);

impl HumanReviewId {
    pub fn new(value: u64) -> Result<Self, HumanReviewRecordError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(HumanReviewRecordError::InvalidId)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HumanReviewSubject {
    Document(ReviewRunId),
    Asset(AssetReviewRunId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumanReviewDecision {
    Approve,
    Reject,
}

/// Immutable audit fact recording a final human decision about one automatic review attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanReviewRecord {
    id: HumanReviewId,
    subject: HumanReviewSubject,
    decision: HumanReviewDecision,
    created_at_unix_ms: u64,
    reviewer: Option<String>,
    note: Option<String>,
}

impl HumanReviewRecord {
    fn new(
        id: HumanReviewId,
        subject: HumanReviewSubject,
        decision: HumanReviewDecision,
        created_at: SystemTime,
        reviewer: Option<String>,
        note: Option<String>,
    ) -> Result<Self, HumanReviewRecordError> {
        if reviewer
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(HumanReviewRecordError::EmptyReviewer);
        }
        let created_at_unix_ms = created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| HumanReviewRecordError::CreatedBeforeUnixEpoch)?
            .as_millis()
            .try_into()
            .map_err(|_| HumanReviewRecordError::TimestampOutOfRange)?;
        Ok(Self {
            id,
            subject,
            decision,
            created_at_unix_ms,
            reviewer,
            note,
        })
    }

    pub fn id(&self) -> HumanReviewId {
        self.id
    }

    pub fn subject(&self) -> HumanReviewSubject {
        self.subject
    }

    pub fn decision(&self) -> HumanReviewDecision {
        self.decision
    }

    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    pub fn reviewer(&self) -> Option<&str> {
        self.reviewer.as_deref()
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    pub(crate) fn rehydrate(
        id: HumanReviewId,
        subject: HumanReviewSubject,
        decision: HumanReviewDecision,
        created_at_unix_ms: u64,
        reviewer: Option<String>,
        note: Option<String>,
    ) -> Result<Self, HumanReviewRecordError> {
        if reviewer
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(HumanReviewRecordError::EmptyReviewer);
        }
        Ok(Self {
            id,
            subject,
            decision,
            created_at_unix_ms,
            reviewer,
            note,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanReviewRecordError {
    InvalidId,
    EmptyReviewer,
    CreatedBeforeUnixEpoch,
    TimestampOutOfRange,
}

impl fmt::Display for HumanReviewRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => formatter.write_str("human review id must be non-zero"),
            Self::EmptyReviewer => formatter.write_str("human reviewer cannot be empty"),
            Self::CreatedBeforeUnixEpoch => {
                formatter.write_str("human review timestamp is before the Unix epoch")
            }
            Self::TimestampOutOfRange => {
                formatter.write_str("human review timestamp is outside the supported range")
            }
        }
    }
}

impl Error for HumanReviewRecordError {}

pub trait HumanReviewStore {
    type Error: Error + Send + Sync + 'static;

    fn save(&self, record: &HumanReviewRecord) -> Result<(), Self::Error>;
    fn get(&self, id: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error>;
    fn get_for_subject(
        &self,
        subject: HumanReviewSubject,
    ) -> Result<Option<HumanReviewRecord>, Self::Error>;
    fn list(&self) -> Result<Vec<HumanReviewRecord>, Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveReviewDecision {
    Approved,
    Rejected,
    PendingHumanReview,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveReviewDecisionError {
    expected: HumanReviewSubject,
    actual: HumanReviewSubject,
}

impl fmt::Display for EffectiveReviewDecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "human resolution subject {:?} does not match automatic review subject {:?}",
            self.actual, self.expected
        )
    }
}

impl Error for EffectiveReviewDecisionError {}

pub struct HumanReviewResolution;

impl HumanReviewResolution {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_document<R, H>(
        automatic_store: &R,
        human_store: &H,
        id: HumanReviewId,
        review_run_id: ReviewRunId,
        decision: HumanReviewDecision,
        created_at: SystemTime,
        reviewer: Option<String>,
        note: Option<String>,
    ) -> Result<HumanReviewRecord, HumanReviewResolutionError<R::Error, H::Error>>
    where
        R: ReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
    {
        let run = automatic_store
            .get(review_run_id)
            .map_err(HumanReviewResolutionError::AutomaticStore)?
            .ok_or(HumanReviewResolutionError::AutomaticReviewNotFound(
                HumanReviewSubject::Document(review_run_id),
            ))?;
        Self::resolve(
            human_store,
            id,
            HumanReviewSubject::Document(review_run_id),
            run.needs_human_review(),
            decision,
            created_at,
            reviewer,
            note,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve_asset<R, H>(
        automatic_store: &R,
        human_store: &H,
        id: HumanReviewId,
        review_run_id: AssetReviewRunId,
        decision: HumanReviewDecision,
        created_at: SystemTime,
        reviewer: Option<String>,
        note: Option<String>,
    ) -> Result<HumanReviewRecord, HumanReviewResolutionError<R::Error, H::Error>>
    where
        R: AssetReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
    {
        let run = automatic_store
            .get(review_run_id)
            .map_err(HumanReviewResolutionError::AutomaticStore)?
            .ok_or(HumanReviewResolutionError::AutomaticReviewNotFound(
                HumanReviewSubject::Asset(review_run_id),
            ))?;
        Self::resolve(
            human_store,
            id,
            HumanReviewSubject::Asset(review_run_id),
            run.needs_human_review(),
            decision,
            created_at,
            reviewer,
            note,
        )
    }

    pub fn list_pending_documents<R, H>(
        automatic_store: &R,
        human_store: &H,
    ) -> Result<Vec<ReviewRun>, HumanReviewResolutionError<R::Error, H::Error>>
    where
        R: ReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
    {
        let runs = automatic_store
            .list_pending_human_review()
            .map_err(HumanReviewResolutionError::AutomaticStore)?;
        Self::without_resolutions(runs, human_store, |run| {
            HumanReviewSubject::Document(run.id())
        })
    }

    pub fn list_pending_assets<R, H>(
        automatic_store: &R,
        human_store: &H,
    ) -> Result<Vec<AssetReviewRun>, HumanReviewResolutionError<R::Error, H::Error>>
    where
        R: AssetReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
    {
        let runs = automatic_store
            .list_pending_human_review()
            .map_err(HumanReviewResolutionError::AutomaticStore)?;
        Self::without_resolutions(runs, human_store, |run| HumanReviewSubject::Asset(run.id()))
    }

    pub fn effective_document(
        run: &ReviewRun,
        resolution: Option<&HumanReviewRecord>,
    ) -> Result<EffectiveReviewDecision, EffectiveReviewDecisionError> {
        match run.decision() {
            PublicPolicyDecision::ReviewApproved => Ok(EffectiveReviewDecision::Approved),
            PublicPolicyDecision::ReviewRejected | PublicPolicyDecision::ProgramIssues(_) => {
                Ok(EffectiveReviewDecision::Rejected)
            }
            PublicPolicyDecision::NeedsHumanReview(_) => {
                Self::effective_human(HumanReviewSubject::Document(run.id()), resolution)
            }
        }
    }

    pub fn effective_asset(
        run: &AssetReviewRun,
        resolution: Option<&HumanReviewRecord>,
    ) -> Result<EffectiveReviewDecision, EffectiveReviewDecisionError> {
        match run.outcome().disposition() {
            AssetReviewDisposition::Blocked
            | AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject) => {
                Ok(EffectiveReviewDecision::Rejected)
            }
            AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve) => {
                Ok(EffectiveReviewDecision::Approved)
            }
            AssetReviewDisposition::NeedsHumanReview(_)
            | AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview) => {
                Self::effective_human(HumanReviewSubject::Asset(run.id()), resolution)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve<A, H>(
        human_store: &H,
        id: HumanReviewId,
        subject: HumanReviewSubject,
        needs_human_review: bool,
        decision: HumanReviewDecision,
        created_at: SystemTime,
        reviewer: Option<String>,
        note: Option<String>,
    ) -> Result<HumanReviewRecord, HumanReviewResolutionError<A, H::Error>>
    where
        A: Error + Send + Sync + 'static,
        H: HumanReviewStore + ?Sized,
    {
        if !needs_human_review {
            return Err(HumanReviewResolutionError::NotPendingHumanReview(subject));
        }
        let record = HumanReviewRecord::new(id, subject, decision, created_at, reviewer, note)
            .map_err(HumanReviewResolutionError::InvalidRecord)?;
        human_store
            .save(&record)
            .map_err(HumanReviewResolutionError::HumanStore)?;
        Ok(record)
    }

    fn without_resolutions<T, A, H>(
        runs: Vec<T>,
        human_store: &H,
        subject: impl Fn(&T) -> HumanReviewSubject,
    ) -> Result<Vec<T>, HumanReviewResolutionError<A, H::Error>>
    where
        A: Error + Send + Sync + 'static,
        H: HumanReviewStore + ?Sized,
    {
        let mut pending = Vec::new();
        for run in runs {
            if human_store
                .get_for_subject(subject(&run))
                .map_err(HumanReviewResolutionError::HumanStore)?
                .is_none()
            {
                pending.push(run);
            }
        }
        Ok(pending)
    }

    fn effective_human(
        expected: HumanReviewSubject,
        resolution: Option<&HumanReviewRecord>,
    ) -> Result<EffectiveReviewDecision, EffectiveReviewDecisionError> {
        let Some(resolution) = resolution else {
            return Ok(EffectiveReviewDecision::PendingHumanReview);
        };
        if resolution.subject() != expected {
            return Err(EffectiveReviewDecisionError {
                expected,
                actual: resolution.subject(),
            });
        }
        Ok(match resolution.decision() {
            HumanReviewDecision::Approve => EffectiveReviewDecision::Approved,
            HumanReviewDecision::Reject => EffectiveReviewDecision::Rejected,
        })
    }
}

#[derive(Debug)]
pub enum HumanReviewResolutionError<A, H> {
    AutomaticStore(A),
    HumanStore(H),
    AutomaticReviewNotFound(HumanReviewSubject),
    NotPendingHumanReview(HumanReviewSubject),
    InvalidRecord(HumanReviewRecordError),
}

impl<A: fmt::Display, H: fmt::Display> fmt::Display for HumanReviewResolutionError<A, H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AutomaticStore(error) => {
                write!(formatter, "automatic review lookup failed: {error}")
            }
            Self::HumanStore(error) => {
                write!(formatter, "human review persistence failed: {error}")
            }
            Self::AutomaticReviewNotFound(subject) => {
                write!(formatter, "automatic review does not exist: {subject:?}")
            }
            Self::NotPendingHumanReview(subject) => {
                write!(
                    formatter,
                    "automatic review is not awaiting human review: {subject:?}"
                )
            }
            Self::InvalidRecord(error) => write!(formatter, "invalid human review record: {error}"),
        }
    }
}

impl<A, H> Error for HumanReviewResolutionError<A, H>
where
    A: Error + 'static,
    H: Error + 'static,
{
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use crate::{
        domain::{ContentPath, Sha256, SnapshotId},
        policy::{
            HumanReviewReason, PolicyIdentity, ProgramCheckIssue, PublicPolicyDecision,
            ReviewerError,
        },
        storage::{
            SqliteAssetReviewRunStore, SqliteHumanReviewStore, SqliteHumanReviewStoreError,
            SqliteReviewRunStore,
        },
        workflow::{AssetHumanReviewReason, AssetReviewOutcome, AssetReviewerError},
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-human-review-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn document_run(id: u64, decision: PublicPolicyDecision) -> ReviewRun {
        ReviewRun::rehydrate(
            ReviewRunId::new(id).unwrap(),
            SnapshotId::new(1).unwrap(),
            path(&format!("document-{id}.md")),
            Sha256::digest(format!("document-{id}").as_bytes()),
            PolicyIdentity::new("public", "v1", Sha256::new([1; 32])).unwrap(),
            (decision, None),
            1_000 + id,
        )
    }

    fn asset_run(id: u64, disposition: AssetReviewDisposition) -> AssetReviewRun {
        let asset_path = path(&format!("asset-{id}.png"));
        let sha256 = Sha256::digest(format!("asset-{id}").as_bytes());
        AssetReviewRun::rehydrate(
            AssetReviewRunId::new(id).unwrap(),
            SnapshotId::new(1).unwrap(),
            asset_path.clone(),
            sha256,
            PolicyIdentity::new("asset", "v1", Sha256::new([2; 32])).unwrap(),
            AssetReviewOutcome::from_parts_for_test(
                asset_path,
                vec![path("document.md")],
                Some(sha256),
                vec![],
                disposition,
            ),
            2_000 + id,
        )
    }

    fn at(milliseconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(milliseconds)
    }

    #[test]
    fn document_approve_and_reject_produce_effective_decisions() {
        let directory = TestDirectory::new();
        let automatic =
            SqliteReviewRunStore::open(directory.database("documents.sqlite3")).unwrap();
        let human = SqliteHumanReviewStore::open(directory.database("human.sqlite3")).unwrap();
        let approve_run = document_run(
            1,
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        let reject_run = document_run(
            2,
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        automatic.save(&approve_run).unwrap();
        automatic.save(&reject_run).unwrap();

        let approved = HumanReviewResolution::resolve_document(
            &automatic,
            &human,
            HumanReviewId::new(1).unwrap(),
            approve_run.id(),
            HumanReviewDecision::Approve,
            at(3_001),
            Some("reviewer-a".to_owned()),
            Some("safe to publish".to_owned()),
        )
        .unwrap();
        let rejected = HumanReviewResolution::resolve_document(
            &automatic,
            &human,
            HumanReviewId::new(2).unwrap(),
            reject_run.id(),
            HumanReviewDecision::Reject,
            at(3_002),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            HumanReviewResolution::effective_document(&approve_run, Some(&approved)).unwrap(),
            EffectiveReviewDecision::Approved
        );
        assert_eq!(
            HumanReviewResolution::effective_document(&reject_run, Some(&rejected)).unwrap(),
            EffectiveReviewDecision::Rejected
        );
    }

    #[test]
    fn reviewer_failures_remain_automatic_facts_after_human_resolution() {
        let directory = TestDirectory::new();
        let documents =
            SqliteReviewRunStore::open(directory.database("documents.sqlite3")).unwrap();
        let assets = SqliteAssetReviewRunStore::open(directory.database("assets.sqlite3")).unwrap();
        let human_path = directory.database("human.sqlite3");
        let human = SqliteHumanReviewStore::open(&human_path).unwrap();
        let document = document_run(
            10,
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(
                ReviewerError::new("document provider unavailable"),
            )),
        );
        let asset = asset_run(
            10,
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(
                AssetReviewerError::new("asset provider unavailable"),
            )),
        );
        documents.save(&document).unwrap();
        assets.save(&asset).unwrap();
        let document_resolution = HumanReviewResolution::resolve_document(
            &documents,
            &human,
            HumanReviewId::new(10).unwrap(),
            document.id(),
            HumanReviewDecision::Approve,
            at(4_000),
            None,
            None,
        )
        .unwrap();
        let asset_resolution = HumanReviewResolution::resolve_asset(
            &assets,
            &human,
            HumanReviewId::new(11).unwrap(),
            asset.id(),
            HumanReviewDecision::Reject,
            at(4_001),
            None,
            None,
        )
        .unwrap();
        drop(human);

        let reopened = SqliteHumanReviewStore::open(&human_path).unwrap();
        assert!(matches!(
            documents.get(document.id()).unwrap().unwrap().decision(),
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(error))
                if error.message() == "document provider unavailable"
        ));
        assert!(matches!(
            assets.get(asset.id()).unwrap().unwrap().outcome().disposition(),
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(error))
                if error.message() == "asset provider unavailable"
        ));
        assert_eq!(
            reopened.get(document_resolution.id()).unwrap(),
            Some(document_resolution)
        );
        assert_eq!(
            reopened.get(asset_resolution.id()).unwrap(),
            Some(asset_resolution)
        );
    }

    #[test]
    fn policy_and_reviewer_asset_human_review_can_be_approved_or_rejected() {
        let directory = TestDirectory::new();
        let automatic =
            SqliteAssetReviewRunStore::open(directory.database("assets.sqlite3")).unwrap();
        let human = SqliteHumanReviewStore::open(directory.database("human.sqlite3")).unwrap();
        let runs = [
            asset_run(
                1,
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            ),
            asset_run(
                2,
                AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview),
            ),
        ];
        for run in &runs {
            automatic.save(run).unwrap();
        }
        let approved = HumanReviewResolution::resolve_asset(
            &automatic,
            &human,
            HumanReviewId::new(20).unwrap(),
            runs[0].id(),
            HumanReviewDecision::Approve,
            at(5_000),
            None,
            None,
        )
        .unwrap();
        let rejected = HumanReviewResolution::resolve_asset(
            &automatic,
            &human,
            HumanReviewId::new(21).unwrap(),
            runs[1].id(),
            HumanReviewDecision::Reject,
            at(5_001),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            HumanReviewResolution::effective_asset(&runs[0], Some(&approved)).unwrap(),
            EffectiveReviewDecision::Approved
        );
        assert_eq!(
            HumanReviewResolution::effective_asset(&runs[1], Some(&rejected)).unwrap(),
            EffectiveReviewDecision::Rejected
        );
    }

    #[test]
    fn non_pending_automatic_outcomes_cannot_be_overridden() {
        let directory = TestDirectory::new();
        let documents =
            SqliteReviewRunStore::open(directory.database("documents.sqlite3")).unwrap();
        let assets = SqliteAssetReviewRunStore::open(directory.database("assets.sqlite3")).unwrap();
        let human = SqliteHumanReviewStore::open(directory.database("human.sqlite3")).unwrap();
        let document_runs = [
            document_run(1, PublicPolicyDecision::ReviewApproved),
            document_run(2, PublicPolicyDecision::ReviewRejected),
            document_run(
                3,
                PublicPolicyDecision::ProgramIssues(Vec::<ProgramCheckIssue>::new()),
            ),
        ];
        for run in &document_runs {
            documents.save(run).unwrap();
            let error = HumanReviewResolution::resolve_document(
                &documents,
                &human,
                HumanReviewId::new(run.id().get()).unwrap(),
                run.id(),
                HumanReviewDecision::Approve,
                at(6_000),
                None,
                None,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                HumanReviewResolutionError::NotPendingHumanReview(_)
            ));
        }
        let blocked = asset_run(4, AssetReviewDisposition::Blocked);
        assets.save(&blocked).unwrap();
        let error = HumanReviewResolution::resolve_asset(
            &assets,
            &human,
            HumanReviewId::new(4).unwrap(),
            blocked.id(),
            HumanReviewDecision::Approve,
            at(6_001),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            HumanReviewResolutionError::NotPendingHumanReview(_)
        ));
        assert!(human.list().unwrap().is_empty());
    }

    #[test]
    fn saves_are_idempotent_but_ids_and_subjects_cannot_be_reused() {
        let directory = TestDirectory::new();
        let automatic =
            SqliteReviewRunStore::open(directory.database("documents.sqlite3")).unwrap();
        let human = SqliteHumanReviewStore::open(directory.database("human.sqlite3")).unwrap();
        let run = document_run(
            1,
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        automatic.save(&run).unwrap();
        let arguments = || {
            HumanReviewResolution::resolve_document(
                &automatic,
                &human,
                HumanReviewId::new(1).unwrap(),
                run.id(),
                HumanReviewDecision::Approve,
                at(7_000),
                None,
                None,
            )
        };
        let expected = arguments().unwrap();
        assert_eq!(arguments().unwrap(), expected);

        let id_conflict = HumanReviewResolution::resolve_document(
            &automatic,
            &human,
            HumanReviewId::new(1).unwrap(),
            run.id(),
            HumanReviewDecision::Reject,
            at(7_000),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            id_conflict,
            HumanReviewResolutionError::HumanStore(
                SqliteHumanReviewStoreError::ConflictingHumanReviewId(_)
            )
        ));

        let subject_conflict = HumanReviewResolution::resolve_document(
            &automatic,
            &human,
            HumanReviewId::new(2).unwrap(),
            run.id(),
            HumanReviewDecision::Approve,
            at(7_001),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            subject_conflict,
            HumanReviewResolutionError::HumanStore(
                SqliteHumanReviewStoreError::SubjectAlreadyResolved(_)
            )
        ));
    }

    #[test]
    fn pending_queries_exclude_resolved_subjects_and_keep_stable_order() {
        let directory = TestDirectory::new();
        let documents =
            SqliteReviewRunStore::open(directory.database("documents.sqlite3")).unwrap();
        let assets = SqliteAssetReviewRunStore::open(directory.database("assets.sqlite3")).unwrap();
        let human = SqliteHumanReviewStore::open(directory.database("human.sqlite3")).unwrap();
        let document_runs = [
            document_run(
                3,
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document_run(
                1,
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document_run(
                2,
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
        ];
        for run in &document_runs {
            documents.save(run).unwrap();
        }
        let asset_runs = [
            asset_run(
                2,
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            ),
            asset_run(
                1,
                AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview),
            ),
        ];
        for run in &asset_runs {
            assets.save(run).unwrap();
        }
        HumanReviewResolution::resolve_document(
            &documents,
            &human,
            HumanReviewId::new(30).unwrap(),
            document_runs[2].id(),
            HumanReviewDecision::Approve,
            at(8_000),
            None,
            None,
        )
        .unwrap();
        HumanReviewResolution::resolve_asset(
            &assets,
            &human,
            HumanReviewId::new(31).unwrap(),
            asset_runs[0].id(),
            HumanReviewDecision::Reject,
            at(8_001),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            HumanReviewResolution::list_pending_documents(&documents, &human)
                .unwrap()
                .iter()
                .map(|run| run.content_path().as_str())
                .collect::<Vec<_>>(),
            ["document-1.md", "document-3.md"]
        );
        assert_eq!(
            HumanReviewResolution::list_pending_assets(&assets, &human)
                .unwrap()
                .iter()
                .map(|run| run.content_path().as_str())
                .collect::<Vec<_>>(),
            ["asset-1.png"]
        );
        assert_eq!(
            human
                .list()
                .unwrap()
                .iter()
                .map(|record| record.subject())
                .collect::<Vec<_>>(),
            [
                HumanReviewSubject::Asset(AssetReviewRunId::new(2).unwrap()),
                HumanReviewSubject::Document(ReviewRunId::new(2).unwrap()),
            ]
        );
    }

    #[test]
    fn missing_automatic_run_and_mismatched_effective_subject_fail_closed() {
        let directory = TestDirectory::new();
        let automatic =
            SqliteReviewRunStore::open(directory.database("documents.sqlite3")).unwrap();
        let human = SqliteHumanReviewStore::open(directory.database("human.sqlite3")).unwrap();
        let missing = HumanReviewResolution::resolve_document(
            &automatic,
            &human,
            HumanReviewId::new(1).unwrap(),
            ReviewRunId::new(999).unwrap(),
            HumanReviewDecision::Approve,
            at(9_000),
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(
            missing,
            HumanReviewResolutionError::AutomaticReviewNotFound(HumanReviewSubject::Document(_))
        ));

        let run = document_run(
            1,
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        let wrong = HumanReviewRecord::rehydrate(
            HumanReviewId::new(2).unwrap(),
            HumanReviewSubject::Asset(AssetReviewRunId::new(1).unwrap()),
            HumanReviewDecision::Approve,
            9_001,
            None,
            None,
        )
        .unwrap();
        assert!(HumanReviewResolution::effective_document(&run, Some(&wrong)).is_err());
        assert_eq!(
            HumanReviewResolution::effective_document(&run, None).unwrap(),
            EffectiveReviewDecision::PendingHumanReview
        );
    }

    #[test]
    fn invalid_record_fields_are_rejected() {
        assert_eq!(
            HumanReviewId::new(0).unwrap_err(),
            HumanReviewRecordError::InvalidId
        );
        let result = HumanReviewRecord::new(
            HumanReviewId::new(1).unwrap(),
            HumanReviewSubject::Document(ReviewRunId::new(1).unwrap()),
            HumanReviewDecision::Approve,
            SystemTime::UNIX_EPOCH - Duration::from_millis(1),
            Some("   ".to_owned()),
            None,
        );
        assert!(matches!(result, Err(HumanReviewRecordError::EmptyReviewer)));
    }
}
