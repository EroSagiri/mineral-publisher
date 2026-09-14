use std::{error::Error, fmt};

use crate::{
    domain::{Sha256, SnapshotId},
    publication::git::{GitCommitOid, GitCommitSpec, GitRefTarget},
};

use super::{delivery::BackupDeliveryProjection, lfs::RequiredLfsObject};

/// Stable identity of one backup attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BackupRunId(u64);

impl BackupRunId {
    pub fn new(value: u64) -> Result<Self, BackupRunIdError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(BackupRunIdError::InvalidBackupRunId)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupRunIdError {
    InvalidBackupRunId,
}

impl fmt::Display for BackupRunIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("backup run id must be non-zero")
    }
}

impl Error for BackupRunIdError {}

/// The frozen intent of one backup attempt.
///
/// It is persisted *before* any remote side effect, and it freezes everything a
/// resume needs: the Snapshot, the full delivery (paths, representations, pointer
/// blobs, required LFS objects), the base commit the compare-and-swap expects, the
/// exact commit specification and the commit it must create. A restart therefore
/// continues the same attempt instead of re-deriving one from today's configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupRun {
    id: BackupRunId,
    snapshot_id: SnapshotId,
    backup_projection_sha256: Sha256,
    delivery: BackupDeliveryProjection,
    base_commit: GitCommitOid,
    desired_commit: GitCommitOid,
    target: GitRefTarget,
    commit_spec: GitCommitSpec,
    created_at_unix_ms: u64,
}

impl BackupRun {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: BackupRunId,
        snapshot_id: SnapshotId,
        backup_projection_sha256: Sha256,
        delivery: BackupDeliveryProjection,
        base_commit: GitCommitOid,
        desired_commit: GitCommitOid,
        target: GitRefTarget,
        commit_spec: GitCommitSpec,
        created_at_unix_ms: u64,
    ) -> Result<Self, BackupRunError> {
        if delivery.snapshot_id() != snapshot_id {
            return Err(BackupRunError::SnapshotMismatch);
        }
        if delivery.source_projection_sha256() != backup_projection_sha256 {
            return Err(BackupRunError::ProjectionMismatch);
        }
        if commit_spec.parent() != &base_commit {
            return Err(BackupRunError::BaseMismatch);
        }
        Ok(Self {
            id,
            snapshot_id,
            backup_projection_sha256,
            delivery,
            base_commit,
            desired_commit,
            target,
            commit_spec,
            created_at_unix_ms,
        })
    }

    pub fn id(&self) -> BackupRunId {
        self.id
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn backup_projection_sha256(&self) -> Sha256 {
        self.backup_projection_sha256
    }

    pub fn delivery(&self) -> &BackupDeliveryProjection {
        &self.delivery
    }

    pub fn delivery_sha256(&self) -> Sha256 {
        self.delivery.delivery_sha256()
    }

    /// Invariant 2, frozen at intent time: the objects and their sizes this commit
    /// requires before its ref may move.
    pub fn required_lfs_objects(&self) -> &[RequiredLfsObject] {
        self.delivery.required_lfs_objects()
    }

    pub fn base_commit(&self) -> &GitCommitOid {
        &self.base_commit
    }

    pub fn desired_commit(&self) -> &GitCommitOid {
        &self.desired_commit
    }

    pub fn target(&self) -> &GitRefTarget {
        &self.target
    }

    pub fn commit_spec(&self) -> &GitCommitSpec {
        &self.commit_spec
    }

    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }
}

/// Durable storage for backup intents.
pub trait BackupRunStore {
    type Error: Error + 'static;

    /// Persists one intent. Recording the same intent twice is success; recording a
    /// different fact under one id fails closed.
    fn save(&self, run: &BackupRun) -> Result<(), Self::Error>;

    fn get(&self, id: BackupRunId) -> Result<Option<BackupRun>, Self::Error>;

    fn list(&self) -> Result<Vec<BackupRun>, Self::Error>;
}

/// Why one backup intent cannot be frozen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupRunError {
    /// The delivery belongs to another Snapshot.
    SnapshotMismatch,
    /// The delivery was built from another backup projection.
    ProjectionMismatch,
    /// The commit specification does not build on the observed base commit.
    BaseMismatch,
}

impl fmt::Display for BackupRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch => {
                formatter.write_str("backup delivery and intent name different snapshots")
            }
            Self::ProjectionMismatch => {
                formatter.write_str("backup delivery and intent name different projections")
            }
            Self::BaseMismatch => {
                formatter.write_str("backup commit specification does not build on the base commit")
            }
        }
    }
}

impl Error for BackupRunError {}
