use std::{error::Error, fmt, time::SystemTime};

use crate::{
    domain::{ContentPath, Sha256, Snapshot, SnapshotId},
    policy::PolicyIdentity,
};

use super::{AssetReviewDecision, AssetReviewDisposition, AssetReviewOutcome};

/// Stable identity of one immutable asset-review attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AssetReviewRunId(u64);

impl AssetReviewRunId {
    pub fn new(value: u64) -> Result<Self, AssetReviewRunError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(AssetReviewRunError::InvalidAssetReviewRunId)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Immutable audit fact for one asset policy/review attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReviewRun {
    id: AssetReviewRunId,
    snapshot_id: SnapshotId,
    content_path: ContentPath,
    content_sha256: Sha256,
    policy: PolicyIdentity,
    outcome: AssetReviewOutcome,
    created_at_unix_ms: u64,
}

impl AssetReviewRun {
    /// Binds an existing asset outcome to the exact asset version in a Snapshot.
    pub fn from_review_outcome(
        id: AssetReviewRunId,
        snapshot: &Snapshot,
        outcome: AssetReviewOutcome,
        policy: PolicyIdentity,
        created_at: SystemTime,
    ) -> Result<Self, AssetReviewRunError> {
        let file = snapshot
            .files()
            .iter()
            .find(|file| file.path() == outcome.path())
            .ok_or_else(|| AssetReviewRunError::AssetNotInSnapshot(outcome.path().clone()))?;
        if is_markdown(file.path()) {
            return Err(AssetReviewRunError::AssetIsMarkdown(file.path().clone()));
        }
        let reviewed_sha256 = outcome.sha256().ok_or_else(|| {
            AssetReviewRunError::AssetHasNoContentIdentity(outcome.path().clone())
        })?;
        if file.sha256() != reviewed_sha256 {
            return Err(AssetReviewRunError::ContentIdentityMismatch {
                path: outcome.path().clone(),
                snapshot_sha256: file.sha256(),
                reviewed_sha256,
            });
        }
        if !outcome
            .dependents()
            .windows(2)
            .all(|pair| pair[0] < pair[1])
        {
            return Err(AssetReviewRunError::DependentsNotInStableOrder(
                outcome.path().clone(),
            ));
        }
        let created_at_unix_ms = created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| AssetReviewRunError::CreatedBeforeUnixEpoch)?
            .as_millis()
            .try_into()
            .map_err(|_| AssetReviewRunError::TimestampOutOfRange)?;

        Ok(Self {
            id,
            snapshot_id: snapshot.id(),
            content_path: outcome.path().clone(),
            content_sha256: reviewed_sha256,
            policy,
            outcome,
            created_at_unix_ms,
        })
    }

    pub fn id(&self) -> AssetReviewRunId {
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
    pub fn outcome(&self) -> &AssetReviewOutcome {
        &self.outcome
    }
    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    pub fn reviewer_was_called(&self) -> bool {
        matches!(
            self.outcome.disposition(),
            AssetReviewDisposition::Reviewed(_)
        ) || matches!(self.outcome.disposition(), AssetReviewDisposition::NeedsHumanReview(reason)
                if matches!(reason, super::AssetHumanReviewReason::ReviewerFailed(_)))
    }

    pub fn needs_human_review(&self) -> bool {
        matches!(
            self.outcome.disposition(),
            AssetReviewDisposition::NeedsHumanReview(_)
                | AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview)
        )
    }

    #[doc(hidden)]
    pub fn rehydrate(
        id: AssetReviewRunId,
        snapshot_id: SnapshotId,
        content_path: ContentPath,
        content_sha256: Sha256,
        policy: PolicyIdentity,
        outcome: AssetReviewOutcome,
        created_at_unix_ms: u64,
    ) -> Self {
        Self {
            id,
            snapshot_id,
            content_path,
            content_sha256,
            policy,
            outcome,
            created_at_unix_ms,
        }
    }
}

fn is_markdown(path: &ContentPath) -> bool {
    path.as_str()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("md"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetReviewRunError {
    InvalidAssetReviewRunId,
    AssetNotInSnapshot(ContentPath),
    AssetIsMarkdown(ContentPath),
    AssetHasNoContentIdentity(ContentPath),
    DependentsNotInStableOrder(ContentPath),
    ContentIdentityMismatch {
        path: ContentPath,
        snapshot_sha256: Sha256,
        reviewed_sha256: Sha256,
    },
    CreatedBeforeUnixEpoch,
    TimestampOutOfRange,
}

impl fmt::Display for AssetReviewRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAssetReviewRunId => f.write_str("asset review run id must be non-zero"),
            Self::AssetNotInSnapshot(path) => {
                write!(f, "reviewed asset is not in snapshot: {path}")
            }
            Self::AssetIsMarkdown(path) => write!(f, "reviewed asset cannot be Markdown: {path}"),
            Self::AssetHasNoContentIdentity(path) => {
                write!(f, "reviewed asset has no content identity: {path}")
            }
            Self::DependentsNotInStableOrder(path) => {
                write!(
                    f,
                    "reviewed asset dependents are not in stable order: {path}"
                )
            }
            Self::ContentIdentityMismatch {
                path,
                snapshot_sha256,
                reviewed_sha256,
            } => write!(
                f,
                "reviewed asset identity for {path} does not match snapshot: snapshot {snapshot_sha256}, reviewed {reviewed_sha256}"
            ),
            Self::CreatedBeforeUnixEpoch => {
                f.write_str("asset review run timestamp is before the Unix epoch")
            }
            Self::TimestampOutOfRange => {
                f.write_str("asset review run timestamp is outside the supported range")
            }
        }
    }
}
impl Error for AssetReviewRunError {}

/// Persistence boundary dedicated to immutable Asset Review Run audit facts.
pub trait AssetReviewRunStore {
    type Error: Error + Send + Sync + 'static;
    fn save(&self, run: &AssetReviewRun) -> Result<(), Self::Error>;
    fn get(&self, id: AssetReviewRunId) -> Result<Option<AssetReviewRun>, Self::Error>;
    fn list_by_snapshot(&self, snapshot_id: SnapshotId)
    -> Result<Vec<AssetReviewRun>, Self::Error>;
    /// Every durable asset-review fact for one exact subject, in the order it was
    /// recorded. See [`ReviewRunStore::list_by_subject`] for why the subject — not
    /// the snapshot — is the lookup.
    fn list_by_subject(
        &self,
        subject: &crate::policy::ReviewSubjectIdentity,
    ) -> Result<Vec<AssetReviewRun>, Self::Error>;
    fn list_pending_human_review(&self) -> Result<Vec<AssetReviewRun>, Self::Error>;
}
