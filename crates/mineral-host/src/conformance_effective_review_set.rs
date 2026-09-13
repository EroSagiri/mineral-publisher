//! Adapter conformance tests for the portable effective review set engine.
//!
//! These tests exercise the engine against the real native SQLite adapters, so
//! they live in the host crate.

use crate::domain::ContentPath;
use crate::policy::{ReviewRunId, ReviewRunStore};
use crate::workflow::*;

#[cfg(test)]
mod tests {
    use std::{error::Error, fmt};

    use crate::{
        domain::{Sha256, SnapshotId},
        policy::{
            HumanReviewReason, PolicyIdentity, PrivateDocument, PrivateReason,
            PublicPolicyDecision, ReviewRun,
        },
        storage::{SqliteAssetReviewRunStore, SqliteHumanReviewStore, SqliteReviewRunStore},
    };

    use super::*;
    use crate::workflow::{
        AssetHumanReviewReason, AssetReviewDecision, AssetReviewDisposition, AssetReviewOutcome,
        AssetReviewRun, AssetReviewWorkflowEntry, HumanReviewDecision, HumanReviewId,
        HumanReviewRecord,
    };

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }
    fn snapshot() -> SnapshotId {
        SnapshotId::new(1).unwrap()
    }
    fn policy() -> PolicyIdentity {
        PolicyIdentity::new("test", "v1", Sha256::new([7; 32])).unwrap()
    }
    fn document(id: u64, path_value: &str, decision: PublicPolicyDecision) -> ReviewRun {
        ReviewRun::rehydrate(
            ReviewRunId::new(id).unwrap(),
            snapshot(),
            path(path_value),
            Sha256::new([id as u8; 32]),
            policy(),
            (decision, None),
            id,
        )
    }
    fn asset_outcome(
        path_value: &str,
        dependents: &[&str],
        disposition: AssetReviewDisposition,
    ) -> AssetReviewOutcome {
        AssetReviewOutcome::from_parts_for_test(
            path(path_value),
            dependents.iter().map(|value| path(value)).collect(),
            Some(Sha256::new([3; 32])),
            Vec::new(),
            disposition,
        )
    }
    fn asset(id: u64, outcome: AssetReviewOutcome) -> AssetReviewRun {
        AssetReviewRun::rehydrate(
            AssetReviewRunId::new(id).unwrap(),
            snapshot(),
            outcome.path().clone(),
            Sha256::new([3; 32]),
            policy(),
            outcome,
            id,
        )
    }
    fn document_result(runs: Vec<ReviewRun>) -> PublicPolicyRunResult {
        PublicPolicyRunResult::from_document_outcomes_for_test(snapshot(), runs)
    }
    fn asset_result(runs: &[AssetReviewRun]) -> AssetReviewWorkflowResult {
        AssetReviewWorkflowResult::from_entries_for_test(
            snapshot(),
            runs.iter()
                .map(|run| {
                    AssetReviewWorkflowEntry::from_parts(
                        run.content_path().clone(),
                        run.id(),
                        run.outcome().clone(),
                    )
                })
                .collect(),
        )
    }
    fn resolution(
        id: u64,
        subject: HumanReviewSubject,
        decision: HumanReviewDecision,
    ) -> HumanReviewRecord {
        HumanReviewRecord::rehydrate(
            HumanReviewId::new(id).unwrap(),
            subject,
            decision,
            id,
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn documents_keep_privacy_distinct_and_merge_explicit_human_resolutions() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let runs = vec![
            document(1, "approved.md", PublicPolicyDecision::ReviewApproved),
            document(2, "rejected.md", PublicPolicyDecision::ReviewRejected),
            document(
                3,
                "program.md",
                PublicPolicyDecision::ProgramIssues(Vec::new()),
            ),
            document(
                4,
                "pending.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document(
                5,
                "human-approved.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document(
                6,
                "human-rejected.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            ),
            document(
                7,
                "failed.md",
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(
                    crate::policy::ReviewerError::new("offline"),
                )),
            ),
        ];
        for run in &runs {
            documents.save(run).unwrap();
        }
        human
            .save(&resolution(
                1,
                HumanReviewSubject::Document(runs[4].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&resolution(
                2,
                HumanReviewSubject::Document(runs[5].id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                3,
                HumanReviewSubject::Document(runs[6].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        let result = PublicPolicyRunResult::from_parts_for_test(
            snapshot(),
            vec![PrivateDocument::from_parts_for_test(
                path("private.md"),
                vec![PrivateReason::FrontmatterPrivate],
            )],
            vec![crate::policy::InvalidPrivacyDocument::from_parts_for_test(
                path("invalid.md"),
                "bad yaml",
            )],
            runs,
        );
        let set =
            EffectiveReviewSet::build(&result, &asset_result(&[]), &documents, &assets, &human)
                .unwrap();
        assert_eq!(
            set.approved_markdown_paths(),
            [
                path("approved.md"),
                path("failed.md"),
                path("human-approved.md")
            ]
            .iter()
            .collect::<Vec<_>>()
        );
        assert_eq!(set.pending_documents().len(), 1);
        assert!(set.has_pending_review());
        assert!(
            set.documents()
                .iter()
                .any(|entry| entry.content_path() == &path("private.md")
                    && entry.decision() == EffectiveDocumentDecision::Private
                    && entry.review_run_id().is_none())
        );
        assert!(
            set.documents()
                .iter()
                .any(|entry| entry.content_path() == &path("invalid.md")
                    && entry.decision() == EffectiveDocumentDecision::InvalidPrivacy)
        );
        assert!(
            set.documents()
                .iter()
                .any(|entry| entry.content_path() == &path("program.md")
                    && entry.decision() == EffectiveDocumentDecision::Rejected)
        );
    }

    #[test]
    fn assets_merge_all_automatic_and_human_effective_decisions() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let runs = vec![
            asset(
                1,
                asset_outcome(
                    "approved.png",
                    &["a.md"],
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
                ),
            ),
            asset(
                2,
                asset_outcome(
                    "rejected.png",
                    &["a.md"],
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject),
                ),
            ),
            asset(
                3,
                asset_outcome("blocked.png", &["a.md"], AssetReviewDisposition::Blocked),
            ),
            asset(
                4,
                asset_outcome(
                    "pending.png",
                    &["a.md"],
                    AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview),
                ),
            ),
            asset(
                5,
                asset_outcome(
                    "human-approved.png",
                    &["a.md"],
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::PolicyFindings,
                    ),
                ),
            ),
            asset(
                6,
                asset_outcome(
                    "human-rejected.png",
                    &["a.md"],
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::PolicyFindings,
                    ),
                ),
            ),
            asset(
                7,
                asset_outcome(
                    "failed-approved.png",
                    &["a.md"],
                    AssetReviewDisposition::NeedsHumanReview(
                        AssetHumanReviewReason::ReviewerFailed(
                            crate::workflow::AssetReviewerError::new("offline"),
                        ),
                    ),
                ),
            ),
        ];
        for run in &runs {
            assets.save(run).unwrap();
        }
        human
            .save(&resolution(
                1,
                HumanReviewSubject::Asset(runs[4].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&resolution(
                2,
                HumanReviewSubject::Asset(runs[5].id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                3,
                HumanReviewSubject::Asset(runs[6].id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(Vec::new()),
            &asset_result(&runs),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.approved_asset_paths(),
            [
                path("approved.png"),
                path("failed-approved.png"),
                path("human-approved.png")
            ]
            .iter()
            .collect::<Vec<_>>()
        );
        assert_eq!(set.pending_assets().len(), 1);
        assert!(
            set.assets()
                .iter()
                .any(|entry| entry.content_path() == &path("blocked.png")
                    && entry.decision() == EffectiveReviewDecision::Rejected)
        );
    }

    #[test]
    fn selected_attempts_win_over_newer_attempts_for_documents_and_assets() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let old_document = document(
            1,
            "a.md",
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        let new_document = document(
            2,
            "a.md",
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        let old_asset = asset(
            1,
            asset_outcome(
                "a.png",
                &["a.md"],
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            ),
        );
        let new_asset = asset(
            2,
            asset_outcome(
                "a.png",
                &["a.md"],
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            ),
        );
        for run in [&old_document, &new_document] {
            documents.save(run).unwrap();
        }
        for run in [&old_asset, &new_asset] {
            assets.save(run).unwrap();
        }
        human
            .save(&resolution(
                1,
                HumanReviewSubject::Document(old_document.id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                2,
                HumanReviewSubject::Document(new_document.id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&resolution(
                3,
                HumanReviewSubject::Asset(old_asset.id()),
                HumanReviewDecision::Reject,
            ))
            .unwrap();
        human
            .save(&resolution(
                4,
                HumanReviewSubject::Asset(new_asset.id()),
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(vec![old_document]),
            &asset_result(&[old_asset]),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.documents()[0].decision(),
            EffectiveDocumentDecision::Rejected
        );
        assert_eq!(
            set.assets()[0].decision(),
            EffectiveReviewDecision::Rejected
        );
    }

    #[test]
    fn run_path_and_snapshot_mismatches_fail_closed() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let stored = document(1, "actual.md", PublicPolicyDecision::ReviewApproved);
        documents.save(&stored).unwrap();
        let selected = document(1, "claimed.md", PublicPolicyDecision::ReviewApproved);
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(vec![selected]),
                &asset_result(&[]),
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::DocumentRunMismatch { .. })
        ));
        let asset_stored = asset(
            1,
            asset_outcome("actual.png", &["a.md"], AssetReviewDisposition::Blocked),
        );
        assets.save(&asset_stored).unwrap();
        let selected_asset = AssetReviewWorkflowResult::from_entries_for_test(
            snapshot(),
            vec![AssetReviewWorkflowEntry::from_parts(
                path("claimed.png"),
                asset_stored.id(),
                asset_stored.outcome().clone(),
            )],
        );
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(Vec::new()),
                &selected_asset,
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::AssetRunMismatch { .. })
        ));
        let other_snapshot = AssetReviewWorkflowResult::from_entries_for_test(
            SnapshotId::new(2).unwrap(),
            Vec::new(),
        );
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(Vec::new()),
                &other_snapshot,
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::SnapshotMismatch { .. })
        ));
    }

    #[derive(Debug)]
    struct TestError;
    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test")
        }
    }
    impl Error for TestError {}
    struct WrongSubjectStore(HumanReviewRecord);
    impl HumanReviewStore for WrongSubjectStore {
        type Error = TestError;
        fn save(&self, _: &HumanReviewRecord) -> Result<(), Self::Error> {
            Ok(())
        }
        fn get(&self, _: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(None)
        }
        fn get_for_subject(
            &self,
            _: HumanReviewSubject,
        ) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(Some(self.0.clone()))
        }
        fn list(&self) -> Result<Vec<HumanReviewRecord>, Self::Error> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn wrong_human_subject_cannot_resolve_a_selected_run() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let run = document(
            1,
            "a.md",
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
        );
        documents.save(&run).unwrap();
        let human = WrongSubjectStore(resolution(
            1,
            HumanReviewSubject::Asset(AssetReviewRunId::new(1).unwrap()),
            HumanReviewDecision::Approve,
        ));
        assert!(matches!(
            EffectiveReviewSet::build(
                &document_result(vec![run]),
                &asset_result(&[]),
                &documents,
                &assets,
                &human
            ),
            Err(EffectiveReviewSetError::DocumentHumanSubjectMismatch(_))
        ));
    }

    #[test]
    fn asset_order_and_shared_dependents_are_preserved() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();
        let late = asset(
            2,
            asset_outcome(
                "z.png",
                &["a.md"],
                AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            ),
        );
        let shared = asset(
            1,
            asset_outcome(
                "shared.png",
                &["a.md", "b.md"],
                AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            ),
        );
        assets.save(&late).unwrap();
        assets.save(&shared).unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(Vec::new()),
            &asset_result(&[late, shared]),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.assets()
                .iter()
                .map(|entry| entry.content_path().as_str())
                .collect::<Vec<_>>(),
            ["shared.png", "z.png"]
        );
        assert_eq!(set.assets()[0].dependents(), [path("a.md"), path("b.md")]);
    }
}
