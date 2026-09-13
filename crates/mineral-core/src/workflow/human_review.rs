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
    #[doc(hidden)]
    pub fn new(
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

    #[doc(hidden)]
    pub fn rehydrate(
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
