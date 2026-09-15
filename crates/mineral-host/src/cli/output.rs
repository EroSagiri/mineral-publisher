//! Outcomes to text.
//!
//! Every write to standard output in this binary lives in this module. An
//! outcome carries the facts; this is where they become the words an operator
//! reads, and nowhere else has an opinion about formatting.

/// Writes one line to standard output, ignoring a closed pipe.
///
/// `mineral status | head -1` is a normal thing to type. The default `println!`
/// panics when the reader has gone away, which would turn a successful report
/// into a crash; a report that nobody is reading is simply not an error.
macro_rules! emit {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stdout().lock(), $($arg)*);
    }};
}

use mineral_publisher::{
    application::{
        backup::{BackupInitOutcome, BackupOutcome, BackupStatusOutcome, BackupVerifyOutcome},
        doctor::DoctorOutcome,
        publish::PublishOutcome,
        review::ReviewOutcome,
        status::StatusOutcome,
    },
    asset::AssetPublicationOutcome,
    domain::SnapshotId,
    operations::{OperationResult, ProgressEvent},
    publisher::GitRefTarget,
    runtime::WorkspaceRuntime,
    workflow::PublicationApplicationOutcome,
};

/// Streams one progress line.
///
/// Progress goes to standard error, not standard output: it is not the result,
/// and a caller that pipes the report must not have to filter it out.
pub fn progress(event: &ProgressEvent) {
    use ::std::io::Write as _;
    let _ = ::std::writeln!(::std::io::stderr().lock(), "{}", event.message);
}

/// Renders whatever an operation produced.
pub fn operated(result: &OperationResult) {
    match result {
        OperationResult::Published(outcome) => publication(outcome),
        OperationResult::BackedUp(backup) => match backup {
            mineral_publisher::application::backup::BackupResult::NotConfigured => {
                backup_not_configured("nothing to do");
            }
            mineral_publisher::application::backup::BackupResult::Attempted(outcome) => {
                backup_outcome(outcome);
            }
        },
        OperationResult::BackupVerified(outcome) => backup_verify(outcome),
        OperationResult::BackupInitialized(outcome) => backup_init(outcome),
        OperationResult::Diagnosed(outcome) => doctor(outcome),
        OperationResult::ReviewResolved(outcome) => review(outcome),
    }
}

/// The address the local API is listening on, and whether a UI was found.
pub fn serving(address: std::net::SocketAddr, assets: Option<&std::path::Path>) {
    emit!("Mineral Web API listening on http://{address}");
    match assets {
        Some(directory) => emit!("  Web UI: {}", directory.display()),
        None => emit!(
            "  Web UI: not built. The JSON API works; build the UI with\n    \
             npm --prefix web-ui install && npm --prefix web-ui run build"
        ),
    }
}

/// The workspace `init` just created.
pub fn initialized(workspace: &WorkspaceRuntime) {
    emit!(
        "Initialized Mineral workspace\n  config: {}\n  state: {}\n  source: {}",
        workspace.config_path.display(),
        workspace.config.state.path.display(),
        workspace.source_description()
    );
}

/// The report of one publication.
pub fn publication(outcome: &PublishOutcome) {
    match &outcome.outcome {
        PublicationApplicationOutcome::NeedsHumanReview { trace } => {
            emit!(
                "Publication\n  status: waiting_for_human_review\n  snapshot: {}\n  files: {}\n\nMarkdown\n  documents: {}\n  private: {}\n  invalid_privacy: {}\n  needs human review: {}\n\nAssets\n  reviewed: {}\n  needs human review: {}\n\nNext:\n  mineral-publisher review list",
                trace.snapshot().id().get(),
                trace.snapshot().files().len(),
                trace.markdown_reviews().document_outcomes().len(),
                trace.markdown_reviews().private_documents().len(),
                trace.markdown_reviews().invalid_privacy_documents().len(),
                trace.effective_reviews().pending_documents().len(),
                trace.asset_reviews().entries().len(),
                trace.effective_reviews().pending_assets().len()
            );
            emit!("  public scope: {}", trace.public_scope());
            warnings(trace.markdown_reviews());
        }
        PublicationApplicationOutcome::Completed { trace, completed } => {
            let publication = completed.publication();
            let delivery = publication.workflow();
            let status = outcome.status_code();
            let git = outcome.git_code().unwrap_or("not_published");
            let assets = match delivery.assets() {
                AssetPublicationOutcome::NoDurableProjection => "not_required".to_owned(),
                AssetPublicationOutcome::NotAttempted => "not_attempted".to_owned(),
                AssetPublicationOutcome::Satisfied {
                    verified,
                    published,
                } => format!("{verified} verified, {published} published"),
            };
            emit!(
                "Publication\n  status: {status}\n  git: {git}\n  run: {}\n  snapshot: {}\n  projection: {}\n  delivery: {}\n  markdown: {}\n  assets: {}\n  asset delivery: {}\n  asset target: {}",
                publication.publish_run_id().get(),
                trace.snapshot().id().get(),
                completed.projection().projection_sha256(),
                publication.delivery_sha256(),
                completed.publication_set().markdown_paths().len(),
                completed.publication_set().asset_paths().len(),
                assets,
                outcome.asset_location
            );
            emit!(
                "  public scope: {} ({} rule(s))",
                trace.public_scope(),
                outcome.public_scope.len()
            );
            if outcome.is_noop() {
                emit!("  No changes to publish.");
            }
            warnings(trace.markdown_reviews());
        }
    }
}

/// The navigation warnings a policy run collected.
fn warnings(result: &mineral_publisher::workflow::PublicPolicyRunResult) {
    emit!("\nWarnings\n  navigation: {}", result.warnings().len());
    for warning in result.warnings() {
        emit!(
            "  {} {:?} target={} span={:?}",
            warning.document_path(),
            warning.origin().kind(),
            warning.origin().target(),
            warning.origin().span()
        );
    }
}

/// The workspace's state.
pub fn status(outcome: &StatusOutcome) {
    emit!(
        "Mineral status\n  source: {} ({})\n  state: {}\n  publication target: {} {}\n  last publication: {}\n  pending Markdown reviews: {}\n  pending Asset reviews: {}",
        outcome.source_id,
        outcome.source,
        outcome.state_path.display(),
        outcome.target_remote,
        outcome.target_reference,
        outcome
            .last_publication
            .clone()
            .unwrap_or_else(|| "none".to_owned()),
        outcome.pending_documents,
        outcome.pending_assets
    );
}

/// Every health check, pass or fail.
pub fn doctor(outcome: &DoctorOutcome) {
    emit!("Mineral doctor");
    for check in &outcome.checks {
        emit!(
            "  {}: {} ({})",
            check.name,
            if check.ok { "ok" } else { "failed" },
            check.detail
        );
    }
}

/// The review queue, one attempt, or a recorded decision.
pub fn review(outcome: &ReviewOutcome) {
    match outcome {
        ReviewOutcome::List { documents, assets } => {
            emit!("Markdown");
            for pending in documents {
                emit!("  {}  {}", pending.subject, pending.content_path);
            }
            emit!("Assets");
            for pending in assets {
                emit!("  {}  {}", pending.subject, pending.content_path);
            }
        }
        ReviewOutcome::Shown(detail) => {
            emit!(
                "Review\n  id: {}\n  subject: {}\n  path: {}\n  sha256: {}\n  decision: {}\n  contract: {} {} {}",
                detail.subject,
                detail.kind,
                detail.content_path,
                detail.content_sha256,
                detail.decision.detail,
                detail.policy_name,
                detail.policy_version,
                detail.policy_hash
            );
            if let (Some(reasons), Some(summary)) = (&detail.reason_codes, &detail.summary) {
                emit!("  reason_codes: {}\n  summary: {summary}", reasons.detail);
            }
            emit!(
                "  human_resolution: {}",
                if detail.human_resolution {
                    "resolved"
                } else {
                    "pending"
                }
            );
        }
        ReviewOutcome::Resolved {
            decision, already, ..
        } => {
            if *already {
                emit!("Review already {decision:?}.");
            } else {
                emit!("Review {decision:?}.");
            }
        }
    }
}

/// The workspace does not back up.
pub fn backup_not_configured(what: &str) {
    emit!("Backup is not configured for this workspace; {what}.");
}

/// The ref a backup builds on does not exist yet.
pub fn backup_missing_ref(target: &GitRefTarget, snapshot_id: SnapshotId, files: usize) {
    emit!(
        "Backup\n  status: ref missing\n  ref: {} {}\n  snapshot: {}\n  files: {}",
        target.remote_name(),
        target.destination_ref(),
        snapshot_id.get(),
        files
    );
}

/// What one backup attempt did.
pub fn backup_outcome(outcome: &BackupOutcome) {
    let status = match &outcome.execution {
        mineral_core::backup::BackupExecutionOutcome::BackedUp { .. } => "backed up",
        mineral_core::backup::BackupExecutionOutcome::AlreadyBackedUp { .. } => "already backed up",
        mineral_core::backup::BackupExecutionOutcome::RemoteChanged { .. } => "remote changed",
    };
    emit!(
        "Backup\n  status: {status}\n  run: {}\n  snapshot: {}\n  files: {}\n  lfs objects: {}\n  ref: {} {}\n  endpoint: {}",
        outcome
            .run_id
            .map(|id| id.get().to_string())
            .unwrap_or_else(|| "none (already up to date)".to_owned()),
        outcome.snapshot_id.get(),
        outcome.files,
        outcome.lfs_objects,
        outcome.target.remote_name(),
        outcome.target.destination_ref(),
        outcome.endpoint
    );
}

/// The observed backup ref and the newest durable intent.
pub fn backup_status(outcome: &BackupStatusOutcome) {
    emit!("Mineral backup");
    match outcome {
        BackupStatusOutcome::NotConfigured => emit!("  backup: not configured"),
        BackupStatusOutcome::Reported {
            remote,
            reference,
            reference_state,
            newest_run,
        } => {
            emit!("  ref: {remote} {reference} {reference_state}");
            match newest_run {
                Some(run) => emit!(
                    "  newest run: {}\n  snapshot: {}\n  delivery: {}",
                    run.run_id,
                    run.snapshot_id,
                    run.delivery_sha256
                ),
                None => emit!("  newest run: none"),
            }
        }
    }
}

/// The result of verifying the backup ref.
pub fn backup_verify(outcome: &BackupVerifyOutcome) {
    match outcome {
        BackupVerifyOutcome::NotConfigured => {
            backup_not_configured("nothing to verify");
        }
        BackupVerifyOutcome::Verified {
            commit,
            files,
            lfs_objects,
        } => emit!(
            "Backup verify\n  status: verified\n  commit: {commit}\n  files: {files}\n  lfs objects: {lfs_objects}"
        ),
    }
}

/// The result of bootstrapping the backup ref.
pub fn backup_init(outcome: &BackupInitOutcome) {
    match outcome {
        BackupInitOutcome::NotConfigured => backup_not_configured("nothing to initialize"),
        BackupInitOutcome::AlreadyPresent { target, commit_oid } => emit!(
            "Backup ref {} {} already exists at {}; nothing changed.",
            target.remote_name(),
            target.destination_ref(),
            commit_oid
        ),
        BackupInitOutcome::Created { target, commit_oid } => emit!(
            "Initialized backup ref {} {} at {}.",
            target.remote_name(),
            target.destination_ref(),
            commit_oid
        ),
    }
}
