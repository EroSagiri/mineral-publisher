//! The private backup: freeze a Snapshot, deliver it to Git, store binaries in LFS.
//!
//! A backup is the mirror of a publication. It is byte-faithful and restorable,
//! it moves its ref only after every required LFS object is confirmed, and it
//! refuses to run at all in a workspace that has no backup target. That refusal
//! is a value in the outcome, not a printed line.

use std::{error::Error, fmt, time::SystemTime};

use mineral_core::backup::{
    BackupExecutionOutcome, BackupRunStore, TypeFirstBackupRepresentationPolicy, verify_backup,
};

use crate::{
    domain::{SnapshotId, TimestampMillis},
    publisher::{GitRefTarget, RemoteRefState},
    runtime::{Progress, RuntimeError, WorkspaceRuntime},
};

use super::ApplicationError;

/// What one backup is asked for.
#[derive(Clone, Copy, Debug)]
pub struct BackupRequest {
    /// The instant the durable intent is recorded as beginning.
    ///
    /// Time is an input, not something a use case reads.
    pub created_at: TimestampMillis,
}

impl BackupRequest {
    /// The request a caller makes right now.
    pub fn now() -> Result<Self, ApplicationError> {
        Ok(Self {
            created_at: TimestampMillis::from_system_time(SystemTime::now()).ok_or_else(|| {
                ApplicationError::Unsupported {
                    message: "system time is before the Unix epoch".to_owned(),
                }
            })?,
        })
    }
}

/// What one backup attempt did.
#[derive(Debug)]
pub struct BackupOutcome {
    /// The Snapshot the backup froze.
    pub snapshot_id: SnapshotId,
    /// How many files that Snapshot holds.
    pub files: usize,
    /// How many of them are stored as Git LFS objects.
    pub lfs_objects: usize,
    /// The durable intent, present unless the snapshot was already backed up.
    pub run_id: Option<mineral_core::backup::BackupRunId>,
    /// What the engine did with the ref.
    pub execution: BackupExecutionOutcome,
    /// The remote and ref the backup compares-and-swaps.
    pub target: GitRefTarget,
    /// How the LFS endpoint describes itself, with the username but no token.
    pub endpoint: String,
}

/// Why one backup could not complete.
#[derive(Debug)]
pub enum BackupError {
    /// The workspace, the credential or an adapter could not be used.
    Application(ApplicationError),
    /// The backup ref does not exist yet, so there is no base to build on.
    ///
    /// This is one operator action away from a backup — `mineral backup init` —
    /// so it is a distinct outcome rather than a failure message, and it carries
    /// the Snapshot the attempt had already frozen so a report can name it.
    NoBaseCommit {
        target: GitRefTarget,
        snapshot_id: SnapshotId,
        files: usize,
    },
}

impl fmt::Display for BackupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Application(error) => write!(formatter, "{error}"),
            Self::NoBaseCommit { target, .. } => write!(
                formatter,
                "backup ref {} {} does not exist yet; run `mineral backup init` once to create it",
                target.remote_name(),
                target.destination_ref()
            ),
        }
    }
}

impl Error for BackupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Application(error) => Some(error),
            Self::NoBaseCommit { .. } => None,
        }
    }
}

impl From<ApplicationError> for BackupError {
    fn from(error: ApplicationError) -> Self {
        Self::Application(error)
    }
}

impl From<RuntimeError> for BackupError {
    fn from(error: RuntimeError) -> Self {
        Self::Application(ApplicationError::Runtime(error))
    }
}

impl From<crate::config::ConfigError> for BackupError {
    fn from(error: crate::config::ConfigError) -> Self {
        Self::Application(ApplicationError::from(error))
    }
}

/// The report of one backup, or a refusal because the workspace has no target.
#[derive(Debug)]
pub enum BackupResult {
    /// This workspace does not back up.
    NotConfigured,
    /// One attempt finished, successfully or not.
    Attempted(Box<BackupOutcome>),
}

/// Runs one backup of a fresh Snapshot.
pub fn backup(
    runtime: &WorkspaceRuntime,
    request: BackupRequest,
    progress: &dyn Progress,
) -> Result<BackupResult, BackupError> {
    if !runtime.backup_enabled() {
        return Ok(BackupResult::NotConfigured);
    }
    runtime.prepare()?;
    let content_store = runtime.content_store();
    progress.stage("Backup: create immutable source snapshot");
    let snapshot = runtime.snapshot(progress)?;
    let store = runtime.backup_runs()?;
    let repository = runtime.backup_repository()?;
    let remote = runtime.backup_git_remote()?;
    let lfs = runtime.backup_lfs_remote()?;
    let target = runtime.backup_target()?;
    let metadata = runtime.backup_commit_metadata()?;
    let policy = TypeFirstBackupRepresentationPolicy;
    let request = crate::backup::application::BackupApplicationRequest {
        progress,
        snapshot: &snapshot,
        target: &target,
        commit_metadata: &metadata,
        policy: &policy,
        created_at: request.created_at,
    };
    let run_ids = crate::runtime::composition::UuidBackupRunIdGenerator;
    let outcome = match crate::backup::application::run_backup(
        request,
        &store,
        &repository,
        &remote,
        &lfs,
        &content_store,
        &run_ids,
    ) {
        Ok(outcome) => outcome,
        Err(crate::backup::application::BackupApplicationError::NoBaseCommit(target)) => {
            return Err(BackupError::NoBaseCommit {
                target,
                snapshot_id: snapshot.id(),
                files: snapshot.files().len(),
            });
        }
        Err(error) => {
            return Err(BackupError::Application(ApplicationError::operation(
                "backup", error,
            )));
        }
    };
    Ok(BackupResult::Attempted(Box::new(BackupOutcome {
        snapshot_id: outcome.snapshot_id(),
        files: outcome.files(),
        lfs_objects: outcome.lfs_objects(),
        run_id: outcome.run_id(),
        execution: outcome.execution().clone(),
        target,
        endpoint: lfs.describe().to_owned(),
    })))
}

/// What the observed backup ref and the newest durable intent say.
#[derive(Debug)]
pub enum BackupStatusOutcome {
    /// This workspace does not back up.
    NotConfigured,
    /// The observed state of the ref and the newest durable run.
    Reported {
        remote: String,
        reference: String,
        /// The observed ref: present at a commit, missing, or unavailable.
        reference_state: String,
        newest_run: Option<BackupRunSummary>,
    },
}

/// One durable backup intent, in the terms an operator reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupRunSummary {
    pub run_id: String,
    pub snapshot_id: String,
    pub delivery_sha256: String,
}

/// Reports the observed backup ref and the newest durable intent.
pub fn backup_status(runtime: &WorkspaceRuntime) -> Result<BackupStatusOutcome, ApplicationError> {
    if !runtime.backup_enabled() {
        return Ok(BackupStatusOutcome::NotConfigured);
    }
    let target = runtime.backup_target()?;
    let reference_state = match runtime.observe_backup_ref(&target) {
        Ok(RemoteRefState::Present { commit_oid }) => format!("({})", commit_oid.as_str()),
        Ok(RemoteRefState::Missing) => "(missing)".to_owned(),
        Err(error) => format!("(unavailable: {error})"),
    };
    let store = runtime.backup_runs()?;
    let newest_run = store
        .list()
        .map_err(|error| ApplicationError::operation("list backup runs", error))?
        .last()
        .map(|run| BackupRunSummary {
            run_id: run.id().get().to_string(),
            snapshot_id: run.snapshot_id().get().to_string(),
            delivery_sha256: run.delivery_sha256().to_string(),
        });
    Ok(BackupStatusOutcome::Reported {
        remote: target.remote_name().to_owned(),
        reference: target.destination_ref().to_owned(),
        reference_state,
        newest_run,
    })
}

/// What verifying the backup ref found.
#[derive(Debug)]
pub enum BackupVerifyOutcome {
    /// This workspace does not back up.
    NotConfigured,
    /// Every file in the ref was read back and matched.
    Verified {
        commit: String,
        files: usize,
        lfs_objects: usize,
    },
}

/// Verifies that the backup ref can be restored byte-for-byte.
pub fn backup_verify(runtime: &WorkspaceRuntime) -> Result<BackupVerifyOutcome, ApplicationError> {
    if !runtime.backup_enabled() {
        return Ok(BackupVerifyOutcome::NotConfigured);
    }
    let repository = runtime.backup_repository()?;
    let remote = runtime.backup_git_remote()?;
    let lfs = runtime.backup_lfs_remote()?;
    let target = runtime.backup_target()?;
    let report = verify_backup(&repository, &remote, &lfs, &target, None)
        .map_err(|error| ApplicationError::operation("backup verification", error))?;
    Ok(BackupVerifyOutcome::Verified {
        commit: report.commit().as_str().to_owned(),
        files: report.files_verified(),
        lfs_objects: report.lfs_objects_verified(),
    })
}

/// What bootstrapping the backup ref did.
///
/// The ref is part of every answer, so a renderer never has to look at the
/// configuration again to describe what happened.
#[derive(Debug)]
pub enum BackupInitOutcome {
    /// This workspace does not back up.
    NotConfigured,
    /// The ref already exists and was not touched.
    AlreadyPresent {
        target: GitRefTarget,
        commit_oid: String,
    },
    /// The empty root commit was created and pushed.
    Created {
        target: GitRefTarget,
        commit_oid: String,
    },
}

/// Bootstraps the backup ref with one empty root commit.
///
/// The engine builds every backup on the commit the ref already holds, so the
/// very first backup needs a base derived from nothing. An existing ref is never
/// touched.
pub fn backup_init(runtime: &WorkspaceRuntime) -> Result<BackupInitOutcome, ApplicationError> {
    if !runtime.backup_enabled() {
        return Ok(BackupInitOutcome::NotConfigured);
    }
    let target = runtime.backup_target()?;
    match runtime.observe_backup_ref(&target)? {
        RemoteRefState::Present { commit_oid } => Ok(BackupInitOutcome::AlreadyPresent {
            target,
            commit_oid: commit_oid.as_str().to_owned(),
        }),
        RemoteRefState::Missing => {
            let metadata = runtime.backup_commit_metadata()?;
            let commit = runtime.backup_root_commit(&metadata)?;
            runtime.push_backup_root_commit(&target, &commit)?;
            Ok(BackupInitOutcome::Created {
                target,
                commit_oid: commit.as_str().to_owned(),
            })
        }
    }
}
