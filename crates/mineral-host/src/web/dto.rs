//! The wire vocabulary, and the mapping into it.
//!
//! Nothing internal is serialised directly. `PublishRun`, `ReviewRun`,
//! `OperationSnapshot` and friends are implementation; the moment a client
//! depends on their shape, every internal rename becomes a breaking API change.
//! So each response here is written out by hand, and this module is the only
//! place that reads internals to fill one in.
//!
//! Two consequences that are deliberate:
//!
//! * A field's absence is meaningful. Optional groups are skipped rather than
//!   emitted as `null`, so a client can tell "no backup yet" from "backup
//!   reported nothing".
//! * **No secret can appear.** There is no field for one, on any response, so
//!   redaction is a property of the types rather than a rule a handler has to
//!   remember.

use serde::Serialize;

use crate::{
    application::{
        backup::{BackupInitOutcome, BackupResult, BackupStatusOutcome, BackupVerifyOutcome},
        doctor::DoctorOutcome,
        publish::PublishOutcome,
        review::{PendingReview, ReviewDetail, ReviewOutcome},
        status::StatusOutcome,
    },
    operations::{
        OperationFailure, OperationKind, OperationResult, OperationSnapshot, OperationState,
        ProgressEvent, ProgressKind,
    },
    workflow::PublicationApplicationOutcome,
};

/// `GET /api/v1/status`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebStatusResponse {
    /// Always true: a server that answered at all read a configuration.
    pub configured: bool,
    pub source: WebSource,
    pub state_path: String,
    pub publication: WebPublicationTarget,
    pub reviews: WebReviewCounts,
    pub backup: WebBackupStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebSource {
    pub id: String,
    /// `local` or `r2`.
    pub kind: String,
    /// Where the source comes from, for a human.
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebPublicationTarget {
    pub remote: String,
    pub reference: String,
    /// The newest durable publication run, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebReviewCounts {
    pub pending_markdown: usize,
    pub pending_assets: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebBackupStatus {
    /// Whether this workspace has an enabled backup target.
    ///
    /// Presence only. Nothing about the endpoint's credentials is reported, and
    /// there is no field that could carry one.
    pub configured: bool,
}

/// `GET /api/v1/doctor`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebDoctorResponse {
    pub ok: bool,
    pub passed: usize,
    pub failed: usize,
    pub checks: Vec<WebDoctorCheck>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebDoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// `GET /api/v1/reviews`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebReviewListResponse {
    pub markdown: Vec<WebReviewSummary>,
    pub assets: Vec<WebReviewSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebReviewSummary {
    /// The attempt, for example `document:42`.
    pub attempt: String,
    pub path: String,
}

/// `GET /api/v1/reviews/:attempt`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebReviewDetailResponse {
    pub attempt: String,
    /// `markdown` or `asset`.
    pub subject: String,
    pub path: String,
    pub sha256: String,
    pub decision: WebDecision,
    pub policy: WebPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_codes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `resolved` or `pending`.
    pub human_resolution: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebDecision {
    /// A stable code a client switches on.
    pub code: String,
    /// The decision as the domain renders it, for a human.
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebPolicy {
    pub name: String,
    pub version: String,
    pub hash: String,
}

/// The answer to a request that started an operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebAcceptedOperation {
    pub operation_id: String,
    pub kind: String,
    /// Always `queued` or `running` when this is returned.
    pub state: String,
}

/// `GET /api/v1/operations` and the list inside a snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebOperationSummary {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    /// The failure's stable code, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

/// `GET /api/v1/operations/:id`.
///
/// This is the authoritative state; an SSE stream is a notification of change,
/// never the record itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebOperationResponse {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub queued_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub progress: Vec<WebProgressEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<WebOperationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<WebOperationFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebProgressEvent {
    pub sequence: u64,
    /// `stage` or `detail`.
    pub kind: String,
    pub message: String,
}

/// A failure, as the wire sees it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebOperationFailure {
    /// The stable code. `message` may be reworded; this may not.
    pub code: String,
    /// For a human.
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    /// The one failure a caller acts on with its own data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_base_commit: Option<WebNoBaseCommit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebNoBaseCommit {
    pub remote: String,
    pub reference: String,
    pub snapshot_id: String,
    pub files: usize,
}

/// What an operation produced, tagged by kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebOperationResult {
    /// `publish`, `backup`, `verify_backup`, `backup_init`, `doctor` or
    /// `review_decision`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish: Option<WebPublishResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<WebBackupResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<WebVerifyResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_init: Option<WebBackupInitResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doctor: Option<WebDoctorResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<WebReviewResolution>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebPublishResult {
    /// `published`, `noop`, `incomplete`, `conflict`, `indeterminate`,
    /// `target_missing`, `not_published` or `waiting_for_human_review`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    pub snapshot_id: String,
    pub files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub markdown: usize,
    pub assets: usize,
    pub public_scope: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebBackupResult {
    /// `not_configured`, `backed_up`, `already_backed_up` or `remote_changed`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lfs_objects: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The remote and ref, with no endpoint credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebVerifyResult {
    /// `not_configured` or `verified`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lfs_objects: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebBackupInitResult {
    /// `not_configured`, `already_present` or `created`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebReviewResolution {
    pub attempt: String,
    /// `approve` or `reject`.
    pub decision: String,
    /// True when the same decision was already durable, so nothing changed.
    pub already: bool,
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

/// Milliseconds since the Unix epoch, or zero for an instant before it.
fn millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn optional_millis(time: Option<std::time::SystemTime>) -> Option<u64> {
    time.map(millis)
}

impl WebStatusResponse {
    pub fn from_status(outcome: &StatusOutcome, kind: &str, backup_configured: bool) -> Self {
        Self {
            configured: true,
            source: WebSource {
                id: outcome.source_id.clone(),
                kind: kind.to_owned(),
                description: outcome.source.clone(),
            },
            state_path: outcome.state_path.display().to_string(),
            publication: WebPublicationTarget {
                remote: outcome.target_remote.clone(),
                reference: outcome.target_reference.clone(),
                last_run: outcome.last_publication.clone(),
            },
            reviews: WebReviewCounts {
                pending_markdown: outcome.pending_documents,
                pending_assets: outcome.pending_assets,
            },
            backup: WebBackupStatus {
                configured: backup_configured,
            },
        }
    }
}

impl From<&DoctorOutcome> for WebDoctorResponse {
    fn from(outcome: &DoctorOutcome) -> Self {
        Self {
            ok: !outcome.failed(),
            passed: outcome.passed(),
            failed: outcome.checks.iter().filter(|check| !check.ok).count(),
            checks: outcome
                .checks
                .iter()
                .map(|check| WebDoctorCheck {
                    name: check.name.clone(),
                    ok: check.ok,
                    detail: check.detail.clone(),
                })
                .collect(),
        }
    }
}

fn summaries(pending: &[PendingReview]) -> Vec<WebReviewSummary> {
    pending
        .iter()
        .map(|pending| WebReviewSummary {
            attempt: pending.subject.clone(),
            path: pending.content_path.clone(),
        })
        .collect()
}

impl WebReviewListResponse {
    pub fn from_outcome(outcome: &ReviewOutcome) -> Self {
        match outcome {
            ReviewOutcome::List { documents, assets } => Self {
                markdown: summaries(documents),
                assets: summaries(assets),
            },
            // A list request can only produce a list; an empty answer is more
            // honest than a panic.
            _ => Self {
                markdown: Vec::new(),
                assets: Vec::new(),
            },
        }
    }
}

impl From<&ReviewDetail> for WebReviewDetailResponse {
    fn from(detail: &ReviewDetail) -> Self {
        Self {
            attempt: detail.subject.clone(),
            subject: detail.kind.to_ascii_lowercase(),
            path: detail.content_path.clone(),
            sha256: detail.content_sha256.clone(),
            decision: WebDecision {
                code: detail.decision.code.to_owned(),
                detail: detail.decision.detail.clone(),
            },
            policy: WebPolicy {
                name: detail.policy_name.clone(),
                version: detail.policy_version.clone(),
                hash: detail.policy_hash.clone(),
            },
            reason_codes: detail
                .reason_codes
                .as_ref()
                .map(|reasons| reasons.codes.clone()),
            summary: detail.summary.clone(),
            human_resolution: if detail.human_resolution {
                "resolved".to_owned()
            } else {
                "pending".to_owned()
            },
        }
    }
}

impl From<&ReviewOutcome> for WebReviewResolution {
    fn from(outcome: &ReviewOutcome) -> Self {
        match outcome {
            ReviewOutcome::Resolved {
                subject,
                decision,
                already,
            } => Self {
                attempt: subject.clone(),
                decision: match decision {
                    crate::workflow::HumanReviewDecision::Approve => "approve".to_owned(),
                    crate::workflow::HumanReviewDecision::Reject => "reject".to_owned(),
                },
                already: *already,
            },
            _ => Self {
                attempt: String::new(),
                decision: String::new(),
                already: false,
            },
        }
    }
}

impl From<&OperationKind> for String {
    fn from(kind: &OperationKind) -> Self {
        kind.to_string()
    }
}

impl WebAcceptedOperation {
    pub fn new(
        id: crate::operations::OperationId,
        kind: &OperationKind,
        state: OperationState,
    ) -> Self {
        Self {
            operation_id: id.to_string(),
            kind: kind.to_string(),
            state: state.to_string(),
        }
    }
}

impl WebOperationSummary {
    pub fn from_snapshot(snapshot: &OperationSnapshot) -> Self {
        Self {
            id: snapshot.id.to_string(),
            kind: snapshot.kind.to_string(),
            state: snapshot.state.to_string(),
            started_at_ms: optional_millis(snapshot.started_at),
            finished_at_ms: optional_millis(snapshot.finished_at),
            failure_code: snapshot
                .failure
                .as_ref()
                .map(|failure| failure.code().as_str().to_owned()),
        }
    }
}

impl WebProgressEvent {
    pub fn from_event(event: &ProgressEvent) -> Self {
        Self {
            sequence: event.sequence,
            kind: match event.kind {
                ProgressKind::Stage => "stage".to_owned(),
                ProgressKind::Detail => "detail".to_owned(),
            },
            message: event.message.clone(),
        }
    }
}

impl WebOperationFailure {
    pub fn from_failure(failure: &OperationFailure) -> Self {
        Self {
            code: failure.code().as_str().to_owned(),
            message: failure.message().to_owned(),
            causes: failure.causes().to_vec(),
            no_base_commit: failure
                .no_base_commit()
                .map(|(target, snapshot_id, files)| WebNoBaseCommit {
                    remote: target.remote_name().to_owned(),
                    reference: target.destination_ref().to_owned(),
                    snapshot_id: snapshot_id.get().to_string(),
                    files,
                }),
        }
    }
}

impl WebOperationResponse {
    pub fn from_snapshot(snapshot: &OperationSnapshot) -> Self {
        Self {
            id: snapshot.id.to_string(),
            kind: snapshot.kind.to_string(),
            state: snapshot.state.to_string(),
            queued_at_ms: millis(snapshot.queued_at),
            started_at_ms: optional_millis(snapshot.started_at),
            finished_at_ms: optional_millis(snapshot.finished_at),
            progress: snapshot
                .progress
                .iter()
                .map(WebProgressEvent::from_event)
                .collect(),
            result: snapshot
                .result
                .as_ref()
                .map(|result| WebOperationResult::from_result(result)),
            failure: snapshot
                .failure
                .as_ref()
                .map(WebOperationFailure::from_failure),
        }
    }
}

impl WebOperationResult {
    pub fn from_result(result: &OperationResult) -> Self {
        match result {
            OperationResult::Published(outcome) => Self {
                kind: "publish".to_owned(),
                publish: Some(publish_result(outcome)),
                backup: None,
                verify: None,
                backup_init: None,
                doctor: None,
                review: None,
            },
            OperationResult::BackedUp(result) => Self {
                kind: "backup".to_owned(),
                publish: None,
                backup: Some(backup_result(result)),
                verify: None,
                backup_init: None,
                doctor: None,
                review: None,
            },
            OperationResult::BackupVerified(outcome) => Self {
                kind: "verify_backup".to_owned(),
                publish: None,
                backup: None,
                verify: Some(verify_result(outcome)),
                backup_init: None,
                doctor: None,
                review: None,
            },
            OperationResult::BackupInitialized(outcome) => Self {
                kind: "backup_init".to_owned(),
                publish: None,
                backup: None,
                verify: None,
                backup_init: Some(backup_init_result(outcome)),
                doctor: None,
                review: None,
            },
            OperationResult::Diagnosed(outcome) => Self {
                kind: "doctor".to_owned(),
                publish: None,
                backup: None,
                verify: None,
                backup_init: None,
                doctor: Some(WebDoctorResponse::from(outcome)),
                review: None,
            },
            OperationResult::ReviewResolved(outcome) => Self {
                kind: "review_decision".to_owned(),
                publish: None,
                backup: None,
                verify: None,
                backup_init: None,
                doctor: None,
                review: Some(WebReviewResolution::from(outcome.as_ref())),
            },
        }
    }
}

fn publish_result(outcome: &PublishOutcome) -> WebPublishResult {
    let (snapshot_id, files, markdown, assets) = match &outcome.outcome {
        PublicationApplicationOutcome::NeedsHumanReview { trace } => (
            trace.snapshot().id().get().to_string(),
            trace.snapshot().files().len(),
            trace.markdown_reviews().document_outcomes().len(),
            trace.asset_reviews().entries().len(),
        ),
        PublicationApplicationOutcome::Completed { trace, completed } => (
            trace.snapshot().id().get().to_string(),
            trace.snapshot().files().len(),
            completed.publication_set().markdown_paths().len(),
            completed.publication_set().asset_paths().len(),
        ),
    };
    let run_id = match &outcome.outcome {
        PublicationApplicationOutcome::Completed { completed, .. } => {
            Some(completed.publication().publish_run_id().get().to_string())
        }
        PublicationApplicationOutcome::NeedsHumanReview { .. } => None,
    };
    let public_scope = match &outcome.outcome {
        PublicationApplicationOutcome::NeedsHumanReview { trace }
        | PublicationApplicationOutcome::Completed { trace, .. } => {
            trace.public_scope().to_string()
        }
    };
    WebPublishResult {
        status: outcome.status_code().to_owned(),
        git: outcome.git_code().map(str::to_owned),
        snapshot_id,
        files,
        run_id,
        markdown,
        assets,
        public_scope,
    }
}

fn backup_result(result: &BackupResult) -> WebBackupResult {
    match result {
        BackupResult::NotConfigured => WebBackupResult {
            status: "not_configured".to_owned(),
            snapshot_id: None,
            files: None,
            lfs_objects: None,
            run_id: None,
            remote: None,
            reference: None,
        },
        BackupResult::Attempted(outcome) => WebBackupResult {
            status: match &outcome.execution {
                mineral_core::backup::BackupExecutionOutcome::BackedUp { .. } => "backed_up",
                mineral_core::backup::BackupExecutionOutcome::AlreadyBackedUp { .. } => {
                    "already_backed_up"
                }
                mineral_core::backup::BackupExecutionOutcome::RemoteChanged { .. } => {
                    "remote_changed"
                }
            }
            .to_owned(),
            snapshot_id: Some(outcome.snapshot_id.get().to_string()),
            files: Some(outcome.files),
            lfs_objects: Some(outcome.lfs_objects),
            run_id: outcome.run_id.map(|id| id.get().to_string()),
            remote: Some(outcome.target.remote_name().to_owned()),
            reference: Some(outcome.target.destination_ref().to_owned()),
        },
    }
}

fn verify_result(outcome: &BackupVerifyOutcome) -> WebVerifyResult {
    match outcome {
        BackupVerifyOutcome::NotConfigured => WebVerifyResult {
            status: "not_configured".to_owned(),
            commit: None,
            files: None,
            lfs_objects: None,
        },
        BackupVerifyOutcome::Verified {
            commit,
            files,
            lfs_objects,
        } => WebVerifyResult {
            status: "verified".to_owned(),
            commit: Some(commit.clone()),
            files: Some(*files),
            lfs_objects: Some(*lfs_objects),
        },
    }
}

fn backup_init_result(outcome: &BackupInitOutcome) -> WebBackupInitResult {
    let (status, commit, remote, reference) = match outcome {
        BackupInitOutcome::NotConfigured => ("not_configured", None, None, None),
        BackupInitOutcome::AlreadyPresent { target, commit_oid } => (
            "already_present",
            Some(commit_oid.clone()),
            Some(target.remote_name().to_owned()),
            Some(target.destination_ref().to_owned()),
        ),
        BackupInitOutcome::Created { target, commit_oid } => (
            "created",
            Some(commit_oid.clone()),
            Some(target.remote_name().to_owned()),
            Some(target.destination_ref().to_owned()),
        ),
    };
    WebBackupInitResult {
        status: status.to_owned(),
        commit,
        remote,
        reference,
    }
}

/// The backup read-only report, for anything that wants it directly.
impl WebBackupStatus {
    pub fn from_outcome(outcome: &BackupStatusOutcome) -> Self {
        Self {
            configured: matches!(outcome, BackupStatusOutcome::Reported { .. }),
        }
    }
}
