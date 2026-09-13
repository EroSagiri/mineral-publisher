use std::{error::Error, fmt, time::SystemTime};

use crate::{
    domain::{ContentPath, Sha256},
    policy::{
        PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId, ReviewRunStore,
        ReviewSubjectIdentity,
    },
};

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
pub enum HumanReviewKind {
    Document,
    Asset,
}

/// The subject one automatic review outcome is about.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HumanReviewSubject {
    kind: HumanReviewKind,
    identity: ReviewSubjectIdentity,
}

impl HumanReviewSubject {
    fn new(kind: HumanReviewKind, identity: ReviewSubjectIdentity) -> Self {
        Self { kind, identity }
    }

    /// The subject of one reviewed file, before any attempt about it exists.
    ///
    /// The review workflows use this to recognise a question a human has already
    /// answered without running the provider again.
    pub fn for_path(
        kind: HumanReviewKind,
        content_path: ContentPath,
        content_sha256: Sha256,
        policy: PolicyIdentity,
    ) -> Self {
        Self::new(
            kind,
            ReviewSubjectIdentity::new(content_path, content_sha256, policy),
        )
    }

    pub fn document(run: &ReviewRun) -> Self {
        Self::new(
            HumanReviewKind::Document,
            ReviewSubjectIdentity::new(
                run.content_path().clone(),
                run.content_sha256(),
                run.policy().clone(),
            ),
        )
    }

    pub fn asset(run: &AssetReviewRun) -> Self {
        Self::new(
            HumanReviewKind::Asset,
            ReviewSubjectIdentity::new(
                run.content_path().clone(),
                run.content_sha256(),
                run.policy().clone(),
            ),
        )
    }

    pub fn kind(&self) -> HumanReviewKind {
        self.kind
    }

    pub fn identity(&self) -> &ReviewSubjectIdentity {
        &self.identity
    }

    pub fn content_path(&self) -> &ContentPath {
        self.identity.content_path()
    }

    pub fn content_sha256(&self) -> Sha256 {
        self.identity.content_sha256()
    }

    pub fn policy(&self) -> &PolicyIdentity {
        self.identity.policy()
    }
}

/// One automatic review attempt, as an audit identity.
///
/// It is what a decision written before subject binding named, and it stays in the
/// audit trail of a current decision as the attempt that raised the question.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HumanReviewAttempt {
    Document(ReviewRunId),
    Asset(AssetReviewRunId),
}

impl HumanReviewAttempt {
    pub fn kind(self) -> HumanReviewKind {
        match self {
            Self::Document(_) => HumanReviewKind::Document,
            Self::Asset(_) => HumanReviewKind::Asset,
        }
    }

    pub fn run_id(self) -> u64 {
        match self {
            Self::Document(id) => id.get(),
            Self::Asset(id) => id.get(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumanReviewDecision {
    Approve,
    Reject,
}

/// What one human review record binds to.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HumanReviewBinding {
    /// The current binding: the decision is about this subject, and this attempt
    /// is the one that raised the question.
    Subject {
        subject: HumanReviewSubject,
        attempt: HumanReviewAttempt,
    },
    /// A record written before subjects were bound: it decided about exactly one
    /// automatic attempt and nothing else. Kept readable, and never widened to a
    /// subject its author never saw.
    AttemptOnly(HumanReviewAttempt),
}

/// Immutable audit fact recording a final human decision about one review subject.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanReviewRecord {
    id: HumanReviewId,
    binding: HumanReviewBinding,
    decision: HumanReviewDecision,
    created_at_unix_ms: u64,
    reviewer: Option<String>,
    note: Option<String>,
}

impl HumanReviewRecord {
    #[doc(hidden)]
    pub fn new(
        id: HumanReviewId,
        binding: HumanReviewBinding,
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
            binding,
            decision,
            created_at_unix_ms,
            reviewer,
            note,
        })
    }

    pub fn id(&self) -> HumanReviewId {
        self.id
    }

    pub fn binding(&self) -> &HumanReviewBinding {
        &self.binding
    }

    /// The subject this decision is about, when it was written under the current
    /// binding.
    pub fn subject(&self) -> Option<&HumanReviewSubject> {
        match &self.binding {
            HumanReviewBinding::Subject { subject, .. } => Some(subject),
            HumanReviewBinding::AttemptOnly(_) => None,
        }
    }

    /// Whether this decision answers the question one automatic outcome raised.
    ///
    /// A subject-bound decision answers every attempt about that subject; an
    /// attempt-bound decision answers only the attempt it named.
    pub fn applies_to(&self, subject: &HumanReviewSubject, attempt: HumanReviewAttempt) -> bool {
        match &self.binding {
            HumanReviewBinding::Subject { subject: bound, .. } => bound == subject,
            HumanReviewBinding::AttemptOnly(bound) => *bound == attempt,
        }
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
        binding: HumanReviewBinding,
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
            binding,
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
    /// The decision recorded for this exact subject under the current binding.
    fn get_for_subject(
        &self,
        subject: &HumanReviewSubject,
    ) -> Result<Option<HumanReviewRecord>, Self::Error>;
    /// The decision recorded for one automatic attempt under the pre-subject
    /// binding.
    fn get_for_attempt(
        &self,
        attempt: HumanReviewAttempt,
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
    /// Boxed: a binding carries the whole reviewed subject, and this error travels
    /// through every review outcome.
    expected: Box<HumanReviewBinding>,
    actual: Box<HumanReviewBinding>,
}

impl fmt::Display for EffectiveReviewDecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "human resolution {:?} does not answer the automatic review {:?}",
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
        let attempt = HumanReviewAttempt::Document(review_run_id);
        let run = automatic_store
            .get(review_run_id)
            .map_err(HumanReviewResolutionError::AutomaticStore)?
            .ok_or(HumanReviewResolutionError::AutomaticReviewNotFound(attempt))?;
        Self::resolve(
            human_store,
            id,
            HumanReviewSubject::document(&run),
            attempt,
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
        let attempt = HumanReviewAttempt::Asset(review_run_id);
        let run = automatic_store
            .get(review_run_id)
            .map_err(HumanReviewResolutionError::AutomaticStore)?
            .ok_or(HumanReviewResolutionError::AutomaticReviewNotFound(attempt))?;
        Self::resolve(
            human_store,
            id,
            HumanReviewSubject::asset(&run),
            attempt,
            run.needs_human_review(),
            decision,
            created_at,
            reviewer,
            note,
        )
    }

    /// The human decision that answers one subject, wherever its attempt was
    /// recorded.
    ///
    /// The subject is looked up first, so a decision survives every later attempt
    /// about the same content under the same policy. A decision written before
    /// subjects were bound is found through the attempts it could have named: it
    /// keeps exactly the meaning its author gave it.
    pub fn subject_resolution<H: HumanReviewStore + ?Sized>(
        subject: &HumanReviewSubject,
        attempts: impl IntoIterator<Item = HumanReviewAttempt>,
        human_store: &H,
    ) -> Result<Option<HumanReviewRecord>, H::Error> {
        if let Some(record) = human_store.get_for_subject(subject)? {
            return Ok(Some(record));
        }
        for attempt in attempts {
            if let Some(record) = human_store.get_for_attempt(attempt)? {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// The human decision that answers one automatic document outcome.
    ///
    /// The subject is looked up first, so a decision survives every later attempt
    /// about the same content under the same policy. A decision written before
    /// subjects were bound is found through the attempt it named: it keeps exactly
    /// the meaning its author gave it.
    pub fn document_resolution<H: HumanReviewStore + ?Sized>(
        run: &ReviewRun,
        human_store: &H,
    ) -> Result<Option<HumanReviewRecord>, H::Error> {
        Self::subject_resolution(
            &HumanReviewSubject::document(run),
            [HumanReviewAttempt::Document(run.id())],
            human_store,
        )
    }

    /// The human decision that answers one automatic asset outcome.
    pub fn asset_resolution<H: HumanReviewStore + ?Sized>(
        run: &AssetReviewRun,
        human_store: &H,
    ) -> Result<Option<HumanReviewRecord>, H::Error> {
        Self::subject_resolution(
            &HumanReviewSubject::asset(run),
            [HumanReviewAttempt::Asset(run.id())],
            human_store,
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
        Self::without_resolutions(runs, human_store, Self::document_resolution)
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
        Self::without_resolutions(runs, human_store, Self::asset_resolution)
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
            PublicPolicyDecision::NeedsHumanReview(_) => Self::effective_human(
                HumanReviewSubject::document(run),
                HumanReviewAttempt::Document(run.id()),
                resolution,
            ),
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
                Self::effective_human(
                    HumanReviewSubject::asset(run),
                    HumanReviewAttempt::Asset(run.id()),
                    resolution,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve<A, H>(
        human_store: &H,
        id: HumanReviewId,
        subject: HumanReviewSubject,
        attempt: HumanReviewAttempt,
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
            return Err(HumanReviewResolutionError::NotPendingHumanReview(attempt));
        }
        let record = HumanReviewRecord::new(
            id,
            HumanReviewBinding::Subject { subject, attempt },
            decision,
            created_at,
            reviewer,
            note,
        )
        .map_err(HumanReviewResolutionError::InvalidRecord)?;
        human_store
            .save(&record)
            .map_err(HumanReviewResolutionError::HumanStore)?;
        Ok(record)
    }

    fn without_resolutions<T, A, H>(
        runs: Vec<T>,
        human_store: &H,
        resolution: impl Fn(&T, &H) -> Result<Option<HumanReviewRecord>, H::Error>,
    ) -> Result<Vec<T>, HumanReviewResolutionError<A, H::Error>>
    where
        A: Error + Send + Sync + 'static,
        H: HumanReviewStore + ?Sized,
    {
        let mut pending = Vec::new();
        for run in runs {
            if resolution(&run, human_store)
                .map_err(HumanReviewResolutionError::HumanStore)?
                .is_none()
            {
                pending.push(run);
            }
        }
        Ok(pending)
    }

    fn effective_human(
        subject: HumanReviewSubject,
        attempt: HumanReviewAttempt,
        resolution: Option<&HumanReviewRecord>,
    ) -> Result<EffectiveReviewDecision, EffectiveReviewDecisionError> {
        let Some(resolution) = resolution else {
            return Ok(EffectiveReviewDecision::PendingHumanReview);
        };
        if !resolution.applies_to(&subject, attempt) {
            return Err(EffectiveReviewDecisionError {
                expected: Box::new(HumanReviewBinding::Subject { subject, attempt }),
                actual: Box::new(resolution.binding().clone()),
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
    AutomaticReviewNotFound(HumanReviewAttempt),
    NotPendingHumanReview(HumanReviewAttempt),
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
            Self::AutomaticReviewNotFound(attempt) => {
                write!(formatter, "automatic review does not exist: {attempt:?}")
            }
            Self::NotPendingHumanReview(attempt) => write!(
                formatter,
                "automatic review is not awaiting human review: {attempt:?}"
            ),
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
