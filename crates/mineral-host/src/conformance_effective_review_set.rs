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
        storage::{
            SqliteAssetReviewRunStore, SqliteHumanReviewStore, SqliteHumanReviewStoreError,
            SqliteReviewRunStore,
        },
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
    /// A current decision: bound to the subject, raised by the attempt.
    fn resolution(
        id: u64,
        subject: HumanReviewSubject,
        attempt: HumanReviewAttempt,
        decision: HumanReviewDecision,
    ) -> HumanReviewRecord {
        HumanReviewRecord::rehydrate(
            HumanReviewId::new(id).unwrap(),
            HumanReviewBinding::Subject { subject, attempt },
            decision,
            id,
            None,
            None,
        )
        .unwrap()
    }

    fn document_decision(
        id: u64,
        run: &ReviewRun,
        decision: HumanReviewDecision,
    ) -> HumanReviewRecord {
        resolution(
            id,
            HumanReviewSubject::document(run),
            HumanReviewAttempt::Document(run.id()),
            decision,
        )
    }

    fn asset_decision(
        id: u64,
        run: &AssetReviewRun,
        decision: HumanReviewDecision,
    ) -> HumanReviewRecord {
        resolution(
            id,
            HumanReviewSubject::asset(run),
            HumanReviewAttempt::Asset(run.id()),
            decision,
        )
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
            .save(&document_decision(
                1,
                &runs[4],
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&document_decision(2, &runs[5], HumanReviewDecision::Reject))
            .unwrap();
        human
            .save(&document_decision(
                3,
                &runs[6],
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
            .save(&asset_decision(1, &runs[4], HumanReviewDecision::Approve))
            .unwrap();
        human
            .save(&asset_decision(2, &runs[5], HumanReviewDecision::Reject))
            .unwrap();
        human
            .save(&asset_decision(3, &runs[6], HumanReviewDecision::Approve))
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

    /// S6.5: a decision belongs to the reviewed content and the policy, so it
    /// answers every later attempt of that subject, and one subject never holds
    /// two answers.
    #[test]
    fn one_decision_answers_every_attempt_of_its_subject() {
        let documents = SqliteReviewRunStore::open(":memory:").unwrap();
        let assets = SqliteAssetReviewRunStore::open(":memory:").unwrap();
        let human = SqliteHumanReviewStore::open(":memory:").unwrap();

        // Two attempts of the same reviewed content under the same policy: only the
        // attempt identity differs.
        let content = Sha256::new([9; 32]);
        let document_attempt = |id: u64| {
            ReviewRun::rehydrate(
                ReviewRunId::new(id).unwrap(),
                snapshot(),
                path("a.md"),
                content,
                policy(),
                (
                    PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
                    None,
                ),
                id,
            )
        };
        let first_document = document_attempt(1);
        let second_document = document_attempt(2);
        documents.save(&first_document).unwrap();
        documents.save(&second_document).unwrap();

        let asset_outcome = asset_outcome(
            "a.png",
            &["a.md"],
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
        );
        let asset_attempt = |id: u64| {
            AssetReviewRun::rehydrate(
                AssetReviewRunId::new(id).unwrap(),
                snapshot(),
                path("a.png"),
                Sha256::new([3; 32]),
                policy(),
                asset_outcome.clone(),
                id,
            )
        };
        let first_asset = asset_attempt(1);
        let second_asset = asset_attempt(2);
        assets.save(&first_asset).unwrap();
        assets.save(&second_asset).unwrap();

        // The operator answers the attempt that raised the question.
        human
            .save(&document_decision(
                1,
                &first_document,
                HumanReviewDecision::Approve,
            ))
            .unwrap();
        human
            .save(&asset_decision(
                3,
                &first_asset,
                HumanReviewDecision::Reject,
            ))
            .unwrap();

        // A later attempt of the same subject reuses the decision: the provider is
        // never asked again, and the delivery is not blocked.
        let set = EffectiveReviewSet::build(
            &document_result(vec![second_document.clone()]),
            &asset_result(std::slice::from_ref(&second_asset)),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.documents()[0].decision(),
            EffectiveDocumentDecision::Approved
        );
        assert_eq!(
            set.documents()[0].review_run_id(),
            Some(second_document.id())
        );
        assert_eq!(
            set.assets()[0].decision(),
            EffectiveReviewDecision::Rejected
        );
        assert_eq!(set.assets()[0].review_run_id(), second_asset.id());

        // One subject, one answer: a second decision about the same content and
        // policy is refused rather than silently overriding the first.
        assert!(matches!(
            human.save(&document_decision(
                2,
                &second_document,
                HumanReviewDecision::Reject
            )),
            Err(SqliteHumanReviewStoreError::SubjectAlreadyResolved(_))
        ));

        // Different content is a different subject, and the decision does not
        // follow it.
        let changed = ReviewRun::rehydrate(
            ReviewRunId::new(3).unwrap(),
            snapshot(),
            path("a.md"),
            Sha256::new([8; 32]),
            policy(),
            (
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
                None,
            ),
            3,
        );
        documents.save(&changed).unwrap();
        let set = EffectiveReviewSet::build(
            &document_result(vec![changed]),
            &asset_result(&[]),
            &documents,
            &assets,
            &human,
        )
        .unwrap();
        assert_eq!(
            set.documents()[0].decision(),
            EffectiveDocumentDecision::PendingHumanReview
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
            _: &HumanReviewSubject,
        ) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(Some(self.0.clone()))
        }
        fn get_for_attempt(
            &self,
            _: HumanReviewAttempt,
        ) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(None)
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
            HumanReviewSubject::for_path(
                HumanReviewKind::Asset,
                path("a.md"),
                Sha256::new([9; 32]),
                policy(),
            ),
            HumanReviewAttempt::Document(run.id()),
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
