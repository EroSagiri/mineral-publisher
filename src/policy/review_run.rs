use std::{error::Error, fmt, time::SystemTime};

use crate::{
    domain::{ContentPath, Sha256, Snapshot, SnapshotId},
    public_policy::{PublicPolicyDecision, PublicPolicyOutcome},
};

/// Stable identity of one immutable review attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewRunId(u64);

impl ReviewRunId {
    pub fn new(value: u64) -> Result<Self, ReviewRunError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(ReviewRunError::InvalidReviewRunId)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Versioned identity of the policy which produced a review outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyIdentity {
    name: String,
    version: String,
    hash: Sha256,
}

impl PolicyIdentity {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        hash: Sha256,
    ) -> Result<Self, ReviewRunError> {
        let name = name.into();
        let version = version.into();
        if name.trim().is_empty() {
            return Err(ReviewRunError::InvalidPolicyName);
        }
        if version.trim().is_empty() {
            return Err(ReviewRunError::InvalidPolicyVersion);
        }
        Ok(Self {
            name,
            version,
            hash,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn hash(&self) -> Sha256 {
        self.hash
    }
}

/// Immutable audit fact for one public-policy review attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRun {
    id: ReviewRunId,
    snapshot_id: SnapshotId,
    content_path: ContentPath,
    content_sha256: Sha256,
    policy: PolicyIdentity,
    decision: PublicPolicyDecision,
    created_at_unix_ms: u64,
}

impl ReviewRun {
    /// Binds a policy outcome to the exact file identity in an immutable Snapshot.
    pub fn from_policy_outcome(
        id: ReviewRunId,
        snapshot: &Snapshot,
        outcome: &PublicPolicyOutcome,
        policy: PolicyIdentity,
        created_at: SystemTime,
    ) -> Result<Self, ReviewRunError> {
        let file = snapshot
            .files()
            .iter()
            .find(|file| file.path() == outcome.path())
            .ok_or_else(|| ReviewRunError::DocumentNotInSnapshot(outcome.path().clone()))?;
        if file.sha256() != outcome.content_sha256() {
            return Err(ReviewRunError::ContentIdentityMismatch {
                path: outcome.path().clone(),
                snapshot_sha256: file.sha256(),
                reviewed_sha256: outcome.content_sha256(),
            });
        }
        let created_at_unix_ms = created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| ReviewRunError::CreatedBeforeUnixEpoch)?
            .as_millis()
            .try_into()
            .map_err(|_| ReviewRunError::TimestampOutOfRange)?;

        Ok(Self {
            id,
            snapshot_id: snapshot.id(),
            content_path: outcome.path().clone(),
            content_sha256: outcome.content_sha256(),
            policy,
            decision: outcome.decision().clone(),
            created_at_unix_ms,
        })
    }

    pub fn id(&self) -> ReviewRunId {
        self.id
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn content_path(&self) -> &ContentPath {
        &self.content_path
    }

    pub fn content_sha256(&self) -> Sha256 {
        self.content_sha256
    }

    pub fn policy(&self) -> &PolicyIdentity {
        &self.policy
    }

    pub fn decision(&self) -> &PublicPolicyDecision {
        &self.decision
    }

    /// Program issues stop before semantic review; every other current outcome
    /// was produced after calling the Reviewer.
    pub fn reviewer_was_called(&self) -> bool {
        !matches!(self.decision, PublicPolicyDecision::ProgramIssues(_))
    }

    pub fn needs_human_review(&self) -> bool {
        matches!(self.decision, PublicPolicyDecision::NeedsHumanReview(_))
    }

    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    pub(crate) fn rehydrate(
        id: ReviewRunId,
        snapshot_id: SnapshotId,
        content_path: ContentPath,
        content_sha256: Sha256,
        policy: PolicyIdentity,
        decision: PublicPolicyDecision,
        created_at_unix_ms: u64,
    ) -> Self {
        Self {
            id,
            snapshot_id,
            content_path,
            content_sha256,
            policy,
            decision,
            created_at_unix_ms,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewRunError {
    InvalidReviewRunId,
    InvalidPolicyName,
    InvalidPolicyVersion,
    DocumentNotInSnapshot(ContentPath),
    ContentIdentityMismatch {
        path: ContentPath,
        snapshot_sha256: Sha256,
        reviewed_sha256: Sha256,
    },
    CreatedBeforeUnixEpoch,
    TimestampOutOfRange,
}

impl fmt::Display for ReviewRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReviewRunId => formatter.write_str("review run id must be non-zero"),
            Self::InvalidPolicyName => formatter.write_str("policy name cannot be empty"),
            Self::InvalidPolicyVersion => formatter.write_str("policy version cannot be empty"),
            Self::DocumentNotInSnapshot(path) => {
                write!(formatter, "reviewed document is not in snapshot: {path}")
            }
            Self::ContentIdentityMismatch {
                path,
                snapshot_sha256,
                reviewed_sha256,
            } => write!(
                formatter,
                "reviewed content identity for {path} does not match snapshot: snapshot {snapshot_sha256}, reviewed {reviewed_sha256}"
            ),
            Self::CreatedBeforeUnixEpoch => {
                formatter.write_str("review run timestamp is before the Unix epoch")
            }
            Self::TimestampOutOfRange => {
                formatter.write_str("review run timestamp is outside the supported range")
            }
        }
    }
}

impl Error for ReviewRunError {}

/// Persistence boundary dedicated to Review Run audit facts.
pub trait ReviewRunStore {
    type Error: Error + Send + Sync + 'static;

    /// Saves an immutable attempt. Re-saving an identical ID and fact is
    /// idempotent; reusing an ID for different data must fail.
    fn save(&self, run: &ReviewRun) -> Result<(), Self::Error>;

    fn get(&self, id: ReviewRunId) -> Result<Option<ReviewRun>, Self::Error>;

    fn list_by_snapshot(&self, snapshot_id: SnapshotId) -> Result<Vec<ReviewRun>, Self::Error>;

    fn list_pending_human_review(&self) -> Result<Vec<ReviewRun>, Self::Error>;
}
