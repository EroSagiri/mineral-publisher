//! Which durable automatic review fact may answer one review subject.
//!
//! A review result is a conclusion about *reviewed content under a policy*, not
//! about the snapshot it happened to be produced in. Snapshot identity is
//! provenance: it says which source facts the run was derived from, and nothing
//! about whether its conclusion still applies. So reuse is decided by the subject
//! alone — and by whether the durable fact is a conclusion at all:
//!
//! * a validated reviewer report is a conclusion, whatever it decided;
//! * an attempt that failed (timeout, HTTP error, malformed response, transport
//!   error) is a record of the attempt and never a conclusion, so it is never
//!   reused as an automatic answer;
//! * a human decision about the subject takes precedence over every automatic
//!   conclusion, and is applied to the durable attempt that raised the question.
//!
//! The definition of "subject" here is deliberately the same one the human review
//! binding uses, which is *narrower* than the identity of the provider request:
//! the request would be identical for the same bytes under a different path, but
//! reusing across a rename is a widening of trust this stage does not need.

use std::error::Error;
use std::fmt;

use crate::policy::ReviewRun;

use super::{
    AssetReviewRun, HumanReviewAttempt, HumanReviewResolution, HumanReviewStore, HumanReviewSubject,
};

/// Why no durable review fact could answer one subject.
#[derive(Debug)]
pub enum ReviewReuseError<HumanError> {
    /// The human review store could not be read.
    HumanStore(HumanError),
    /// A human decision exists for the subject, but no durable attempt that awaits
    /// it does. Nothing is guessed: the operator's decision is not discarded, and
    /// an automatic conclusion is not allowed to silently outrank it.
    HumanDecisionWithoutPendingAttempt(Box<HumanReviewSubject>),
    /// More than one durable conclusion exists for the subject and they disagree.
    ///
    /// A conclusion is never picked by recency to resolve this: two different
    /// reviewer reports about identical content under an identical policy mean the
    /// subject has no stable answer, and inventing one would hide that.
    ConflictingConclusions(Box<HumanReviewSubject>),
}

impl<HumanError: fmt::Display> fmt::Display for ReviewReuseError<HumanError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HumanStore(error) => write!(formatter, "human review lookup failed: {error}"),
            Self::HumanDecisionWithoutPendingAttempt(subject) => write!(
                formatter,
                "a human decision answers {} but no durable attempt awaits it",
                subject.content_path()
            ),
            Self::ConflictingConclusions(subject) => write!(
                formatter,
                "durable automatic reviews disagree about {} under the same policy",
                subject.content_path()
            ),
        }
    }
}

impl<HumanError: Error + 'static> Error for ReviewReuseError<HumanError> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::HumanStore(error) => Some(error),
            Self::HumanDecisionWithoutPendingAttempt(_) | Self::ConflictingConclusions(_) => None,
        }
    }
}

/// The durable document-review fact that answers one subject, if one exists.
///
/// `runs` must be the subject's facts, in the order they were recorded. The first
/// eligible fact wins, so the same subject always selects the same durable attempt
/// however many snapshots have been reviewed since.
pub fn reuse_document_review<H: HumanReviewStore + ?Sized>(
    subject: &HumanReviewSubject,
    runs: &[ReviewRun],
    human_reviews: &H,
) -> Result<Option<ReviewRun>, ReviewReuseError<H::Error>> {
    let attempts = runs
        .iter()
        .map(|run| HumanReviewAttempt::Document(run.id()))
        .collect::<Vec<_>>();
    if HumanReviewResolution::subject_resolution(subject, attempts.iter().copied(), human_reviews)
        .map_err(ReviewReuseError::HumanStore)?
        .is_some()
    {
        return match runs.iter().find(|run| run.needs_human_review()) {
            Some(run) => Ok(Some(run.clone())),
            None => Err(ReviewReuseError::HumanDecisionWithoutPendingAttempt(
                Box::new(subject.clone()),
            )),
        };
    }

    let conclusions = runs
        .iter()
        .filter(|run| run.reviewer_report().is_some())
        .collect::<Vec<_>>();
    let Some(first) = conclusions.first() else {
        return Ok(None);
    };
    if conclusions
        .iter()
        .any(|run| run.decision() != first.decision())
    {
        return Err(ReviewReuseError::ConflictingConclusions(Box::new(
            subject.clone(),
        )));
    }
    Ok(Some((*first).clone()))
}

/// The durable asset-review fact that answers one subject, if one exists.
///
/// The asset policy decides some subjects without asking a provider at all
/// (`reviewer_was_called` is false). Those are conclusions too — a deterministic
/// policy verdict is not a missing answer — so they are reusable alongside the
/// ones that carry a validated reviewer report.
pub fn reuse_asset_review<H: HumanReviewStore + ?Sized>(
    subject: &HumanReviewSubject,
    runs: &[AssetReviewRun],
    human_reviews: &H,
) -> Result<Option<AssetReviewRun>, ReviewReuseError<H::Error>> {
    let attempts = runs
        .iter()
        .map(|run| HumanReviewAttempt::Asset(run.id()))
        .collect::<Vec<_>>();
    if HumanReviewResolution::subject_resolution(subject, attempts.iter().copied(), human_reviews)
        .map_err(ReviewReuseError::HumanStore)?
        .is_some()
    {
        return match runs.iter().find(|run| run.needs_human_review()) {
            Some(run) => Ok(Some(run.clone())),
            None => Err(ReviewReuseError::HumanDecisionWithoutPendingAttempt(
                Box::new(subject.clone()),
            )),
        };
    }

    let conclusions = runs
        .iter()
        .filter(|run| is_reusable_asset_conclusion(run))
        .collect::<Vec<_>>();
    let Some(first) = conclusions.first() else {
        return Ok(None);
    };
    if conclusions
        .iter()
        .any(|run| run.outcome().disposition() != first.outcome().disposition())
    {
        return Err(ReviewReuseError::ConflictingConclusions(Box::new(
            subject.clone(),
        )));
    }
    Ok(Some((*first).clone()))
}

/// Whether one durable asset-review run states a conclusion rather than a failure.
pub fn is_reusable_asset_conclusion(run: &AssetReviewRun) -> bool {
    !run.reviewer_was_called() || run.outcome().reviewer_report().is_some()
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        domain::{ContentPath, Sha256, SnapshotId},
        policy::{PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId, ReviewerReport},
        workflow::{
            HumanReviewBinding, HumanReviewDecision, HumanReviewId, HumanReviewKind,
            HumanReviewRecord,
        },
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn policy() -> PolicyIdentity {
        PolicyIdentity::new("public", "v1", Sha256::new([1; 32])).unwrap()
    }

    fn report(decision: crate::policy::ReviewDecision) -> ReviewerReport {
        let reasons = match decision {
            crate::policy::ReviewDecision::Approve => {
                vec![crate::policy::ReviewReasonCode::PublicTechnicalContent]
            }
            crate::policy::ReviewDecision::Reject
            | crate::policy::ReviewDecision::NeedsHumanReview => {
                vec![crate::policy::ReviewReasonCode::OtherPrivacyRisk]
            }
        };
        ReviewerReport::new(decision, reasons, "test summary").unwrap()
    }

    /// One durable automatic fact: a conclusion when a report is given, and an
    /// attempt failure when it is not.
    fn run(
        id: u64,
        snapshot: u64,
        decision: PublicPolicyDecision,
        reviewer_report: Option<ReviewerReport>,
    ) -> ReviewRun {
        ReviewRun::rehydrate(
            ReviewRunId::new(id).unwrap(),
            SnapshotId::new(snapshot).unwrap(),
            path("a.md"),
            Sha256::new([9; 32]),
            policy(),
            (decision, reviewer_report),
            id * 10,
        )
    }

    fn approved(id: u64, snapshot: u64) -> ReviewRun {
        run(
            id,
            snapshot,
            PublicPolicyDecision::ReviewApproved,
            Some(report(crate::policy::ReviewDecision::Approve)),
        )
    }

    fn rejected(id: u64, snapshot: u64) -> ReviewRun {
        run(
            id,
            snapshot,
            PublicPolicyDecision::ReviewRejected,
            Some(report(crate::policy::ReviewDecision::Reject)),
        )
    }

    fn failed(id: u64, snapshot: u64) -> ReviewRun {
        run(
            id,
            snapshot,
            PublicPolicyDecision::NeedsHumanReview(
                crate::policy::HumanReviewReason::ReviewerFailed(
                    crate::policy::ReviewerError::new("provider unavailable"),
                ),
            ),
            None,
        )
    }

    fn subject() -> HumanReviewSubject {
        HumanReviewSubject::for_path(
            HumanReviewKind::Document,
            path("a.md"),
            Sha256::new([9; 32]),
            policy(),
        )
    }

    /// A conclusion recorded in an earlier snapshot answers a later one, and the
    /// durable fact itself is what comes back — its provenance intact.
    #[test]
    fn the_first_durable_conclusion_answers_the_subject() {
        let runs = vec![approved(1, 1), approved(2, 2)];

        let reused = reuse_document_review(&subject(), &runs, &NoHumanDecisions)
            .unwrap()
            .expect("a conclusion is reusable");

        assert_eq!(reused.id(), ReviewRunId::new(1).unwrap());
        assert_eq!(reused.snapshot_id(), SnapshotId::new(1).unwrap());
    }

    /// An attempt that failed states nothing about the content, so it is never
    /// reused as an automatic answer.
    #[test]
    fn a_failed_attempt_is_not_a_conclusion() {
        assert!(
            reuse_document_review(&subject(), &[failed(1, 1)], &NoHumanDecisions)
                .unwrap()
                .is_none()
        );
        assert!(
            reuse_document_review(
                &subject(),
                &[failed(1, 1), approved(2, 2)],
                &NoHumanDecisions
            )
            .unwrap()
            .is_some_and(|run| run.id() == ReviewRunId::new(2).unwrap()),
            "a failure does not shadow a conclusion"
        );
    }

    /// Two different conclusions about identical content under one policy mean the
    /// subject has no stable answer. Nothing is picked by recency.
    #[test]
    fn disagreeing_conclusions_fail_closed() {
        let error = reuse_document_review(
            &subject(),
            &[approved(1, 1), rejected(2, 2)],
            &NoHumanDecisions,
        )
        .unwrap_err();

        assert!(matches!(error, ReviewReuseError::ConflictingConclusions(_)));
        // Repeating the same conclusion in another snapshot is not a disagreement.
        assert!(
            reuse_document_review(
                &subject(),
                &[approved(1, 1), approved(2, 2)],
                &NoHumanDecisions
            )
            .unwrap()
            .is_some()
        );
    }

    /// A human decision about the subject is what answers it, and it is applied to
    /// the durable attempt that raised the question — not to a conclusion that
    /// cannot carry it.
    #[test]
    fn a_human_decision_takes_precedence() {
        let human = RecordingHumanDecisions::approving(&subject());
        let runs = vec![failed(1, 1), approved(2, 2)];

        let reused = reuse_document_review(&subject(), &runs, &human)
            .unwrap()
            .expect("the pending attempt is selected");

        assert_eq!(reused.id(), ReviewRunId::new(1).unwrap());
    }

    /// A decision that no durable attempt awaits is reported instead of being
    /// silently outranked by an automatic conclusion.
    #[test]
    fn a_human_decision_without_a_pending_attempt_fails_closed() {
        let human = RecordingHumanDecisions::approving(&subject());

        let error = reuse_document_review(&subject(), &[approved(1, 1)], &human).unwrap_err();

        assert!(matches!(
            error,
            ReviewReuseError::HumanDecisionWithoutPendingAttempt(_)
        ));
    }

    /// The store the reuse rule reads human decisions from.
    #[derive(Default)]
    struct RecordingHumanDecisions {
        subject: Option<HumanReviewSubject>,
    }

    impl RecordingHumanDecisions {
        fn approving(subject: &HumanReviewSubject) -> Self {
            Self {
                subject: Some(subject.clone()),
            }
        }
    }

    impl HumanReviewStore for RecordingHumanDecisions {
        type Error = std::convert::Infallible;

        fn save(&self, _: &HumanReviewRecord) -> Result<(), Self::Error> {
            Ok(())
        }

        fn get(&self, _: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok(None)
        }

        fn get_for_subject(
            &self,
            subject: &HumanReviewSubject,
        ) -> Result<Option<HumanReviewRecord>, Self::Error> {
            Ok((self.subject.as_ref() == Some(subject)).then(|| {
                HumanReviewRecord::new(
                    HumanReviewId::new(1).unwrap(),
                    HumanReviewBinding::Subject {
                        subject: subject.clone(),
                        attempt: HumanReviewAttempt::Document(ReviewRunId::new(1).unwrap()),
                    },
                    HumanReviewDecision::Approve,
                    SystemTime::UNIX_EPOCH,
                    None,
                    None,
                )
                .unwrap()
            }))
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

    /// No decisions at all, for the cases that are only about automatic facts.
    struct NoHumanDecisions;

    impl HumanReviewStore for NoHumanDecisions {
        type Error = std::convert::Infallible;

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
            Ok(None)
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
}
