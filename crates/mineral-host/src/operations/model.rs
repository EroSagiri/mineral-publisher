//! What an operation is, and what it looks like from outside.
//!
//! These types are the whole vocabulary of the operation API. They are
//! deliberately transport-neutral: nothing here knows about HTTP, JSON, SSE or
//! a terminal. A CLI renders a snapshot as text, and the Web adapter planned as
//! S8.2 will serialise the same value — the vocabulary does not change for
//! either.

use std::{error::Error, fmt, sync::Arc, time::SystemTime};

use crate::{
    application::{
        ApplicationError,
        backup::{BackupInitOutcome, BackupRequest, BackupResult, BackupVerifyOutcome},
        doctor::DoctorOutcome,
        publish::{PublishOutcome, PublishRequest},
        review::ReviewOutcome,
    },
    domain::SnapshotId,
    publisher::GitRefTarget,
    runtime::RuntimeError,
    workflow::HumanReviewDecision,
};

/// Stable identity of one operation, for as long as its supervisor lives.
///
/// Identities are allocated per supervisor and are not durable: an operation is
/// in-process work, and a restart is a new supervisor with a new address space.
/// A caller that needs to correlate across restarts correlates on the durable
/// records the *engine* writes (a publish run, a backup run), not on this.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationId(u64);

impl OperationId {
    /// Allocates the first identity. The supervisor hands out the rest.
    pub(crate) fn first() -> Self {
        Self(1)
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Rebuilds an identity from its number, for a caller that read one off the
    /// wire.
    ///
    /// This is not a way to invent an operation: the supervisor only answers for
    /// the identities it allocated, so an unknown number resolves to nothing
    /// rather than to a different operation.
    pub fn from_number(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "op-{}", self.0)
    }
}

/// What an operation does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationKind {
    /// Read the source, judge it, deliver it, record the run.
    Publish,
    /// Freeze a Snapshot and deliver it to the private backup.
    Backup,
    /// Read the backup ref back and prove it restores byte-for-byte.
    VerifyBackup,
    /// Create the empty root commit a first backup builds on.
    BackupInit,
    /// Scan the workspace for health.
    Doctor,
    /// Record an immutable human decision about one review attempt.
    ReviewDecision {
        subject: String,
        decision: HumanReviewDecision,
    },
}

impl OperationKind {
    /// Whether this operation changes something a reader could observe.
    ///
    /// Mutating operations are mutually exclusive within one workspace: they
    /// share a Git worktree, a set of SQLite databases, an object store and a
    /// remote ref, and two of them at once would be a race the engine is not
    /// asked to survive. Read-only operations may run alongside them, because a
    /// user watching a publication needs `status` to answer *during* it.
    pub fn is_mutating(&self) -> bool {
        match self {
            Self::Publish | Self::Backup | Self::BackupInit | Self::ReviewDecision { .. } => true,
            Self::VerifyBackup | Self::Doctor => false,
        }
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Publish => "publish",
            Self::Backup => "backup",
            Self::VerifyBackup => "verify_backup",
            Self::BackupInit => "backup_init",
            Self::Doctor => "doctor",
            Self::ReviewDecision { .. } => "review_decision",
        };
        formatter.write_str(name)
    }
}

/// Where an operation is in its life.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationState {
    /// Accepted and not yet started.
    Queued,
    /// Running now.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Finished with a typed failure.
    Failed,
}

impl OperationState {
    /// Whether no further change is possible.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

impl fmt::Display for OperationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        })
    }
}

/// A human decision, which is the only review work that mutates.
///
/// Listing and showing are reads: they never reach the supervisor, and this
/// type is what makes "an operation cannot be a read" a fact rather than a
/// convention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewOperation {
    Approve(String),
    Reject(String),
}

impl ReviewOperation {
    pub fn subject(&self) -> &str {
        match self {
            Self::Approve(subject) | Self::Reject(subject) => subject,
        }
    }

    pub fn decision(&self) -> HumanReviewDecision {
        match self {
            Self::Approve(_) => HumanReviewDecision::Approve,
            Self::Reject(_) => HumanReviewDecision::Reject,
        }
    }
}

/// What a caller asks for.
#[derive(Clone, Debug)]
pub enum OperationRequest {
    Publish(PublishRequest),
    Backup(BackupRequest),
    VerifyBackup,
    BackupInit,
    Doctor,
    ReviewDecision(ReviewOperation),
}

impl OperationRequest {
    /// What this request will do.
    pub fn kind(&self) -> OperationKind {
        match self {
            Self::Publish(_) => OperationKind::Publish,
            Self::Backup(_) => OperationKind::Backup,
            Self::VerifyBackup => OperationKind::VerifyBackup,
            Self::BackupInit => OperationKind::BackupInit,
            Self::Doctor => OperationKind::Doctor,
            Self::ReviewDecision(decision) => OperationKind::ReviewDecision {
                subject: decision.subject().to_owned(),
                decision: decision.decision(),
            },
        }
    }
}

/// Which progress line this is.
///
/// The distinction already existed at the call sites (`stage` versus `detail`);
/// naming it makes it usable by a renderer that wants to show phases
/// differently from the lines inside them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressKind {
    /// A phase boundary.
    Stage,
    /// One item of work inside a phase.
    Detail,
}

/// One progress line, in the order the use case produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressEvent {
    /// Monotonic within one operation, starting at 1.
    pub sequence: u64,
    /// Whether this is a phase boundary or a line inside one.
    pub kind: ProgressKind,
    /// What the use case reported, verbatim.
    pub message: String,
}

/// What a subscriber receives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationEvent {
    /// One progress line.
    Progress(ProgressEvent),
    /// The operation reached a terminal state; nothing more will arrive.
    Finished { state: OperationState },
}

/// What an operation produced, in the use case's own terms.
#[derive(Debug)]
pub enum OperationResult {
    Published(Box<PublishOutcome>),
    BackedUp(BackupResult),
    BackupVerified(BackupVerifyOutcome),
    BackupInitialized(BackupInitOutcome),
    Diagnosed(DoctorOutcome),
    ReviewResolved(Box<ReviewOutcome>),
}

/// A stable, machine-readable reason an operation failed.
///
/// This is the interface a Web UI switches on. `message` is for a human and may
/// be reworded at any time; a code may not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationErrorCode {
    /// Another mutating operation holds the workspace.
    WorkspaceBusy,
    /// This workspace is not configured for the requested work.
    WorkspaceNotConfigured,
    /// The configuration could not be read, parsed or validated.
    ConfigurationInvalid,
    /// A credential the configuration names is not available.
    CredentialMissing,
    /// An endpoint could not be built or reached.
    ConnectionFailed,
    /// The request itself is unusable.
    InvalidRequest,
    /// The named review attempt does not exist.
    ReviewNotFound,
    /// The subject is in a state that forbids the change.
    ReviewConflict,
    /// The backup ref does not exist yet.
    BackupNoBaseCommit,
    /// The publication workflow failed.
    PublicationFailed,
    /// The backup workflow failed.
    BackupFailed,
    /// Reading the backup ref back failed.
    VerificationFailed,
    /// The health scan could not run.
    DiagnosisFailed,
    /// A use case panicked. The workspace is left as the use case left it.
    OperationPanicked,
    /// No such endpoint exists.
    EndpointNotFound,
    /// No operation with that identity is known to this supervisor.
    ///
    /// It means the identity was never allocated, has been evicted by retention,
    /// or belonged to a previous process. A client must not guess: it should
    /// read the durable state instead.
    OperationNotFound,
}

impl OperationErrorCode {
    /// The stable wire name. This is the value a client matches on.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceBusy => "workspace_busy",
            Self::WorkspaceNotConfigured => "workspace_not_configured",
            Self::ConfigurationInvalid => "configuration_invalid",
            Self::CredentialMissing => "credential_missing",
            Self::ConnectionFailed => "connection_failed",
            Self::InvalidRequest => "invalid_request",
            Self::ReviewNotFound => "review_not_found",
            Self::ReviewConflict => "review_conflict",
            Self::BackupNoBaseCommit => "backup_no_base_commit",
            Self::PublicationFailed => "publication_failed",
            Self::BackupFailed => "backup_failed",
            Self::VerificationFailed => "verification_failed",
            Self::DiagnosisFailed => "diagnosis_failed",
            Self::OperationPanicked => "operation_panicked",
            Self::OperationNotFound => "operation_not_found",
            Self::EndpointNotFound => "endpoint_not_found",
        }
    }
}

impl OperationErrorCode {
    /// The code one application failure maps to, given the code its use case
    /// implies.
    ///
    /// The default comes from *which* use case ran; the refinement comes from
    /// the error's own variant. This is the single place the application layer's
    /// error vocabulary meets the public one, so the CLI and the HTTP API cannot
    /// classify the same failure differently.
    pub fn classify(default: Self, error: &ApplicationError) -> Self {
        match error {
            ApplicationError::Runtime(RuntimeError::Configuration(_)) => Self::ConfigurationInvalid,
            ApplicationError::Runtime(RuntimeError::Credential { .. }) => Self::CredentialMissing,
            ApplicationError::Runtime(RuntimeError::Connection { .. }) => Self::ConnectionFailed,
            ApplicationError::Unsupported { .. } => Self::InvalidRequest,
            ApplicationError::NotFound { .. } => Self::ReviewNotFound,
            ApplicationError::Conflict { .. } => Self::ReviewConflict,
            ApplicationError::Operation { .. } => default,
        }
    }
}

impl fmt::Display for OperationErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why an operation failed, as data.
///
/// It is `Send` on purpose: the supervisor runs work on another thread, and a
/// failure that could not cross that boundary would have to become a string.
/// The chain is kept, because "the store failed" is only actionable together
/// with what the store said.
#[derive(Clone, Debug)]
pub enum OperationFailure {
    /// A use case failed, classified.
    Application {
        code: OperationErrorCode,
        message: String,
        causes: Vec<String>,
    },
    /// The backup ref does not exist yet, so there is no base to build on.
    ///
    /// This is the one failure a caller acts on with its own data: creating the
    /// ref is one command away, and the report names what the attempt froze.
    BackupNoBaseCommit {
        target: GitRefTarget,
        snapshot_id: SnapshotId,
        files: usize,
        message: String,
    },
}

impl OperationFailure {
    /// The stable reason.
    pub fn code(&self) -> OperationErrorCode {
        match self {
            Self::Application { code, .. } => *code,
            Self::BackupNoBaseCommit { .. } => OperationErrorCode::BackupNoBaseCommit,
        }
    }

    /// The human-readable reason.
    pub fn message(&self) -> &str {
        match self {
            Self::Application { message, .. } => message,
            Self::BackupNoBaseCommit { message, .. } => message,
        }
    }

    /// The causes, outermost first, when the failure has a chain.
    pub fn causes(&self) -> &[String] {
        match self {
            Self::Application { causes, .. } => causes,
            Self::BackupNoBaseCommit { .. } => &[],
        }
    }

    /// The target and frozen Snapshot, for the one failure that carries them.
    pub fn no_base_commit(&self) -> Option<(&GitRefTarget, SnapshotId, usize)> {
        match self {
            Self::BackupNoBaseCommit {
                target,
                snapshot_id,
                files,
                ..
            } => Some((target, *snapshot_id, *files)),
            Self::Application { .. } => None,
        }
    }

    /// Classifies an application failure under the code its use case implies.
    ///
    /// The default comes from *which* use case ran; the refinement comes from
    /// the error's own variant. That is the explicit mapping from the
    /// application layer's error type to a stable public code, and it is the
    /// only place the two vocabularies meet.
    pub fn classify(default: OperationErrorCode, error: &ApplicationError) -> Self {
        let code = OperationErrorCode::classify(default, error);
        let mut causes = Vec::new();
        let mut source = error.source();
        while let Some(cause) = source {
            causes.push(cause.to_string());
            source = cause.source();
        }
        Self::Application {
            code,
            message: error.to_string(),
            causes,
        }
    }

    /// The failure as an error value, with its chain intact.
    ///
    /// A caller that reports errors the way Rust does — `Display` plus
    /// `source()` — gets the same text an in-process failure would have given,
    /// which is what lets the CLI keep its output while running work on another
    /// thread.
    pub fn to_error(&self) -> OperationFailureError {
        let mut chain: Option<Box<Cause>> = None;
        for cause in self.causes().iter().rev() {
            chain = Some(Box::new(Cause {
                message: cause.clone(),
                source: chain,
            }));
        }
        OperationFailureError {
            message: self.message().to_owned(),
            source: chain,
        }
    }
}

/// One link of a stored cause chain.
#[derive(Debug)]
struct Cause {
    message: String,
    source: Option<Box<Cause>>,
}

impl fmt::Display for Cause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for Cause {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|cause| cause as &(dyn Error + 'static))
    }
}

/// A failure rendered as an error with its chain restored.
#[derive(Debug)]
pub struct OperationFailureError {
    message: String,
    source: Option<Box<Cause>>,
}

impl fmt::Display for OperationFailureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for OperationFailureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|cause| cause as &(dyn Error + 'static))
    }
}

/// Everything known about one operation, as one value.
///
/// This is what a Web adapter serialises and what a test asserts on. The result
/// is behind an `Arc` so that reading a snapshot never copies a publication
/// trace.
#[derive(Debug)]
pub struct OperationSnapshot {
    pub id: OperationId,
    pub kind: OperationKind,
    pub state: OperationState,
    pub queued_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
    /// Every progress line so far, in order.
    pub progress: Vec<ProgressEvent>,
    /// The typed result, once it succeeded.
    pub result: Option<Arc<OperationResult>>,
    /// The typed failure, once it failed.
    pub failure: Option<OperationFailure>,
}

impl OperationSnapshot {
    /// Whether the operation reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// The typed result, once one exists.
    pub fn result(&self) -> Option<&Arc<OperationResult>> {
        self.result.as_ref()
    }

    /// The typed failure, once one exists.
    pub fn failure(&self) -> Option<&OperationFailure> {
        self.failure.as_ref()
    }
}

/// Why an operation could not be started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartError {
    /// Another mutating operation holds this workspace.
    WorkspaceBusy { active_operation_id: OperationId },
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceBusy {
                active_operation_id,
            } => write!(
                formatter,
                "workspace is busy with {active_operation_id}; wait for it to finish"
            ),
        }
    }
}

impl Error for StartError {}
