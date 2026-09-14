use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use mineral_core::backup::{
    BackupExecutionOutcome, BackupRunStore, TypeFirstBackupRepresentationPolicy, verify_backup,
};
use mineral_core::source::{DEFAULT_MAX_SCAN_ATTEMPTS, stabilize_scan};
use mineral_publisher::{
    asset::{AssetPublicationOutcome, UuidAssetObservationIdGenerator},
    backup::application::{
        BackupApplicationError, BackupApplicationOutcome, BackupApplicationRequest, run_backup,
    },
    backup::lfs_http::LfsHttpRemote,
    domain::{Snapshot, SnapshotFile, SnapshotId, SourceId, TimestampMillis},
    policy::{PolicyIdentity, ReviewRunId, ReviewRunStore},
    publisher::{
        GitPublicationExecution, GitRefTarget, PublishRunStore, RemoteRefState,
        UuidPublishRunIdGenerator, UuidRemoteObservationIdGenerator,
    },
    reviewer::{ASSET_REVIEWER_PROMPT_VERSION, MARKDOWN_REVIEWER_PROMPT_VERSION},
    runtime::{
        HostAssetReviews, HostMarkdownReviews, WorkspaceRuntime,
        composition::{
            RandomAssetIds, RandomDocumentIds, UuidBackupRunIdGenerator, random_human_id,
            sqlite_positive_id,
        },
    },
    storage::{SqliteAssetReviewRunStore, SqliteHumanReviewStore, SqliteReviewRunStore},
    workflow::{
        AssetReviewRunId, AssetReviewRunStore, ExplicitHumanReviewSelection, HumanReviewAttempt,
        HumanReviewDecision, HumanReviewResolution, PublicExclusionRules, PublicationApplication,
        PublicationApplicationOutcome, PublicationApplicationRequest,
    },
};
use sha2::{Digest, Sha256 as Sha256Hasher};

use mineral_publisher::config::{ConfigFormat, SourceType};

/// The YAML template, which the tests exercise as the shape of a workspace.
#[cfg(test)]
use mineral_publisher::config::DEFAULT_CONFIG;
/// The raw configuration model, under the name this binary and its tests have
/// always used for it. It is only ever held inside a validated configuration.
#[cfg(test)]
use mineral_publisher::config::RawConfig as Config;
/// The runtime this binary drives.
///
/// Every adapter is built inside it, so this module decides *what* to run and
/// never *how* it is connected.
type Workspace = WorkspaceRuntime;

pub fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    let (config_path, configured) = if args.first().is_some_and(|arg| arg == "--config") {
        if args.len() < 2 {
            return Err("--config requires a path".into());
        }
        let value = PathBuf::from(args.remove(1));
        args.remove(0);
        (value, true)
    } else {
        (default_config_path(), false)
    };
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    match command {
        "init" => {
            // A fresh workspace may be written in either language. The flag also
            // decides the name of the file when the operator did not choose one,
            // so `mineral init --toml` cannot silently write YAML to `mineral.toml`.
            let format = if args[1..].iter().any(|arg| arg == "--toml") {
                ConfigFormat::Toml
            } else {
                ConfigFormat::Yaml
            };
            let path = if format == ConfigFormat::Toml && !configured {
                PathBuf::from(format!("mineral.{}", format.extension()))
            } else {
                config_path
            };
            init(&path, format)
        }
        "publish" => publish(Workspace::load(config_path)?),
        "status" => status(Workspace::load(config_path)?),
        "doctor" => doctor(Workspace::load(config_path)?),
        "review" => review(Workspace::load(config_path)?, &args[1..]),
        "backup" => backup(Workspace::load(config_path)?, &args[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => Err(format!("unknown command: {command}").into()),
    }
}

/// The configuration a command uses when the operator names none.
///
/// `mineral.yaml` stays the default, exactly as it always was. A workspace that
/// was created with `mineral init --toml` is found too, so the language a file is
/// written in never has to be repeated on every command line.
fn default_config_path() -> PathBuf {
    let yaml = PathBuf::from("mineral.yaml");
    if !yaml.exists() {
        let toml = PathBuf::from("mineral.toml");
        if toml.exists() {
            return toml;
        }
    }
    yaml
}

fn print_help() {
    println!(
        "Mineral Publisher\n\nUsage:\n  mineral [--config PATH] init [--toml]\n  mineral [--config PATH] publish\n  mineral [--config PATH] status\n  mineral [--config PATH] review list\n  mineral [--config PATH] review show <document:ID|asset:ID>\n  mineral [--config PATH] review approve <document:ID|asset:ID>\n  mineral [--config PATH] review reject <document:ID|asset:ID>\n  mineral [--config PATH] backup\n  mineral [--config PATH] backup status\n  mineral [--config PATH] backup verify\n  mineral [--config PATH] backup init\n  mineral [--config PATH] doctor"
    );
}

fn init(path: &Path, format: ConfigFormat) -> Result<(), Box<dyn Error>> {
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
    let workspace = Workspace::load(path.to_path_buf())?;
    if workspace.source_kind() == SourceType::Local {
        fs::create_dir_all(workspace.local_source_path()?)?;
    }
    fs::create_dir_all(&workspace.config.state.path)?;
    fs::create_dir_all(workspace.cas())?;
    workspace.open_stores()?;
    println!(
        "Initialized Mineral workspace\n  config: {}\n  state: {}\n  source: {}",
        workspace.config_path.display(),
        workspace.config.state.path.display(),
        workspace.source_description()
    );
    Ok(())
}

/// Reads one complete source state as an immutable Snapshot.
///
/// The source itself is built by the runtime; what stays here is the progress a
/// human watches, because that is presentation.
fn snapshot(workspace: &Workspace) -> Result<Snapshot, Box<dyn Error>> {
    let source_id = SourceId::new(workspace.config.source.id.clone())?;
    match workspace.source_kind() {
        SourceType::Local => {
            let source = workspace.local_source(source_id.clone())?;
            let provisional = source.snapshot(SnapshotId::new(1)?, SystemTime::now())?;
            let id = snapshot_id(&source_id, provisional.files());
            source
                .snapshot(SnapshotId::new(id)?, SystemTime::now())
                .map_err(Into::into)
        }
        SourceType::R2 => {
            // One stabilized scan, one assembled state. The identity is computed from
            // the materialized bytes only, so the same bytes from a local directory
            // and from R2 describe the same source state.
            let source = workspace.r2_source()?;
            let stabilized = stabilize_scan(&source, DEFAULT_MAX_SCAN_ATTEMPTS)
                .map_err(|error| format!("could not read the R2 source: {error}"))?;
            eprintln!(
                "[1/4] R2 source {} prefix {}: {} file(s), {} read, {} reused, inventory {}",
                source.describe(),
                source.prefix(),
                stabilized.materialized().len(),
                source.fetched_objects(),
                source.reused_objects(),
                stabilized.inventory_identity(),
            );
            let provisional =
                stabilized.snapshot(SnapshotId::new(1)?, SystemTime::now(), source_id.clone())?;
            let id = snapshot_id(&source_id, provisional.files());
            stabilized
                .snapshot(SnapshotId::new(id)?, SystemTime::now(), source_id)
                .map_err(Into::into)
        }
    }
}

/// The deterministic snapshot identity of one complete source state.
///
/// It is derived from the source id and every file's path, size and content
/// identity — never from the source kind, so identical bytes from a local directory
/// and from R2 describe the same state.
fn snapshot_id(source_id: &SourceId, files: &[SnapshotFile]) -> u64 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(source_id.as_str().as_bytes());
    for file in files {
        hasher.update(file.path().as_str().as_bytes());
        hasher.update(file.size().to_le_bytes());
        hasher.update(file.sha256().as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    sqlite_positive_id(u64::from_be_bytes(bytes))
}

fn publish(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&workspace.config.state.path)?;
    let content_store = workspace.content_store();
    eprintln!("[1/4] Creating immutable Snapshot...");
    let snapshot = snapshot(&workspace)?;
    eprintln!(
        "[1/4] Snapshot {} contains {} files.",
        snapshot.id().get(),
        snapshot.files().len()
    );
    let document_runs = workspace.document_reviews()?;
    let asset_runs = workspace.asset_reviews()?;
    let human = workspace.human_reviews()?;
    let publish_runs = workspace.publish_runs()?;
    let delivery_projections = workspace.delivery_projections()?;
    let asset_target = workspace.asset_target()?;
    let public_scope = workspace.public_scope()?;
    let asset_location = asset_target.description();
    let asset_observations = workspace.asset_observations()?;
    let observations = workspace.remote_observations()?;
    let markdown_reviewer = workspace.markdown_reviewer()?;
    let asset_reviewer = workspace.asset_reviewer()?;
    let markdown_policy = PolicyIdentity::new(
        format!("deepseek:{}", workspace.config.review.markdown_model),
        MARKDOWN_REVIEWER_PROMPT_VERSION,
        workspace.markdown_contract_hash()?,
    )?;
    let asset_policy = PolicyIdentity::new(
        format!("deepseek:{}", workspace.config.review.asset_model),
        ASSET_REVIEWER_PROMPT_VERSION,
        workspace.asset_contract_hash()?,
    )?;
    let target = workspace.publish_target()?;
    let commit_metadata = workspace.publish_commit_metadata()?;
    let asset_delivery = workspace.asset_delivery()?;
    let request = PublicationApplicationRequest {
        snapshot: &snapshot,
        markdown_policy: &markdown_policy,
        asset_policy: &asset_policy,
        repository: &workspace.config.git.repository,
        target_id: workspace.publish_target_id()?,
        target,
        commit_metadata: &commit_metadata,
        human_reviews: ExplicitHumanReviewSelection::default(),
        public_scope: &public_scope,
        asset_delivery: &asset_delivery,
    };
    // Runtime concerns stay in the composition root: wall-clock time and the
    // review execution strategy are supplied to the application, never read by it.
    let created_at = SystemTime::now();
    let markdown_evaluator =
        HostMarkdownReviews::for_concurrency(workspace.config.review.markdown_concurrency);
    let asset_evaluator =
        HostAssetReviews::for_concurrency(workspace.config.review.asset_concurrency);
    let mut document_ids = RandomDocumentIds;
    let mut asset_ids = RandomAssetIds;
    let mut publish_ids = UuidPublishRunIdGenerator;
    let mut observation_ids = UuidRemoteObservationIdGenerator;
    let mut asset_observation_ids = UuidAssetObservationIdGenerator;
    eprintln!("[2/4] Running privacy, program checks, and semantic review...");
    let outcome = PublicationApplication::run(
        request,
        &content_store,
        &markdown_reviewer,
        &asset_reviewer,
        &document_runs,
        &asset_runs,
        &human,
        &publish_runs,
        &delivery_projections,
        &asset_target,
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut document_ids,
        &mut asset_ids,
        &mut publish_ids,
        &mut observation_ids,
        created_at,
        &markdown_evaluator,
        &asset_evaluator,
    )?;
    eprintln!("[4/4] Publication workflow finished.");
    render_publication(outcome, &asset_location, &public_scope);
    Ok(())
}

fn render_publication(
    outcome: PublicationApplicationOutcome,
    asset_location: &str,
    public_scope: &PublicExclusionRules,
) {
    match outcome {
        PublicationApplicationOutcome::NeedsHumanReview { trace } => {
            println!(
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
            println!("  public scope: {}", trace.public_scope());
            render_warnings(trace.markdown_reviews());
        }
        PublicationApplicationOutcome::Completed { trace, completed } => {
            let publication = completed.publication();
            let delivery = publication.workflow();
            let status = match delivery.git() {
                GitPublicationExecution::NoopSatisfied { .. } => "noop",
                GitPublicationExecution::Published { .. }
                | GitPublicationExecution::AlreadyPublished { .. } => "published",
                GitPublicationExecution::RemoteChanged { .. } => "conflict",
                GitPublicationExecution::Indeterminate { .. } => "indeterminate",
                GitPublicationExecution::TargetMissing { .. } => "target_missing",
                GitPublicationExecution::PushFailedButRemoteUnchanged { .. }
                | GitPublicationExecution::RemoteUnchangedAfterSuccessfulPush { .. } => {
                    "not_published"
                }
            };
            // A Git target holding the right Markdown is not a finished delivery:
            // its documents point at objects, and those have to be verified too.
            let status = if delivery.is_satisfied() {
                status
            } else {
                "incomplete"
            };
            let assets = match delivery.assets() {
                AssetPublicationOutcome::NoDurableProjection => "not_required".to_owned(),
                AssetPublicationOutcome::NotAttempted => "not_attempted".to_owned(),
                AssetPublicationOutcome::Satisfied {
                    verified,
                    published,
                } => format!("{verified} verified, {published} published"),
            };
            println!(
                "Publication\n  status: {status}\n  git: {}\n  run: {}\n  snapshot: {}\n  projection: {}\n  delivery: {}\n  markdown: {}\n  assets: {}\n  asset delivery: {}\n  asset target: {}",
                match delivery.git() {
                    GitPublicationExecution::NoopSatisfied { .. } => "noop",
                    GitPublicationExecution::Published { .. }
                    | GitPublicationExecution::AlreadyPublished { .. } => "published",
                    GitPublicationExecution::RemoteChanged { .. } => "conflict",
                    GitPublicationExecution::Indeterminate { .. } => "indeterminate",
                    GitPublicationExecution::TargetMissing { .. } => "target_missing",
                    GitPublicationExecution::PushFailedButRemoteUnchanged { .. }
                    | GitPublicationExecution::RemoteUnchangedAfterSuccessfulPush { .. } => {
                        "not_published"
                    }
                },
                publication.publish_run_id().get(),
                trace.snapshot().id().get(),
                completed.projection().projection_sha256(),
                publication.delivery_sha256(),
                completed.publication_set().markdown_paths().len(),
                completed.publication_set().asset_paths().len(),
                assets,
                asset_location
            );
            println!(
                "  public scope: {} ({} rule(s))",
                trace.public_scope(),
                public_scope.len()
            );
            if status == "noop" {
                println!("  No changes to publish.");
            }
            render_warnings(trace.markdown_reviews());
        }
    }
}

fn render_warnings(result: &mineral_publisher::workflow::PublicPolicyRunResult) {
    println!("\nWarnings\n  navigation: {}", result.warnings().len());
    for warning in result.warnings() {
        println!(
            "  {} {:?} target={} span={:?}",
            warning.document_path(),
            warning.origin().kind(),
            warning.origin().target(),
            warning.origin().span()
        );
    }
}

fn status(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    let documents = workspace.document_reviews()?;
    let assets = workspace.asset_reviews()?;
    let human = workspace.human_reviews()?;
    let runs = workspace.publish_runs()?;
    workspace.source_materializations()?;
    let pending_documents =
        HumanReviewResolution::list_pending_documents(&documents, &human)?.len();
    let pending_assets = HumanReviewResolution::list_pending_assets(&assets, &human)?.len();
    let all_runs = runs.list()?;
    let last = all_runs.last();
    println!(
        "Mineral status\n  source: {} ({})\n  state: {}\n  publication target: {} {}\n  last publication: {}\n  pending Markdown reviews: {}\n  pending Asset reviews: {}",
        workspace.config.source.id,
        workspace.source_description(),
        workspace.config.state.path.display(),
        workspace.config.git.remote,
        workspace.config.git.reference,
        last.map(|run| run.id().get().to_string())
            .unwrap_or_else(|| "none".to_owned()),
        pending_documents,
        pending_assets
    );
    Ok(())
}

fn review(workspace: Workspace, args: &[String]) -> Result<(), Box<dyn Error>> {
    let documents = workspace.document_reviews()?;
    let assets = workspace.asset_reviews()?;
    let human = workspace.human_reviews()?;
    match args.first().map(String::as_str) {
        Some("list") => {
            println!("Markdown");
            for run in HumanReviewResolution::list_pending_documents(&documents, &human)? {
                println!("  document:{}  {}", run.id().get(), run.content_path());
            }
            println!("Assets");
            for run in HumanReviewResolution::list_pending_assets(&assets, &human)? {
                println!("  asset:{}  {}", run.id().get(), run.content_path());
            }
            Ok(())
        }
        Some("show") => show_review(
            args.get(1).ok_or("review show requires an ID")?,
            &documents,
            &assets,
            &human,
        ),
        Some("approve") => resolve_review(
            args.get(1).ok_or("review approve requires an ID")?,
            HumanReviewDecision::Approve,
            &documents,
            &assets,
            &human,
        ),
        Some("reject") => resolve_review(
            args.get(1).ok_or("review reject requires an ID")?,
            HumanReviewDecision::Reject,
            &documents,
            &assets,
            &human,
        ),
        _ => Err("review requires list, show, approve, or reject".into()),
    }
}

/// Parses the operator-facing review ID, which names one automatic attempt.
///
/// The ID an operator sees is the attempt they are being asked about. The decision
/// they make is recorded against the reviewed content and policy, so approving an
/// attempt answers every later attempt about the same subject.
fn parse_attempt(value: &str) -> Result<HumanReviewAttempt, Box<dyn Error>> {
    let (kind, id) = value
        .split_once(':')
        .ok_or("review ID must be document:ID or asset:ID")?;
    let id: u64 = id.parse()?;
    match kind {
        "document" => Ok(HumanReviewAttempt::Document(ReviewRunId::new(id)?)),
        "asset" => Ok(HumanReviewAttempt::Asset(AssetReviewRunId::new(id)?)),
        _ => Err("review ID must be document:ID or asset:ID".into()),
    }
}

fn show_review(
    subject: &str,
    documents: &SqliteReviewRunStore,
    assets: &SqliteAssetReviewRunStore,
    human: &SqliteHumanReviewStore,
) -> Result<(), Box<dyn Error>> {
    let attempt = parse_attempt(subject)?;
    match attempt {
        HumanReviewAttempt::Document(id) => {
            let run = documents.get(id)?.ok_or("document review not found")?;
            println!(
                "Review\n  id: document:{}\n  subject: Markdown\n  path: {}\n  sha256: {}\n  decision: {:?}\n  contract: {} {} {}",
                id.get(),
                run.content_path(),
                run.content_sha256(),
                run.decision(),
                run.policy().name(),
                run.policy().version(),
                run.policy().hash()
            );
            if let Some(report) = run.reviewer_report() {
                println!(
                    "  reason_codes: {:?}\n  summary: {}",
                    report.reason_codes(),
                    report.summary()
                );
            }
        }
        HumanReviewAttempt::Asset(id) => {
            let run = assets.get(id)?.ok_or("asset review not found")?;
            println!(
                "Review\n  id: asset:{}\n  subject: Asset\n  path: {}\n  sha256: {}\n  decision: {:?}\n  contract: {} {} {}",
                id.get(),
                run.content_path(),
                run.content_sha256(),
                run.outcome().disposition(),
                run.policy().name(),
                run.policy().version(),
                run.policy().hash()
            );
            if let Some(report) = run.outcome().reviewer_report() {
                println!(
                    "  reason_codes: {:?}\n  summary: {}",
                    report.reason_codes(),
                    report.summary()
                );
            }
        }
    }
    // A decision is recognised by the subject it decided about, so the question
    // this attempt is still asking is answered by that lookup first, and by the
    // attempt itself for a record written before subjects were bound.
    let resolution = match attempt {
        HumanReviewAttempt::Document(id) => {
            let run = documents.get(id)?.ok_or("document review not found")?;
            HumanReviewResolution::document_resolution(&run, human)?
        }
        HumanReviewAttempt::Asset(id) => {
            let run = assets.get(id)?.ok_or("asset review not found")?;
            HumanReviewResolution::asset_resolution(&run, human)?
        }
    };
    println!(
        "  human_resolution: {}",
        if resolution.is_some() {
            "resolved"
        } else {
            "pending"
        }
    );
    Ok(())
}

fn resolve_review(
    subject: &str,
    decision: HumanReviewDecision,
    documents: &SqliteReviewRunStore,
    assets: &SqliteAssetReviewRunStore,
    human: &SqliteHumanReviewStore,
) -> Result<(), Box<dyn Error>> {
    let attempt = parse_attempt(subject)?;
    let existing = match attempt {
        HumanReviewAttempt::Document(id) => {
            let run = documents.get(id)?.ok_or("document review not found")?;
            HumanReviewResolution::document_resolution(&run, human)?
        }
        HumanReviewAttempt::Asset(id) => {
            let run = assets.get(id)?.ok_or("asset review not found")?;
            HumanReviewResolution::asset_resolution(&run, human)?
        }
    };
    if let Some(existing) = existing {
        if existing.decision() == decision {
            println!("Review already {:?}.", decision);
            return Ok(());
        }
        return Err("review already has the opposite immutable human resolution".into());
    }
    let id = random_human_id()?;
    match attempt {
        HumanReviewAttempt::Document(run) => {
            HumanReviewResolution::resolve_document(
                documents,
                human,
                id,
                run,
                decision,
                SystemTime::now(),
                None,
                None,
            )?;
        }
        HumanReviewAttempt::Asset(run) => {
            HumanReviewResolution::resolve_asset(
                assets,
                human,
                id,
                run,
                decision,
                SystemTime::now(),
                None,
                None,
            )?;
        }
    }
    println!("Review {:?}.", decision);
    Ok(())
}

/// Dispatches the backup subcommands, defaulting to one backup run.
fn backup(workspace: Workspace, args: &[String]) -> Result<(), Box<dyn Error>> {
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
/// The Snapshot comes from the same helper `publish` uses, so a local directory and
/// an R2 prefix both back up. Nothing remote happens until the intent is durable,
/// and the report names the run, the Snapshot it froze and the ref it moved.
fn backup_run(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    if !workspace.backup_enabled() {
        println!("Backup is not configured for this workspace; nothing to do.");
        return Ok(());
    }
    fs::create_dir_all(&workspace.config.state.path)?;
    let content_store = workspace.content_store();
    let snapshot = snapshot(&workspace)?;
    let store = workspace.backup_runs()?;
    let repository = workspace.backup_repository()?;
    let remote = workspace.backup_git_remote()?;
    let lfs = workspace.backup_lfs_remote()?;
    let target = workspace.backup_target()?;
    let metadata = workspace.backup_commit_metadata()?;
    let policy = TypeFirstBackupRepresentationPolicy;
    // Wall-clock time is a runtime concern: it is frozen into the intent here and
    // never read by the engine.
    let created_at = TimestampMillis::from_system_time(SystemTime::now())
        .ok_or("system time is before the Unix epoch")?;
    let request = BackupApplicationRequest {
        snapshot: &snapshot,
        target: &target,
        commit_metadata: &metadata,
        policy: &policy,
        created_at,
    };
    let run_ids = UuidBackupRunIdGenerator;
    let outcome = match run_backup(
        request,
        &store,
        &repository,
        &remote,
        &lfs,
        &content_store,
        &run_ids,
    ) {
        Ok(outcome) => outcome,
        // A missing ref is one operator action away from a backup, so it gets the
        // same short report as the other statuses before failing closed.
        Err(BackupApplicationError::NoBaseCommit(target)) => {
            println!(
                "Backup\n  status: ref missing\n  ref: {} {}\n  snapshot: {}\n  files: {}",
                target.remote_name(),
                target.destination_ref(),
                snapshot.id().get(),
                snapshot.files().len()
            );
            return Err(format!(
                "backup ref {} {} does not exist yet; run `mineral backup init` once to create it",
                target.remote_name(),
                target.destination_ref()
            )
            .into());
        }
        Err(error) => return Err(Box::new(error)),
    };
    render_backup_outcome(&outcome, &target, &lfs);
    Ok(())
}

/// What one backup attempt did, in the operator's words.
fn render_backup_outcome(
    outcome: &BackupApplicationOutcome,
    target: &GitRefTarget,
    lfs: &LfsHttpRemote,
) {
    let status = match outcome.execution() {
        BackupExecutionOutcome::BackedUp { .. } => "backed up",
        BackupExecutionOutcome::AlreadyBackedUp { .. } => "already backed up",
        BackupExecutionOutcome::RemoteChanged { .. } => "remote changed",
    };
    println!(
        "Backup\n  status: {status}\n  run: {}\n  snapshot: {}\n  files: {}\n  lfs objects: {}\n  ref: {} {}\n  endpoint: {}",
        outcome
            .run_id()
            .map(|id| id.get().to_string())
            .unwrap_or_else(|| "none (already up to date)".to_owned()),
        outcome.snapshot_id().get(),
        outcome.files(),
        outcome.lfs_objects(),
        target.remote_name(),
        target.destination_ref(),
        lfs.describe()
    );
}

/// Reports the observed backup ref and the newest durable intent.
///
/// The only network call is the `ls-remote` behind the observation.
fn backup_status(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    println!("Mineral backup");
    if !workspace.backup_enabled() {
        println!("  backup: not configured");
        return Ok(());
    }
    let target = workspace.backup_target()?;
    match workspace.observe_backup_ref(&target) {
        Ok(RemoteRefState::Present { commit_oid }) => println!(
            "  ref: {} {} ({})",
            target.remote_name(),
            target.destination_ref(),
            commit_oid.as_str()
        ),
        Ok(RemoteRefState::Missing) => println!(
            "  ref: {} {} (missing)",
            target.remote_name(),
            target.destination_ref()
        ),
        Err(error) => println!(
            "  ref: {} {} (unavailable: {error})",
            target.remote_name(),
            target.destination_ref()
        ),
    }
    let store = workspace.backup_runs()?;
    match store.list()?.last() {
        Some(run) => println!(
            "  newest run: {}\n  snapshot: {}\n  delivery: {}",
            run.id().get(),
            run.snapshot_id().get(),
            run.delivery_sha256()
        ),
        None => println!("  newest run: none"),
    }
    Ok(())
}

/// Verifies that the backup ref can be restored byte-for-byte.
fn backup_verify(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    if !workspace.backup_enabled() {
        println!("Backup is not configured for this workspace; nothing to verify.");
        return Ok(());
    }
    let repository = workspace.backup_repository()?;
    let remote = workspace.backup_git_remote()?;
    let lfs = workspace.backup_lfs_remote()?;
    let target = workspace.backup_target()?;
    let report = verify_backup(&repository, &remote, &lfs, &target, None)
        .map_err(|error| format!("backup verification failed: {error}"))?;
    println!(
        "Backup verify\n  status: verified\n  commit: {}\n  files: {}\n  lfs objects: {}",
        report.commit().as_str(),
        report.files_verified(),
        report.lfs_objects_verified()
    );
    Ok(())
}

/// Bootstraps the backup ref with one empty root commit.
///
/// The engine builds every backup on the commit the ref already holds, so the very
/// first backup needs a base that is derived from nothing. This is the one place the
/// host creates history rather than continuing it, and an existing ref is never
/// touched.
fn backup_init(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    if !workspace.backup_enabled() {
        println!("Backup is not configured for this workspace; nothing to initialize.");
        return Ok(());
    }
    let target = workspace.backup_target()?;
    match workspace.observe_backup_ref(&target)? {
        RemoteRefState::Present { commit_oid } => {
            println!(
                "Backup ref {} {} already exists at {}; nothing changed.",
                target.remote_name(),
                target.destination_ref(),
                commit_oid.as_str()
            );
            Ok(())
        }
        RemoteRefState::Missing => {
            let metadata = workspace.backup_commit_metadata()?;
            let commit = workspace.backup_root_commit(&metadata)?;
            workspace.push_backup_root_commit(&target, &commit)?;
            println!(
                "Initialized backup ref {} {} at {}.",
                target.remote_name(),
                target.destination_ref(),
                commit.as_str()
            );
            Ok(())
        }
    }
}

fn doctor(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    let mut failed = false;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!(
            "  {name}: {} ({detail})",
            if ok {
                "ok"
            } else {
                failed = true;
                "failed"
            }
        );
    };
    println!("Mineral doctor");
    check(
        "configuration",
        true,
        workspace.config_path.display().to_string(),
    );
    match workspace.source_kind() {
        SourceType::Local => check(
            "source",
            workspace
                .config
                .source
                .path
                .as_ref()
                .is_some_and(|path| path.is_dir()),
            workspace.source_description(),
        ),
        SourceType::R2 => check(
            "source",
            env::var_os(
                workspace
                    .config
                    .source
                    .r2
                    .as_ref()
                    .map(|r2| r2.secret_access_key_env.clone())
                    .unwrap_or_default(),
            )
            .is_some(),
            format!("{} (credential)", workspace.source_description()),
        ),
    }
    check(
        "state",
        workspace.config.state.path.is_dir(),
        workspace.config.state.path.display().to_string(),
    );
    check(
        "CAS",
        workspace.cas().is_dir(),
        workspace.cas().display().to_string(),
    );
    check(
        "database",
        [
            workspace.document_db(),
            workspace.asset_db(),
            workspace.human_db(),
            workspace.publish_db(),
            workspace.observation_db(),
            workspace.delivery_db(),
            workspace.asset_observations_db(),
            workspace.source_materializations_db(),
        ]
        .iter()
        .all(|path| path.is_file()),
        workspace.config.state.path.display().to_string(),
    );
    check(
        "Git target",
        workspace.config.git.repository.is_dir(),
        workspace.config.git.repository.display().to_string(),
    );
    // Presence only: the backup check never touches the network, so `doctor` stays
    // usable while the backup endpoint is unreachable.
    check(
        "backup",
        true,
        match workspace.config.backup.as_ref() {
            Some(backup) if backup.enabled => {
                let repository = backup
                    .git
                    .as_ref()
                    .and_then(|git| git.repository.as_ref())
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unconfigured>".to_owned());
                format!(
                    "{repository} (lfs {})",
                    if backup.lfs.as_ref().is_some_and(|lfs| lfs.enabled) {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )
            }
            _ => "not configured".to_owned(),
        },
    );
    check(
        "remote/ref",
        workspace.publication_ref_present(),
        format!(
            "{} {}",
            workspace.config.git.remote, workspace.config.git.reference
        ),
    );
    check(
        "provider credential",
        env::var_os(&workspace.config.review.api_key_env).is_some(),
        format!(
            "{}: {}",
            workspace.config.review.api_key_env,
            if env::var_os(&workspace.config.review.api_key_env).is_some() {
                "present"
            } else {
                "missing"
            }
        ),
    );
    if failed {
        Err("one or more doctor checks failed".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use mineral_publisher::backup::lfs_http::{LfsHttpConfig, LfsToken};

    use super::{
        Config, ConfigFormat, DEFAULT_CONFIG, LfsHttpRemote, SourceType, Workspace, backup, init,
        sqlite_positive_id,
    };

    /// The three asset-target shapes a configuration can have: the native store,
    /// neither target, and both. Only the first is publishable, and the two that
    /// are not are refused while the workspace is loaded rather than when a
    /// publication is already under way.
    #[test]
    fn a_workspace_must_name_exactly_one_asset_target() {
        let directory =
            std::env::temp_dir().join(format!("mineral-cli-assets-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let r2 = "  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-assets\n    access_key_id: AKIDEXAMPLE\n    secret_access_key_env: MINERAL_R2_SECRET_ACCESS_KEY\n";

        let native = DEFAULT_CONFIG.to_owned();
        let neither = DEFAULT_CONFIG.replace("  target_path: ./asset-target\n", "");
        let both = DEFAULT_CONFIG.replace(
            "  target_path: ./asset-target\n",
            &format!("  target_path: ./asset-target\n{r2}"),
        );

        for (name, text, refusal) in [
            ("native", &native, None),
            ("neither", &neither, Some("either target_path or r2")),
            ("both", &both, Some("choose one")),
        ] {
            let path = directory.join(format!("{name}.yml"));
            std::fs::write(&path, text).unwrap();
            match (refusal, Workspace::load(path)) {
                (None, Ok(workspace)) => {
                    let target = workspace.asset_target().unwrap();
                    assert!(target.description().starts_with("filesystem:"));
                }
                (None, Err(error)) => panic!("the native configuration must load: {error}"),
                (Some(_), Ok(_)) => panic!("{name} must be refused"),
                (Some(fragment), Err(error)) => {
                    let error = error.to_string();
                    assert!(error.contains(fragment), "{name}: {error}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// `public.exclude` is parsed into validated core rules, and an absent section
    /// means the empty scope every earlier workspace had.
    #[test]
    fn a_workspace_parses_its_public_scope() {
        let directory =
            std::env::temp_dir().join(format!("mineral-cli-public-scope-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();

        // Absent: no exclusion at all.
        let path = directory.join("absent.yml");
        std::fs::write(&path, DEFAULT_CONFIG).unwrap();
        let workspace = Workspace::load(path).unwrap();
        assert!(workspace.public_scope().unwrap().is_empty());

        // Configured: the rules are parsed, canonicalized and validated at load.
        let configured = DEFAULT_CONFIG.replace(
            "  exclude: []\n",
            "  exclude:\n    - \"notes/internal.md\"\n    - \"private/**\"\n    - \"attachments/private.png\"\n    - \"**/*.tmp\"\n",
        );
        let path = directory.join("configured.yml");
        std::fs::write(&path, configured).unwrap();
        let scope = Workspace::load(path).unwrap().public_scope().unwrap();
        assert_eq!(
            scope.canonical(),
            [
                "**/*.tmp",
                "attachments/private.png",
                "notes/internal.md",
                "private/**"
            ]
        );
        assert!(scope.excludes(&mineral_core::domain::ContentPath::new("private/a.md").unwrap()));

        // Malformed: refused while the workspace is loaded, not during a publication.
        let malformed =
            DEFAULT_CONFIG.replace("  exclude: []\n", "  exclude:\n    - \"../outside/**\"\n");
        let path = directory.join("malformed.yml");
        std::fs::write(&path, malformed).unwrap();
        let error = Workspace::load(path)
            .expect_err("an unusable rule must be refused")
            .to_string();
        assert!(error.contains("public.exclude"), "{error}");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The secret key is never read from the configuration file: it comes from
    /// the environment, and a workspace that names a variable which is not set
    /// fails closed instead of publishing with an empty credential.
    #[test]
    fn an_r2_target_reads_its_secret_from_the_environment() {
        let directory = std::env::temp_dir().join(format!("mineral-cli-r2-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let text = DEFAULT_CONFIG.replace(
            "  target_path: ./asset-target\n",
            "  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-assets\n    access_key_id: AKIDEXAMPLE\n    secret_access_key_env: MINERAL_R2_TEST_UNSET_SECRET\n",
        );
        let path = directory.join("r2.yml");
        std::fs::write(&path, text).unwrap();
        let workspace = Workspace::load(path).unwrap();

        let error = workspace
            .asset_target()
            .err()
            .expect("an unset secret must be refused")
            .to_string();

        assert!(error.contains("MINERAL_R2_TEST_UNSET_SECRET"), "{error}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn default_config_has_no_managed_root_and_legacy_override_is_rejected() {
        serde_yaml_ng::from_str::<Config>(DEFAULT_CONFIG).unwrap();
        let legacy = DEFAULT_CONFIG.replace(
            "  reference: refs/heads/main\n",
            "  reference: refs/heads/main\n  managed_root: content\n",
        );

        let error = serde_yaml_ng::from_str::<Config>(&legacy).unwrap_err();
        assert!(error.to_string().contains("unknown field `managed_root`"));
    }

    #[test]
    fn generated_ids_always_fit_positive_sqlite_integer_range() {
        for value in [0, 1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            let id = sqlite_positive_id(value);
            assert!((1..=i64::MAX as u64).contains(&id));
        }
    }
    /// A workspace for the source-configuration tests: one source block, one asset
    /// block, everything else minimal.
    fn source_workspace(source: &str, assets: &str) -> Result<Workspace, Box<dyn Error>> {
        let text = format!(
            "source:\n{source}state:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\n{assets}review:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key_env: MINERAL_DEEPSEEK_API_KEY\n  timeout_seconds: 45\n"
        );
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let directory = std::env::temp_dir().join(format!(
            "mineral-cli-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("mineral.yaml");
        fs::write(&path, text).unwrap();
        Workspace::load(path).map_err(Into::into)
    }

    fn r2_source_block(prefix: &str) -> String {
        format!(
            "  id: r2-vault\n  type: r2\n  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-vault\n    prefix: {prefix}\n    access_key_id: AKIDEXAMPLE\n    secret_access_key_env: MINERAL_R2_TEST_UNSET_SECRET\n"
        )
    }

    fn r2_assets_block(bucket: &str) -> String {
        format!(
            "assets:\n  public_base_url: https://assets.example.com\n  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: {bucket}\n    access_key_id: AKIDEXAMPLE\n    secret_access_key_env: MINERAL_R2_TEST_UNSET_SECRET\n"
        )
    }

    #[test]
    fn a_local_source_keeps_working_without_a_type_and_absolute_paths_its_root() {
        let workspace = source_workspace("  id: local-vault\n  path: ./vault\n", "").unwrap();

        assert_eq!(workspace.source_kind(), SourceType::Local);
        assert!(workspace.local_source_path().unwrap().is_absolute());
        assert!(workspace.source_description().ends_with("vault"));
        assert!(workspace.r2_source().is_err());
    }

    #[test]
    fn an_r2_source_canonicalizes_its_prefix_and_reads_its_secret_lazily() {
        let workspace = source_workspace(&r2_source_block("vault"), "").unwrap();

        assert_eq!(workspace.source_kind(), SourceType::R2);
        assert_eq!(
            workspace.config.source.r2.as_ref().unwrap().prefix,
            "vault/",
            "a configured namespace without the separator is canonicalized once"
        );
        assert!(
            workspace
                .source_description()
                .contains("mineral-vault/vault/")
        );
        assert!(workspace.local_source_path().is_err());

        let error = match workspace.r2_source() {
            Ok(_) => panic!("an unset secret must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("MINERAL_R2_TEST_UNSET_SECRET"), "{error}");
    }

    #[test]
    fn a_source_must_choose_exactly_one_kind() {
        // `type: local` cannot also carry an R2 namespace.
        let local_with_r2 = source_workspace(
            "  id: local-vault\n  path: ./vault\n  type: local\n  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-vault\n    prefix: vault/\n    access_key_id: A\n    secret_access_key_env: X\n",
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(
            local_with_r2.contains("source.r2 is configured"),
            "{local_with_r2}"
        );

        // `type: r2` needs its namespace.
        let r2_without_block = source_workspace("  id: r2-vault\n  type: r2\n", "")
            .unwrap_err()
            .to_string();
        assert!(
            r2_without_block.contains("source.r2 is not configured"),
            "{r2_without_block}"
        );

        // An R2 source must not keep reading a local directory.
        let r2_with_path = source_workspace(
            &format!("  path: ./vault\n{}", r2_source_block("vault/")),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(
            r2_with_path.contains("source.path is not used"),
            "{r2_with_path}"
        );

        // A local source still requires its root.
        let local_without_path = source_workspace("  id: local-vault\n", "")
            .unwrap_err()
            .to_string();
        assert!(
            local_without_path.contains("source.path is required"),
            "{local_without_path}"
        );
    }

    #[test]
    fn an_unusable_source_prefix_is_refused_before_anything_reads_the_namespace() {
        for prefix in ["vault//", "../escape", "/absolute", "C:/vault"] {
            let error = source_workspace(&r2_source_block(prefix), "")
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(
                error.contains("source.r2.prefix is unusable"),
                "prefix {prefix:?} was accepted: {error}"
            );
        }
        // `/` names the same thing ambiguously, so it is refused.
        let ambiguous = source_workspace(&r2_source_block("/"), "")
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(
            ambiguous.contains("source.r2.prefix is unusable"),
            "{ambiguous}"
        );
    }

    #[test]
    fn an_explicit_root_prefix_reads_the_whole_bucket_but_never_the_publication_namespace() {
        // The empty prefix is an explicit choice and is accepted.
        let root = source_workspace(&r2_source_block("\"\""), "");
        let root = match root {
            Ok(workspace) => workspace,
            Err(error) => panic!("an explicit root prefix was refused: {error}"),
        };
        assert_eq!(root.source_kind(), SourceType::R2);
        assert_eq!(root.config.source.r2.as_ref().unwrap().prefix, "");

        // A root source and the publication namespace cannot share one bucket: the
        // root contains every publication key.
        let overlapping =
            match source_workspace(&r2_source_block("\"\""), &r2_assets_block("mineral-vault")) {
                Ok(_) => panic!("a root source over the publication bucket was accepted"),
                Err(error) => error.to_string(),
            };
        assert!(
            overlapping.contains("overlaps the publication namespace"),
            "{overlapping}"
        );

        // A different bucket is a different namespace, and the root is fine there.
        assert!(
            source_workspace(&r2_source_block("\"\""), &r2_assets_block("other-bucket")).is_ok()
        );
    }

    #[test]
    fn a_source_namespace_may_not_overlap_the_publication_namespace() {
        // Same endpoint and bucket, one namespace inside the other: refused.
        for prefix in ["assets", "assets/sha256", "assets/sha256/deep"] {
            let error =
                source_workspace(&r2_source_block(prefix), &r2_assets_block("mineral-vault"))
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_default();
            assert!(
                error.contains("overlaps the publication namespace"),
                "{prefix}: {error}"
            );
        }
        // A sibling namespace on the same bucket is exactly the supported case.
        source_workspace(
            &r2_source_block("vault/"),
            &r2_assets_block("mineral-vault"),
        )
        .unwrap();
        // Same namespace but a different bucket is a different namespace.
        source_workspace(
            &r2_source_block("assets/"),
            &r2_assets_block("other-bucket"),
        )
        .unwrap();
    }

    /// A workspace for the backup tests: one local source plus the caller's blocks.
    fn configured_workspace(
        source: &str,
        assets: &str,
        backup: &str,
    ) -> Result<Workspace, Box<dyn Error>> {
        let text = format!(
            "source:\n{source}state:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\n{assets}{backup}review:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key_env: MINERAL_DEEPSEEK_API_KEY\n  timeout_seconds: 45\n"
        );
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let directory = std::env::temp_dir().join(format!(
            "mineral-cli-backup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("mineral.yaml");
        fs::write(&path, text).unwrap();
        Workspace::load(path).map_err(Into::into)
    }

    /// A valid backup block, so each refusal test can change exactly one field.
    fn valid_backup_block() -> String {
        "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n    author_name: Mineral Backup\n    author_email: backup@example.invalid\n    message: Backup knowledge snapshot\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_BACKUP_TEST_LFS_USER\n    token_env: MINERAL_BACKUP_TEST_LFS_TOKEN\n    timeout_seconds: 300\n"
            .to_owned()
    }

    /// A configured backup parses, its repository is absolutized like `git.repository`,
    /// and its target and commit identity are usable without any network call.
    #[test]
    fn a_backup_section_parses_and_absolutizes_its_repository() {
        let workspace = configured_workspace(
            "  id: local-vault\n  path: ./vault\n",
            "",
            &valid_backup_block(),
        )
        .unwrap();

        assert!(workspace.backup_enabled());
        let backup = workspace.config.backup.as_ref().unwrap();
        assert!(backup.enabled);
        let git = backup.git.as_ref().unwrap();
        let repository = git.repository.as_ref().unwrap();
        assert!(repository.is_absolute(), "{}", repository.display());
        assert!(
            repository.ends_with("backup-repo"),
            "{}",
            repository.display()
        );
        assert_eq!(
            git.branch.as_deref(),
            Some("refs/heads/mineral-backup"),
            "the fully qualified branch is stored verbatim"
        );
        assert_eq!(
            workspace.backup_target().unwrap().destination_ref(),
            "refs/heads/mineral-backup"
        );
        let metadata = workspace.backup_commit_metadata().unwrap();
        assert_eq!(metadata.author_name(), "Mineral Backup");
        assert_eq!(metadata.message(), "Backup knowledge snapshot");
    }

    /// An unusable enabled backup section is refused while the workspace loads, so no
    /// Snapshot, ref or LFS object is ever touched. A disabled section stays inert.
    #[test]
    fn an_unusable_backup_section_is_refused_while_the_workspace_loads() {
        let git = "  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n";
        let lfs = "  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_BACKUP_TEST_LFS_USER\n    token_env: MINERAL_BACKUP_TEST_LFS_TOKEN\n";
        let source = "  id: local-vault\n  path: ./vault\n";
        let cases = [
            (
                "missing branch",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n",
                "backup.git.branch",
            ),
            (
                "empty branch",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: \"\"\n",
                "backup.git.branch",
            ),
            (
                "unqualified branch",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: main\n",
                "backup.git.branch",
            ),
            (
                "missing remote",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    branch: refs/heads/mineral-backup\n",
                "backup.git.remote",
            ),
            (
                "missing repository",
                "backup:\n  enabled: true\n  git:\n    remote: origin\n    branch: refs/heads/mineral-backup\n",
                "backup.git.repository",
            ),
            ("missing git", "backup:\n  enabled: true\n", "backup.git"),
            (
                "missing lfs block",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n",
                "backup.lfs",
            ),
            (
                "disabled lfs block",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: false\n    username_env: U\n    token_env: T\n",
                "backup.lfs",
            ),
            (
                "non-http batch url",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: file:///tmp/lfs\n    username_env: U\n    token_env: T\n",
                "backup.lfs.batch_url",
            ),
            (
                "empty username variable name",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: \"\"\n    token_env: T\n",
                "backup.lfs.username_env",
            ),
            (
                "empty token variable name",
                "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: U\n    token_env: \"\"\n",
                "backup.lfs.token_env",
            ),
        ];
        for (name, backup, fragment) in cases {
            let error = configured_workspace(source, "", backup)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(error.contains(fragment), "{name}: {error}");
        }
        // The unchanged fields are exactly what the refusals above prove: the same
        // block without the one changed field loads.
        configured_workspace(source, "", &format!("backup:\n  enabled: true\n{git}{lfs}")).unwrap();
        // A disabled section is inert: it may omit everything an enabled one needs.
        configured_workspace(source, "", "backup:\n  enabled: false\n").unwrap();
    }

    /// A workspace without a `backup:` section still parses, and the command is a
    /// successful no-op rather than an error.
    #[test]
    fn a_workspace_without_a_backup_section_keeps_working() {
        let workspace = source_workspace("  id: local-vault\n  path: ./vault\n", "").unwrap();
        assert!(workspace.config.backup.is_none());
        assert!(!workspace.backup_enabled());

        backup(workspace, &[]).unwrap();
    }

    /// Credentials never reach a configuration dump or an endpoint report.
    ///
    /// Edition 2024 makes writing to the process environment `unsafe`, and this
    /// project forbids `unsafe`, so the environment value is supplied directly to the
    /// same adapter a backup builds. The redaction is identical: only the variable
    /// *name* may appear, and only the username may appear in `describe()`.
    #[test]
    fn lfs_credentials_never_appear_in_a_config_dump_or_an_endpoint_report() {
        let workspace = configured_workspace(
            "  id: local-vault\n  path: ./vault\n",
            "",
            &valid_backup_block(),
        )
        .unwrap();
        let dump = format!("{:?}", workspace.config);
        assert!(dump.contains("MINERAL_BACKUP_TEST_LFS_TOKEN"), "{dump}");
        assert!(!dump.contains("super-secret-token"), "{dump}");

        // A named-but-unset variable fails closed by name, never by value.
        let unset = configured_workspace(
            "  id: local-vault\n  path: ./vault\n",
            "",
            "backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_BACKUP_TEST_UNSET_USER\n    token_env: MINERAL_BACKUP_TEST_UNSET_TOKEN\n",
        )
        .unwrap();
        let error = unset
            .backup_lfs_remote()
            .err()
            .expect("an unset credential variable must be refused")
            .to_string();
        assert!(error.contains("MINERAL_BACKUP_TEST_UNSET_USER"), "{error}");

        let secret = "super-secret-token";
        let config = LfsHttpConfig::new(
            "https://github.com/owner/repo.git/info/lfs",
            "mineral-backup",
            LfsToken::new(secret).unwrap(),
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(!config.describe().contains(secret), "{}", config.describe());
        assert!(!format!("{config:?}").contains(secret));
        let remote = LfsHttpRemote::new(config).unwrap();
        assert!(remote.describe().contains("mineral-backup"));
        assert!(!remote.describe().contains(secret), "{}", remote.describe());
    }

    /// `init` writes the language the file name promises, and refuses to write
    /// one whose name promises a different language.
    ///
    /// The extension is what chooses the syntax when the file is read back, so a
    /// `.yaml` file holding TOML would be a workspace that cannot be opened —
    /// refused here rather than by a parser later.
    #[test]
    fn init_writes_the_language_the_file_name_promises() {
        let directory =
            std::env::temp_dir().join(format!("mineral-cli-init-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();

        let toml = directory.join("workspace.toml");
        init(&toml, ConfigFormat::Toml).unwrap();
        let text = std::fs::read_to_string(&toml).unwrap();
        assert!(text.contains("[source]"), "{text}");
        assert_eq!(
            Workspace::load(toml.clone()).unwrap().source_kind(),
            SourceType::Local
        );

        let mismatched = directory.join("workspace.yaml");
        let error = init(&mismatched, ConfigFormat::Toml)
            .expect_err("a .yaml name must not be written as TOML")
            .to_string();
        assert!(error.contains("toml"), "{error}");
        assert!(!mismatched.exists());

        let error = init(&toml, ConfigFormat::Toml)
            .expect_err("an existing configuration must never be overwritten")
            .to_string();
        assert!(error.contains("already exists"), "{error}");

        let _ = std::fs::remove_dir_all(&directory);
    }
}
