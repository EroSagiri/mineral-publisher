//! Adapter conformance tests for the portable human-review resolution engine.
//!
//! These tests exercise the engine against the real native SQLite adapters and
//! temporary directories, so they live in the host crate.

use std::time::SystemTime;

use crate::policy::{ReviewRun, ReviewRunId, ReviewRunStore};
use crate::workflow::*;
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
