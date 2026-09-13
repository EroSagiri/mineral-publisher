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
    asset::{
        AssetPublicationOutcome, ConfiguredAssetTarget, R2ObjectStore, R2ObjectStoreConfig,
        R2SecretKey, UuidAssetObservationIdGenerator,
    },
    domain::{Sha256, Snapshot, SnapshotId, SourceId},
    policy::{
        PolicyIdentity, ReviewCandidate, ReviewRunId, ReviewRunStore, Reviewer, ReviewerError,
        ReviewerReport,
    },
    publisher::{
        GitCommitMetadata, GitPublicationExecution, GitRefTarget, PublishRunStore, PublishTargetId,
        UuidPublishRunIdGenerator, UuidRemoteObservationIdGenerator,
    },
    reviewer::{
        ASSET_REVIEWER_PROMPT_VERSION, DeepSeekApiKey, DeepSeekAssetReviewer,
        DeepSeekAssetReviewerConfig, DeepSeekMarkdownReviewer, DeepSeekMarkdownReviewerConfig,
        MARKDOWN_REVIEWER_PROMPT_VERSION,
    },
    runtime::{HostAssetReviews, HostMarkdownReviews},
    source::LocalSource,
    storage::{
        LocalContentStore, SqliteAssetObservationStore, SqliteAssetReviewRunStore,
        SqliteDeliveryProjectionStore, SqliteHumanReviewStore, SqlitePublishRunStore,
        SqliteRemoteObservationStore, SqliteReviewRunStore,
    },
    workflow::{
        AssetDeliveryConfig, AssetReviewCandidate, AssetReviewRunId, AssetReviewRunIdGenerator,
        AssetReviewRunStore, AssetReviewer, AssetReviewerError, AssetReviewerReport,
        ExplicitHumanReviewSelection, HumanReviewAttempt, HumanReviewDecision, HumanReviewId,
        HumanReviewRecordError, HumanReviewResolution, PublicationApplication,
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
assets:
  public_base_url: https://assets.example.com
  # Exactly one target: the native store on this machine, or an S3-compatible
  # bucket. The secret key is never written here, only the variable that holds it.
  target_path: ./asset-target
  # r2:
  #   endpoint: https://<account>.r2.cloudflarestorage.com
  #   bucket: mineral-assets
  #   access_key_id: <access key id>
  #   secret_access_key_env: MINERAL_R2_SECRET_ACCESS_KEY
  #   region: auto
  #   timeout_seconds: 300
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
    /// Where delivered binary assets are served from. Optional in the file so
    /// `status`, `doctor` and `review` keep working for workspaces created before
    /// delivery existed; publication fails closed when it is absent.
    #[serde(default)]
    assets: Option<AssetsConfig>,
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
    /// Stable audit identity of the logical publication target. When it is
    /// absent the derived `{remote}:{reference}` identity is used, which is also
    /// what publication runs recorded before this field existed are migrated to.
    #[serde(default)]
    publish_target_id: Option<String>,
    author_name: String,
    author_email: String,
    message: String,
}

impl GitConfig {
    fn publish_target_id(&self) -> Result<PublishTargetId, Box<dyn Error>> {
        let identity = match &self.publish_target_id {
            Some(identity) => identity.clone(),
            None => format!("{}:{}", self.remote, self.reference),
        };
        Ok(PublishTargetId::new(identity)?)
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetsConfig {
    /// Absolute HTTPS base URL every published asset URL is built from.
    public_base_url: String,
    /// Where the native runtime places published objects.
    #[serde(default)]
    target_path: Option<PathBuf>,
    /// An S3-compatible bucket, for runtimes that publish to object storage.
    #[serde(default)]
    r2: Option<R2Config>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct R2Config {
    /// Absolute endpoint of the service, without the bucket and without a
    /// trailing slash.
    endpoint: String,
    bucket: String,
    access_key_id: String,
    /// The name of the environment variable that holds the secret access key.
    /// The key itself never belongs in a configuration file.
    secret_access_key_env: String,
    /// R2 accepts `auto`; a generic S3 endpoint may need its own region.
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
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
        if let Some(assets) = &mut config.assets {
            // One target, chosen explicitly: a workspace that names both (or
            // neither) must fail before a publication picks one for it.
            match (&assets.target_path, &assets.r2) {
                (Some(_), Some(_)) => {
                    return Err(
                        "assets.target_path and assets.r2 are both configured; choose one".into(),
                    );
                }
                (None, None) => {
                    return Err(
                        "assets must configure either target_path or r2 before publishing".into(),
                    );
                }
                _ => {}
            }
            if let Some(target_path) = &assets.target_path {
                assets.target_path = Some(absolute(base, target_path)?);
            }
        }
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
    fn delivery_db(&self) -> PathBuf {
        self.config.state.path.join("delivery-projections.sqlite3")
    }
    fn asset_delivery(&self) -> Result<AssetDeliveryConfig, Box<dyn Error>> {
        Ok(AssetDeliveryConfig::new(
            self.assets()?.public_base_url.clone(),
        )?)
    }
    fn assets(&self) -> Result<&AssetsConfig, Box<dyn Error>> {
        self.config.assets.as_ref().ok_or(
            "assets.public_base_url and one asset target must be configured before publishing"
                .into(),
        )
    }

    /// Builds the configured target, reading the secret access key from the
    /// environment. This is the only place an asset-target credential is read,
    /// and it never reaches the engine or a durable record.
    fn asset_target(&self) -> Result<ConfiguredAssetTarget, Box<dyn Error>> {
        let assets = self.assets()?;
        if let Some(target_path) = &assets.target_path {
            return Ok(ConfiguredAssetTarget::filesystem(target_path));
        }
        let r2 = assets
            .r2
            .as_ref()
            .ok_or("assets must configure either target_path or r2 before publishing")?;
        let mut config = R2ObjectStoreConfig::new(
            r2.endpoint.clone(),
            r2.bucket.clone(),
            r2.access_key_id.clone(),
            R2SecretKey::new(env::var(&r2.secret_access_key_env).map_err(|_| {
                format!(
                    "assets.r2.secret_access_key_env names {}, which is not set",
                    r2.secret_access_key_env
                )
            })?)?,
        )?;
        if let Some(region) = &r2.region {
            config = config.with_region(region.clone())?;
        }
        if let Some(seconds) = r2.timeout_seconds {
            config = config.with_timeout(std::time::Duration::from_secs(seconds));
        }
        Ok(ConfiguredAssetTarget::r2(R2ObjectStore::new(
            config,
            self.config.state.path.join("asset-spool"),
        )?))
    }
    fn asset_observations_db(&self) -> PathBuf {
        self.config.state.path.join("asset-observations.sqlite3")
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
    SqliteDeliveryProjectionStore::open(workspace.delivery_db())?;
    SqliteAssetObservationStore::open(workspace.asset_observations_db())?;
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
    let delivery_projections = SqliteDeliveryProjectionStore::open(workspace.delivery_db())?;
    let asset_target = workspace.asset_target()?;
    let asset_location = asset_target.description();
    let asset_observations = SqliteAssetObservationStore::open(workspace.asset_observations_db())?;
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
        target_id: workspace.config.git.publish_target_id()?,
        target: GitRefTarget::new(
            &workspace.config.git.remote,
            &workspace.config.git.reference,
        )?,
        commit_metadata: &GitCommitMetadata::new(
            &workspace.config.git.author_name,
            &workspace.config.git.author_email,
            &workspace.config.git.message,
        )?,
        human_reviews: ExplicitHumanReviewSelection::default(),
        asset_delivery: &workspace.asset_delivery()?,
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
    render_publication(outcome, &asset_location);
    Ok(())
}

fn render_publication(outcome: PublicationApplicationOutcome, asset_location: &str) {
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
            workspace.delivery_db(),
            workspace.asset_observations_db(),
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
    use super::{Config, DEFAULT_CONFIG, Workspace, sqlite_positive_id};

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
}
