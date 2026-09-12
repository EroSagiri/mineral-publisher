use std::{
    convert::Infallible,
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use mineral_publisher::{
    domain::{Sha256, Snapshot, SnapshotId, SourceId},
    policy::{
        PolicyIdentity, ReviewCandidate, ReviewRunId, ReviewRunStore, Reviewer, ReviewerError,
        ReviewerReport,
    },
    publisher::{
        GitCommitMetadata, PublicationTarget, PublicationWorkflowResult, PublishRunStore,
        UuidPublishRunIdGenerator, UuidRemoteObservationIdGenerator,
    },
    reviewer::{
        ASSET_REVIEWER_PROMPT_VERSION, DeepSeekApiKey, DeepSeekAssetReviewer,
        DeepSeekAssetReviewerConfig, DeepSeekMarkdownReviewer, DeepSeekMarkdownReviewerConfig,
        MARKDOWN_REVIEWER_PROMPT_VERSION,
    },
    source::LocalSource,
    storage::{
        LocalContentStore, SqliteAssetReviewRunStore, SqliteHumanReviewStore,
        SqlitePublishRunStore, SqliteRemoteObservationStore, SqliteReviewRunStore,
    },
    workflow::{
        AssetReviewCandidate, AssetReviewRunId, AssetReviewRunIdGenerator, AssetReviewRunStore,
        AssetReviewer, AssetReviewerError, AssetReviewerReport, ExplicitHumanReviewSelection,
        HumanReviewDecision, HumanReviewId, HumanReviewRecordError, HumanReviewResolution,
        HumanReviewStore, HumanReviewSubject, PublicationApplication,
        PublicationApplicationOutcome, PublicationApplicationRequest, ReviewRunIdGenerator,
    },
};
use serde::Deserialize;
use sha2::{Digest, Sha256 as Sha256Hasher};

const DEFAULT_CONFIG: &str = r#"source:
  path: ./vault
  id: local-vault
state:
  path: ./.mineral
git:
  repository: ./publication
  remote: origin
  reference: refs/heads/main
  author_name: Mineral Publisher
  author_email: publisher@example.invalid
  message: Publish Mineral content
review:
  api_base_url: https://api.deepseek.com
  markdown_model: deepseek-flash
  asset_model: deepseek-flash
  api_key_env: MINERAL_DEEPSEEK_API_KEY
  timeout_seconds: 45
  markdown_concurrency: 4
  asset_concurrency: 2
"#;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    source: SourceConfig,
    state: StateConfig,
    git: GitConfig,
    review: ReviewConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceConfig {
    path: PathBuf,
    id: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateConfig {
    path: PathBuf,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitConfig {
    repository: PathBuf,
    remote: String,
    reference: String,
    author_name: String,
    author_email: String,
    message: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewConfig {
    api_base_url: String,
    markdown_model: String,
    asset_model: String,
    api_key_env: String,
    timeout_seconds: u64,
    #[serde(default = "default_markdown_concurrency")]
    markdown_concurrency: usize,
    #[serde(default = "default_asset_concurrency")]
    asset_concurrency: usize,
}

fn default_markdown_concurrency() -> usize {
    4
}

fn default_asset_concurrency() -> usize {
    2
}

struct Workspace {
    config: Config,
    config_path: PathBuf,
}

impl Workspace {
    fn load(path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let bytes = fs::read_to_string(&path)?;
        let mut config: Config = serde_yaml_ng::from_str(&bytes)?;
        if config.review.markdown_concurrency == 0 || config.review.asset_concurrency == 0 {
            return Err("review concurrency must be at least 1".into());
        }
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        config.source.path = absolute(base, &config.source.path)?;
        config.state.path = absolute(base, &config.state.path)?;
        config.git.repository = absolute(base, &config.git.repository)?;
        Ok(Self {
            config,
            config_path: path,
        })
    }
    fn cas(&self) -> PathBuf {
        self.config.state.path.join("cas")
    }
    fn document_db(&self) -> PathBuf {
        self.config.state.path.join("document-reviews.sqlite3")
    }
    fn asset_db(&self) -> PathBuf {
        self.config.state.path.join("asset-reviews.sqlite3")
    }
    fn human_db(&self) -> PathBuf {
        self.config.state.path.join("human-reviews.sqlite3")
    }
    fn publish_db(&self) -> PathBuf {
        self.config.state.path.join("publish-runs.sqlite3")
    }
    fn observation_db(&self) -> PathBuf {
        self.config.state.path.join("remote-observations.sqlite3")
    }
}

fn absolute(base: &Path, value: &Path) -> Result<PathBuf, Box<dyn Error>> {
    Ok(if value.is_absolute() {
        value.to_path_buf()
    } else {
        env::current_dir()?.join(base).join(value)
    })
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    let config_path = if args.first().is_some_and(|arg| arg == "--config") {
        if args.len() < 2 {
            return Err("--config requires a path".into());
        }
        let value = PathBuf::from(args.remove(1));
        args.remove(0);
        value
    } else {
        PathBuf::from("mineral.yaml")
    };
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    match command {
        "init" => init(&config_path),
        "publish" => publish(Workspace::load(config_path)?),
        "status" => status(Workspace::load(config_path)?),
        "doctor" => doctor(Workspace::load(config_path)?),
        "review" => review(Workspace::load(config_path)?, &args[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => Err(format!("unknown command: {command}").into()),
    }
}

fn print_help() {
    println!(
        "Mineral Publisher\n\nUsage:\n  mineral [--config PATH] init\n  mineral [--config PATH] publish\n  mineral [--config PATH] status\n  mineral [--config PATH] review list\n  mineral [--config PATH] review show <document:ID|asset:ID>\n  mineral [--config PATH] review approve <document:ID|asset:ID>\n  mineral [--config PATH] review reject <document:ID|asset:ID>\n  mineral [--config PATH] doctor"
    );
}

fn init(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        return Err(format!("configuration already exists: {}", path.display()).into());
    }
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, DEFAULT_CONFIG)?;
    let workspace = Workspace::load(path.to_path_buf())?;
    fs::create_dir_all(&workspace.config.source.path)?;
    fs::create_dir_all(&workspace.config.state.path)?;
    fs::create_dir_all(workspace.cas())?;
    open_stores(&workspace)?;
    println!(
        "Initialized Mineral workspace\n  config: {}\n  state: {}\n  source: {}",
        workspace.config_path.display(),
        workspace.config.state.path.display(),
        workspace.config.source.path.display()
    );
    Ok(())
}

fn open_stores(workspace: &Workspace) -> Result<(), Box<dyn Error>> {
    SqliteReviewRunStore::open(workspace.document_db())?;
    SqliteAssetReviewRunStore::open(workspace.asset_db())?;
    SqliteHumanReviewStore::open(workspace.human_db())?;
    SqlitePublishRunStore::open(workspace.publish_db())?;
    SqliteRemoteObservationStore::open(workspace.observation_db())?;
    Ok(())
}

fn snapshot(workspace: &Workspace, store: &LocalContentStore) -> Result<Snapshot, Box<dyn Error>> {
    let source_id = SourceId::new(workspace.config.source.id.clone())?;
    let source = LocalSource::new(
        &workspace.config.source.path,
        source_id.clone(),
        store.clone(),
    );
    let provisional = source.snapshot(SnapshotId::new(1)?, SystemTime::now())?;
    let mut hasher = Sha256Hasher::new();
    hasher.update(source_id.as_str().as_bytes());
    for file in provisional.files() {
        hasher.update(file.path().as_str().as_bytes());
        hasher.update(file.size().to_le_bytes());
        hasher.update(file.sha256().as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let id = sqlite_positive_id(u64::from_be_bytes(bytes));
    source
        .snapshot(SnapshotId::new(id)?, SystemTime::now())
        .map_err(Into::into)
}

fn publish(workspace: Workspace) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&workspace.config.state.path)?;
    let content_store = LocalContentStore::new(workspace.cas());
    eprintln!("[1/4] Creating immutable Snapshot...");
    let snapshot = snapshot(&workspace, &content_store)?;
    eprintln!(
        "[1/4] Snapshot {} contains {} files.",
        snapshot.id().get(),
        snapshot.files().len()
    );
    let document_runs = SqliteReviewRunStore::open(workspace.document_db())?;
    let asset_runs = SqliteAssetReviewRunStore::open(workspace.asset_db())?;
    let human = SqliteHumanReviewStore::open(workspace.human_db())?;
    let publish_runs = SqlitePublishRunStore::open(workspace.publish_db())?;
    let observations = SqliteRemoteObservationStore::open(workspace.observation_db())?;
    let markdown_reviewer = LazyMarkdownReviewer {
        config: workspace.config.review.clone(),
        store: content_store.clone(),
        completed: AtomicUsize::new(0),
        reviewer: OnceLock::new(),
    };
    let asset_reviewer = LazyAssetReviewer {
        config: workspace.config.review.clone(),
        store: content_store.clone(),
        completed: AtomicUsize::new(0),
        reviewer: OnceLock::new(),
    };
    let markdown_policy = PolicyIdentity::new(
        format!("deepseek:{}", workspace.config.review.markdown_model),
        MARKDOWN_REVIEWER_PROMPT_VERSION,
        markdown_contract_hash(&workspace.config.review)?,
    )?;
    let asset_policy = PolicyIdentity::new(
        format!("deepseek:{}", workspace.config.review.asset_model),
        ASSET_REVIEWER_PROMPT_VERSION,
        asset_contract_hash(&workspace.config.review)?,
    )?;
    let request = PublicationApplicationRequest {
        snapshot: &snapshot,
        markdown_policy: &markdown_policy,
        asset_policy: &asset_policy,
        repository: &workspace.config.git.repository,
        target: PublicationTarget::new(
            &workspace.config.git.remote,
            &workspace.config.git.reference,
        )?,
        commit_metadata: &GitCommitMetadata::new(
            &workspace.config.git.author_name,
            &workspace.config.git.author_email,
            &workspace.config.git.message,
        )?,
        human_reviews: ExplicitHumanReviewSelection::default(),
        markdown_review_concurrency: workspace.config.review.markdown_concurrency,
        asset_review_concurrency: workspace.config.review.asset_concurrency,
    };
    let mut document_ids = RandomDocumentIds;
    let mut asset_ids = RandomAssetIds;
    let mut publish_ids = UuidPublishRunIdGenerator;
    let mut observation_ids = UuidRemoteObservationIdGenerator;
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
        &observations,
        &mut document_ids,
        &mut asset_ids,
        &mut publish_ids,
        &mut observation_ids,
    )?;
    eprintln!("[4/4] Publication workflow finished.");
    render_publication(outcome);
    Ok(())
}

fn render_publication(outcome: PublicationApplicationOutcome) {
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
            render_warnings(trace.markdown_reviews());
        }
        PublicationApplicationOutcome::Completed { trace, completed } => {
            let publication = completed.publication();
            let status = match publication.workflow() {
                PublicationWorkflowResult::NoopSatisfied { .. } => "noop",
                PublicationWorkflowResult::Published { .. }
                | PublicationWorkflowResult::AlreadyPublished { .. } => "published",
                PublicationWorkflowResult::RemoteChanged { .. } => "conflict",
                PublicationWorkflowResult::Indeterminate { .. } => "indeterminate",
                PublicationWorkflowResult::TargetMissing { .. } => "target_missing",
                PublicationWorkflowResult::PushFailedButRemoteUnchanged { .. }
                | PublicationWorkflowResult::RemoteUnchangedAfterSuccessfulPush { .. } => {
                    "not_published"
                }
            };
            println!(
                "Publication\n  status: {status}\n  run: {}\n  snapshot: {}\n  projection: {}\n  markdown: {}\n  assets: {}",
                publication.publish_run_id().get(),
                trace.snapshot().id().get(),
                completed.projection().projection_sha256(),
                completed.publication_set().markdown_paths().len(),
                completed.publication_set().asset_paths().len()
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
    let documents = SqliteReviewRunStore::open(workspace.document_db())?;
    let assets = SqliteAssetReviewRunStore::open(workspace.asset_db())?;
    let human = SqliteHumanReviewStore::open(workspace.human_db())?;
    let runs = SqlitePublishRunStore::open(workspace.publish_db())?;
    let pending_documents =
        HumanReviewResolution::list_pending_documents(&documents, &human)?.len();
    let pending_assets = HumanReviewResolution::list_pending_assets(&assets, &human)?.len();
    let all_runs = runs.list()?;
    let last = all_runs.last();
    println!(
        "Mineral status\n  source: {} ({})\n  state: {}\n  publication target: {} {}\n  last publication: {}\n  pending Markdown reviews: {}\n  pending Asset reviews: {}",
        workspace.config.source.id,
        workspace.config.source.path.display(),
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
    let documents = SqliteReviewRunStore::open(workspace.document_db())?;
    let assets = SqliteAssetReviewRunStore::open(workspace.asset_db())?;
    let human = SqliteHumanReviewStore::open(workspace.human_db())?;
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

fn parse_subject(value: &str) -> Result<HumanReviewSubject, Box<dyn Error>> {
    let (kind, id) = value
        .split_once(':')
        .ok_or("review ID must be document:ID or asset:ID")?;
    let id: u64 = id.parse()?;
    match kind {
        "document" => Ok(HumanReviewSubject::Document(ReviewRunId::new(id)?)),
        "asset" => Ok(HumanReviewSubject::Asset(AssetReviewRunId::new(id)?)),
        _ => Err("review ID must be document:ID or asset:ID".into()),
    }
}

fn show_review(
    subject: &str,
    documents: &SqliteReviewRunStore,
    assets: &SqliteAssetReviewRunStore,
    human: &SqliteHumanReviewStore,
) -> Result<(), Box<dyn Error>> {
    let subject = parse_subject(subject)?;
    match subject {
        HumanReviewSubject::Document(id) => {
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
        HumanReviewSubject::Asset(id) => {
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
    println!(
        "  human_resolution: {}",
        if human.get_for_subject(subject)?.is_some() {
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
    let subject = parse_subject(subject)?;
    if let Some(existing) = human.get_for_subject(subject)? {
        if existing.decision() == decision {
            println!("Review already {:?}.", decision);
            return Ok(());
        }
        return Err("review already has the opposite immutable human resolution".into());
    }
    let id = random_human_id()?;
    match subject {
        HumanReviewSubject::Document(run) => {
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
        HumanReviewSubject::Asset(run) => {
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
    check(
        "source",
        workspace.config.source.path.is_dir(),
        workspace.config.source.path.display().to_string(),
    );
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
    let remote = Command::new("git")
        .current_dir(&workspace.config.git.repository)
        .args([
            "ls-remote",
            "--exit-code",
            &workspace.config.git.remote,
            &workspace.config.git.reference,
        ])
        .output();
    check(
        "remote/ref",
        remote.is_ok_and(|output| output.status.success()),
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

fn markdown_config(
    config: &ReviewConfig,
    key: DeepSeekApiKey,
) -> Result<DeepSeekMarkdownReviewerConfig, Box<dyn Error>> {
    Ok(DeepSeekMarkdownReviewerConfig::new(
        &config.api_base_url,
        &config.markdown_model,
        key,
        Duration::from_secs(config.timeout_seconds),
        2 * 1024 * 1024,
        64 * 1024,
    )?)
}
fn asset_config(
    config: &ReviewConfig,
    key: DeepSeekApiKey,
) -> Result<DeepSeekAssetReviewerConfig, Box<dyn Error>> {
    Ok(DeepSeekAssetReviewerConfig::new(
        &config.api_base_url,
        key,
        Duration::from_secs(config.timeout_seconds),
        8 * 1024 * 1024,
        64 * 1024,
    )?
    .with_model(&config.asset_model)?)
}
fn markdown_contract_hash(config: &ReviewConfig) -> Result<Sha256, Box<dyn Error>> {
    let reviewer = DeepSeekMarkdownReviewer::new(
        markdown_config(config, DeepSeekApiKey::new("contract-only")?)?,
        LocalContentStore::new(env::temp_dir().join("mineral-contract-cas")),
    )?;
    Ok(reviewer.prompt_sha256())
}
fn asset_contract_hash(config: &ReviewConfig) -> Result<Sha256, Box<dyn Error>> {
    let reviewer = DeepSeekAssetReviewer::new(
        asset_config(config, DeepSeekApiKey::new("contract-only")?)?,
        LocalContentStore::new(env::temp_dir().join("mineral-contract-cas")),
    )?;
    Ok(reviewer.prompt_sha256())
}

struct LazyMarkdownReviewer {
    config: ReviewConfig,
    store: LocalContentStore,
    completed: AtomicUsize,
    reviewer: OnceLock<Result<DeepSeekMarkdownReviewer, String>>,
}
impl Reviewer for LazyMarkdownReviewer {
    fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError> {
        eprintln!("      Markdown review: {}", candidate.path());
        let reviewer = self.reviewer.get_or_init(|| {
            (|| -> Result<_, Box<dyn Error>> {
                let key = DeepSeekApiKey::from_env(&self.config.api_key_env)?;
                Ok(DeepSeekMarkdownReviewer::new(
                    markdown_config(&self.config, key)?,
                    self.store.clone(),
                )?)
            })()
            .map_err(|error| error.to_string())
        });
        let result = reviewer
            .as_ref()
            .map_err(|error| ReviewerError::new(format!("review provider unavailable: {error}")))
            .and_then(|reviewer| reviewer.review(candidate));
        let completed = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("      Markdown reviews completed: {completed}");
        result
    }
}
struct LazyAssetReviewer {
    config: ReviewConfig,
    store: LocalContentStore,
    completed: AtomicUsize,
    reviewer: OnceLock<Result<DeepSeekAssetReviewer, String>>,
}
impl AssetReviewer for LazyAssetReviewer {
    fn review(
        &self,
        candidate: &AssetReviewCandidate,
    ) -> Result<AssetReviewerReport, AssetReviewerError> {
        eprintln!("[3/4] Asset review: {}", candidate.path());
        let reviewer = self.reviewer.get_or_init(|| {
            (|| -> Result<_, Box<dyn Error>> {
                let key = DeepSeekApiKey::from_env(&self.config.api_key_env)?;
                Ok(DeepSeekAssetReviewer::new(
                    asset_config(&self.config, key)?,
                    self.store.clone(),
                )?)
            })()
            .map_err(|error| error.to_string())
        });
        let result = reviewer
            .as_ref()
            .map_err(|error| {
                AssetReviewerError::new(format!("review provider unavailable: {error}"))
            })
            .and_then(|reviewer| reviewer.review(candidate));
        let completed = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("      Asset reviews completed: {completed}");
        result
    }
}

struct RandomDocumentIds;
impl ReviewRunIdGenerator for RandomDocumentIds {
    type Error = Infallible;
    fn next_id(&mut self) -> Result<ReviewRunId, Self::Error> {
        Ok(ReviewRunId::new(random_u64()).expect("UUID-derived id is nonzero"))
    }
}
struct RandomAssetIds;
impl AssetReviewRunIdGenerator for RandomAssetIds {
    type Error = Infallible;
    fn next_id(&mut self) -> Result<AssetReviewRunId, Self::Error> {
        Ok(AssetReviewRunId::new(random_u64()).expect("UUID-derived id is nonzero"))
    }
}
fn random_human_id() -> Result<HumanReviewId, HumanReviewRecordError> {
    HumanReviewId::new(random_u64())
}
fn random_u64() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    sqlite_positive_id(u64::from_be_bytes(
        bytes[..8].try_into().expect("fixed UUID width"),
    ))
}

/// SQLite INTEGER is signed even though the domain IDs are represented as
/// `u64`. Keep every production-generated identity in the common positive
/// range so persistence can never reject a valid generated ID.
fn sqlite_positive_id(value: u64) -> u64 {
    (value & i64::MAX as u64).max(1)
}

#[cfg(test)]
mod tests {
    use super::{Config, DEFAULT_CONFIG, sqlite_positive_id};

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
}
