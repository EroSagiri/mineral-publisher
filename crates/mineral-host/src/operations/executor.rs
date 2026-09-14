//! Running an operation.
//!
//! The supervisor knows how to observe work; it does not know what the work is.
//! That seam is this trait, and it buys two things: the supervisor can be tested
//! with an executor that blocks on purpose (which is how single-flight is proved
//! without waiting for a real publication), and a future host can supply a
//! different executor without touching the supervisor.

use std::sync::Arc;

use crate::{
    application::{
        backup::{self, BackupError},
        doctor, publish, review,
        review::ReviewRequest,
    },
    runtime::{Progress, WorkspaceRuntime},
};

use super::model::{
    OperationErrorCode, OperationFailure, OperationRequest, OperationResult, ReviewOperation,
};

/// Runs one operation and reports progress while it does.
///
/// An implementation must not print: everything an operator sees is either a
/// progress line through the sink or a value in the result.
pub trait OperationExecutor: Send + Sync {
    fn execute(
        &self,
        request: &OperationRequest,
        progress: Arc<dyn Progress>,
    ) -> Result<OperationResult, OperationFailure>;
}

/// The production executor: the application layer over one workspace.
pub struct ApplicationExecutor {
    runtime: Arc<WorkspaceRuntime>,
}

impl ApplicationExecutor {
    pub fn new(runtime: Arc<WorkspaceRuntime>) -> Self {
        Self { runtime }
    }

    /// The workspace this executor drives.
    pub fn runtime(&self) -> &WorkspaceRuntime {
        &self.runtime
    }
}

impl OperationExecutor for ApplicationExecutor {
    fn execute(
        &self,
        request: &OperationRequest,
        progress: Arc<dyn Progress>,
    ) -> Result<OperationResult, OperationFailure> {
        match request {
            OperationRequest::Publish(request) => {
                publish::publish(&self.runtime, request.clone(), &progress)
                    .map(|outcome| OperationResult::Published(Box::new(outcome)))
                    .map_err(|error| {
                        OperationFailure::classify(OperationErrorCode::PublicationFailed, &error)
                    })
            }

            OperationRequest::Backup(request) => {
                match backup::backup(&self.runtime, *request, progress.as_ref()) {
                    Ok(result) => Ok(OperationResult::BackedUp(result)),
                    Err(BackupError::NoBaseCommit {
                        target,
                        snapshot_id,
                        files,
                    }) => {
                        let message = BackupError::NoBaseCommit {
                            target: target.clone(),
                            snapshot_id,
                            files,
                        }
                        .to_string();
                        Err(OperationFailure::BackupNoBaseCommit {
                            target,
                            snapshot_id,
                            files,
                            message,
                        })
                    }
                    Err(BackupError::Application(error)) => Err(OperationFailure::classify(
                        OperationErrorCode::BackupFailed,
                        &error,
                    )),
                }
            }

            OperationRequest::VerifyBackup => backup::backup_verify(&self.runtime)
                .map(OperationResult::BackupVerified)
                .map_err(|error| {
                    OperationFailure::classify(OperationErrorCode::VerificationFailed, &error)
                }),

            OperationRequest::BackupInit => backup::backup_init(&self.runtime)
                .map(OperationResult::BackupInitialized)
                .map_err(|error| {
                    OperationFailure::classify(OperationErrorCode::BackupFailed, &error)
                }),

            OperationRequest::Doctor => doctor::doctor(&self.runtime)
                .map(OperationResult::Diagnosed)
                .map_err(|error| {
                    OperationFailure::classify(OperationErrorCode::DiagnosisFailed, &error)
                }),

            OperationRequest::ReviewDecision(decision) => {
                let request = match decision {
                    ReviewOperation::Approve(subject) => ReviewRequest::Approve(subject.clone()),
                    ReviewOperation::Reject(subject) => ReviewRequest::Reject(subject.clone()),
                };
                review::review(&self.runtime, &request)
                    .map(|outcome| OperationResult::ReviewResolved(Box::new(outcome)))
                    .map_err(|error| {
                        OperationFailure::classify(OperationErrorCode::InvalidRequest, &error)
                    })
            }
        }
    }
}
