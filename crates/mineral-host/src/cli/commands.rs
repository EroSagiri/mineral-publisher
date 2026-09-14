//! One function per command: build the request, call the use case, render.
//!
//! A command here is deliberately dull. It resolves the workspace, hands the
//! application layer a request, and passes the outcome to [`super::output`].
//! Anything that looks like a decision belongs in the layer below.

use std::{error::Error, fs, path::Path, sync::Arc};

use mineral_publisher::{
    application::{
        backup::{self, BackupError, BackupRequest, BackupResult},
        doctor,
        publish::{self, PublishRequest},
        review::{self, ReviewRequest},
        status,
    },
    config::{ConfigFormat, SourceType},
    runtime::{Progress, StderrProgress, WorkspaceRuntime},
};

use super::output;
use super::output::emit;

/// A sink that streams a use case's progress to standard error.
fn progress() -> Arc<dyn Progress> {
    Arc::new(StderrProgress)
}

/// Creates a fresh workspace in the language its name promises.
pub fn init(path: &Path, format: ConfigFormat) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        return Err(format!("configuration already exists: {}", path.display()).into());
    }
    // The extension is what chooses the syntax when the file is read back, so a
    // file whose name promises a language it is not written in is refused here
    // rather than discovered by a parser later.
    if ConfigFormat::of(path) != Some(format) {
        return Err(format!(
            "a {} configuration must be named with a .{} extension, not {}",
            format.extension(),
            format.extension(),
            path.display()
        )
        .into());
    }
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format.template())?;
    let workspace = WorkspaceRuntime::load(path.to_path_buf())?;
    if workspace.source_kind() == SourceType::Local {
        fs::create_dir_all(workspace.local_source_path()?)?;
    }
    workspace.prepare()?;
    workspace.open_stores()?;
    emit!(
        "Initialized Mineral workspace\n  config: {}\n  state: {}\n  source: {}",
        workspace.config_path.display(),
        workspace.config.state.path.display(),
        workspace.source_description()
    );
    Ok(())
}

/// Publishes the current source state.
pub fn publish(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    let outcome = publish::publish(&workspace, PublishRequest::now(), &progress())?;
    output::publication(&outcome);
    Ok(())
}

/// Reports the workspace's state.
pub fn status(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    let outcome = status::status(&workspace)?;
    output::status(&outcome);
    Ok(())
}

/// Scans the workspace for health, failing when a check failed.
pub fn doctor(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    let outcome = doctor::doctor(&workspace)?;
    output::doctor(&outcome);
    if outcome.failed() {
        return Err("one or more doctor checks failed".into());
    }
    Ok(())
}

/// Inspects or decides the human review queue.
pub fn review(workspace: WorkspaceRuntime, args: &[String]) -> Result<(), Box<dyn Error>> {
    let request = match args.first().map(String::as_str) {
        Some("list") => ReviewRequest::List,
        Some("show") => ReviewRequest::Show(required(args, 1, "review show requires an ID")?),
        Some("approve") => {
            ReviewRequest::Approve(required(args, 1, "review approve requires an ID")?)
        }
        Some("reject") => ReviewRequest::Reject(required(args, 1, "review reject requires an ID")?),
        _ => return Err("review requires list, show, approve, or reject".into()),
    };
    let outcome = review::review(&workspace, &request)?;
    output::review(&outcome);
    Ok(())
}

/// Runs or inspects the private backup.
pub fn backup(workspace: WorkspaceRuntime, args: &[String]) -> Result<(), Box<dyn Error>> {
    match args.first().map(String::as_str) {
        None => backup_run(workspace),
        Some("status") => backup_status(workspace),
        Some("verify") => backup_verify(workspace),
        Some("init") => backup_init(workspace),
        Some(other) => Err(format!("unknown backup command: {other}").into()),
    }
}

/// Runs one backup of a fresh Snapshot.
///
/// The Snapshot comes from the same use case `publish` uses, so a local
/// directory and an R2 prefix both back up. Nothing remote happens until the
/// intent is durable, and the report names the run, the Snapshot it froze and
/// the ref it moved.
fn backup_run(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    let progress = progress();
    let outcome = match backup::backup(&workspace, BackupRequest::now()?, progress.as_ref()) {
        Ok(outcome) => outcome,
        // A missing ref is one operator action away from a backup, so it gets the
        // same short report as the other statuses before failing closed.
        Err(error @ BackupError::NoBaseCommit { .. }) => {
            if let BackupError::NoBaseCommit {
                target,
                snapshot_id,
                files,
            } = &error
            {
                output::backup_missing_ref(target, *snapshot_id, *files);
            }
            return Err(Box::new(error));
        }
        Err(error) => return Err(Box::new(error)),
    };
    match outcome {
        BackupResult::NotConfigured => output::backup_not_configured("nothing to do"),
        BackupResult::Attempted(outcome) => output::backup_outcome(&outcome),
    }
    Ok(())
}

/// Reports the observed backup ref and the newest durable intent.
fn backup_status(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    output::backup_status(&backup::backup_status(&workspace)?);
    Ok(())
}

/// Verifies that the backup ref can be restored byte-for-byte.
fn backup_verify(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    output::backup_verify(&backup::backup_verify(&workspace)?);
    Ok(())
}

/// Bootstraps the backup ref with one empty root commit.
fn backup_init(workspace: WorkspaceRuntime) -> Result<(), Box<dyn Error>> {
    // The ref is only named for a workspace that has one, and only when a report
    // needs it: a workspace without a backup target must not fail on this.
    let target = workspace.backup_target();
    let outcome = backup::backup_init(&workspace)?;
    match &outcome {
        mineral_publisher::application::backup::BackupInitOutcome::NotConfigured => {
            output::backup_not_configured("nothing to initialize")
        }
        _ => output::backup_init(&outcome, &target?),
    }
    Ok(())
}

/// The positional argument a subcommand requires.
fn required(args: &[String], index: usize, message: &str) -> Result<String, Box<dyn Error>> {
    Ok(args.get(index).ok_or(message)?.to_owned())
}
