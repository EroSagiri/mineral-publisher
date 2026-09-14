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

use mineral_core::backup::{
    BackupExecutionOutcome, BackupRunId, BackupRunStore, TypeFirstBackupRepresentationPolicy,
    verify_backup,
};
use mineral_core::source::{DEFAULT_MAX_SCAN_ATTEMPTS, stabilize_scan};
use mineral_publisher::{
    asset::{
        AssetPublicationOutcome, ConfiguredAssetTarget, R2ObjectStore, R2ObjectStoreConfig,
        R2SecretKey, UuidAssetObservationIdGenerator,
    },
    backup::{
        application::{
            BackupApplicationError, BackupApplicationOutcome, BackupApplicationRequest,
            BackupRunIdGenerator, run_backup,
        },
        git_backup::{GitBackupRepository, backup_commit_metadata, observe_backup_ref},
        lfs_http::{LfsHttpConfig, LfsHttpRemote, LfsToken},
    },
    domain::{Sha256, Snapshot, SnapshotFile, SnapshotId, SourceId, TimestampMillis},
    policy::{
        PolicyIdentity, ReviewCandidate, ReviewRunId, ReviewRunStore, Reviewer, ReviewerError,
        ReviewerReport,
    },
    publisher::{
        GitCommitMetadata, GitCommitOid, GitPublicationExecution, GitRefTarget, GitRemoteAdapter,
        PublishRunStore, PublishTargetId, RemoteRefState, UuidPublishRunIdGenerator,
        UuidRemoteObservationIdGenerator,
    },
    reviewer::{
        ASSET_REVIEWER_PROMPT_VERSION, DeepSeekApiKey, DeepSeekAssetReviewer,
        DeepSeekAssetReviewerConfig, DeepSeekMarkdownReviewer, DeepSeekMarkdownReviewerConfig,
        MARKDOWN_REVIEWER_PROMPT_VERSION,
    },
    runtime::{HostAssetReviews, HostMarkdownReviews},
    source::LocalSource,
    source::r2::{R2Source, R2SourcePrefix},
    storage::{
        LocalContentStore, SqliteAssetObservationStore, SqliteAssetReviewRunStore,
        SqliteBackupRunStore, SqliteDeliveryProjectionStore, SqliteHumanReviewStore,
        SqlitePublishRunStore, SqliteRemoteObservationStore, SqliteReviewRunStore,
        SqliteSourceMaterializationStore,
    },
    workflow::{
        ASSET_OBJECT_KEY_PREFIX, AssetDeliveryConfig, AssetReviewCandidate, AssetReviewRunId,
        AssetReviewRunIdGenerator, AssetReviewRunStore, AssetReviewer, AssetReviewerError,
        AssetReviewerReport, ExplicitHumanReviewSelection, HumanReviewAttempt, HumanReviewDecision,
        HumanReviewId, HumanReviewRecordError, HumanReviewResolution, PublicExclusionRules,
        PublicationApplication, PublicationApplicationOutcome, PublicationApplicationRequest,
        ReviewRunIdGenerator,
    },
};
use serde::Deserialize;
use sha2::{Digest, Sha256 as Sha256Hasher};

const DEFAULT_CONFIG: &str = r#"source:
  # One active source. `type` is optional and defaults to a local directory tree, so
  # every configuration written before R2 sources existed keeps working unchanged.
  type: local
  id: local-vault
  path: ./vault
  # An R2 source instead reads one managed prefix of a bucket. It needs a non-empty
  # prefix (never the whole bucket) and the same credential mechanism as the asset
  # target: the secret is named here, never written here.
  # type: r2
  # r2:
  #   endpoint: https://<account>.r2.cloudflarestorage.com
  #   bucket: mineral-vault
  #   prefix: vault/
  #   access_key_id: <access key id>
  #   secret_access_key_env: MINERAL_R2_SECRET_ACCESS_KEY
  #   region: auto
  #   timeout_seconds: 300
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
public:
  # Source paths that stay in the Snapshot and in the content store but never enter
  # the public candidate set: no privacy scan, no review, no publication.
  exclude: []
  # exclude:
  #   - "private/**"
  #   - "drafts/**"
  #   - "secret.md"
  #   - "notes/internal.md"
  #   - "**/*.tmp"
review:
  api_base_url: https://api.deepseek.com
  markdown_model: deepseek-flash
  asset_model: deepseek-flash
  api_key_env: MINERAL_DEEPSEEK_API_KEY
  timeout_seconds: 45
  markdown_concurrency: 4
  asset_concurrency: 2
# An optional private backup of every Snapshot, byte-faithful and restorable.
# It is absent here because every workspace worked before backups existed and
# must keep working unchanged. Credentials are named here, never written here.
# backup:
#   enabled: true
#   git:
#     repository: ./backup-repo
#     remote: origin
#     branch: refs/heads/mineral-backup
#     author_name: Mineral Backup
#     author_email: backup@example.invalid
#     message: Backup knowledge snapshot
#   lfs:
#     # Required whenever the backup is enabled: binary objects live in Git LFS.
#     enabled: true
#     # Omit batch_url to derive it from the Git remote URL, or name the endpoint.
#     batch_url: https://github.com/<owner>/<repo>.git/info/lfs
#     username_env: MINERAL_BACKUP_LFS_USER
#     token_env: MINERAL_BACKUP_LFS_TOKEN
#     timeout_seconds: 300
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
    /// Which source paths the public publication may consider. Absent means the
    /// scope excludes nothing, which is exactly how every earlier workspace behaved.
    #[serde(default)]
    public: Option<PublicConfig>,
    /// The optional private backup target. Absent means this workspace never backs
    /// up, which is exactly how every workspace behaved before backups existed.
    #[serde(default)]
    backup: Option<BackupConfig>,
}

/// Which kind of namespace one workspace reads its source from.
///
/// It defaults to a local directory tree, which is what every workspace written
/// before R2 sources existed means.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SourceType {
    #[default]
    Local,
    R2,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceConfig {
    id: String,
    #[serde(default, rename = "type")]
    kind: Option<SourceType>,
    /// The local source root. Required for a local source, and refused for an R2
    /// source, so a workspace cannot silently keep reading a directory.
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    r2: Option<SourceR2Config>,
}

impl SourceConfig {
    fn kind(&self) -> SourceType {
        self.kind.unwrap_or_default()
    }
}

/// The connection one R2 source reads through.
///
/// The shape mirrors `assets.r2` on purpose: the same endpoint, bucket and
/// credential mechanism reach the same kind of store, whether this engine is
/// reading source objects or writing published ones.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceR2Config {
    endpoint: String,
    bucket: String,
    /// The one managed namespace this source reads. Required and non-empty.
    prefix: String,
    access_key_id: String,
    secret_access_key_env: String,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
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
/// The public publication scope.
///
/// The rules are validated while the workspace is loaded, so an unusable pattern
/// stops the run before it can publish anything, and the canonical rules are what
/// the publication records as its provenance.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicConfig {
    #[serde(default)]
    exclude: Vec<String>,
}

/// Defaults for a backup commit identity the configuration does not spell out.
const DEFAULT_BACKUP_AUTHOR_NAME: &str = "Mineral Backup";
const DEFAULT_BACKUP_AUTHOR_EMAIL: &str = "backup@example.invalid";
const DEFAULT_BACKUP_MESSAGE: &str = "Backup knowledge snapshot";
/// How long one backup LFS request may take when the configuration stays silent.
const DEFAULT_BACKUP_LFS_TIMEOUT_SECONDS: u64 = 300;

/// The optional private backup target.
///
/// A backup stores a byte-faithful restorable copy of every Snapshot in a Git
/// repository, with binary objects in Git LFS. The section is optional and defaults
/// to disabled, so a workspace that never mentions it behaves exactly as it did
/// before backups existed, and a section present only to document a future target
/// cannot start one by accident.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupConfig {
    /// Whether this workspace backs up at all. Validation and execution only ever
    /// happen for an enabled section; a disabled one is inert.
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    git: Option<BackupGitConfig>,
    #[serde(default)]
    lfs: Option<BackupLfsConfig>,
}

/// The Git repository one backup writes to.
///
/// Every field is optional at the type level because a disabled section may omit
/// them; an enabled section requires the repository, and the remote, branch and
/// commit identity are validated or defaulted while the workspace loads.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupGitConfig {
    /// The local Git repository holding the backup ref. Required and absolutized
    /// when the backup is enabled, exactly like `git.repository`.
    #[serde(default)]
    repository: Option<PathBuf>,
    #[serde(default)]
    remote: Option<String>,
    /// The fully qualified destination ref, for example `refs/heads/mineral-backup`.
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    author_name: Option<String>,
    #[serde(default)]
    author_email: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// The Git LFS endpoint binary backup objects are stored through.
///
/// Only the *names* of the credential variables live here: the username and token
/// values are read from the environment when a backup runs and never reach a
/// configuration field, a durable record or a report.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupLfsConfig {
    #[serde(default)]
    enabled: bool,
    /// The endpoint root. Absent means it is derived from the Git remote URL.
    #[serde(default)]
    batch_url: Option<String>,
    /// The name of the environment variable holding the LFS username.
    #[serde(default)]
    username_env: Option<String>,
    /// The name of the environment variable holding the LFS token.
    #[serde(default)]
    token_env: Option<String>,
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

#[derive(Debug)]
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
        // One source, chosen explicitly. A local source still requires its root, and
        // an R2 source requires a managed prefix and must not name a local root:
        // whichever one is configured decides where the Snapshot comes from.
        match config.source.kind() {
            SourceType::Local => {
                if config.source.r2.is_some() {
                    return Err(
                        "source.r2 is configured but source.type is local; set type: r2".into(),
                    );
                }
                let Some(source_path) = config.source.path.clone() else {
                    return Err("source.path is required for a local source".into());
                };
                config.source.path = Some(absolute(base, &source_path)?);
            }
            SourceType::R2 => {
                if config.source.path.is_some() {
                    return Err(
                        "source.path is not used by an R2 source; configure source.r2 instead"
                            .into(),
                    );
                }
                let r2 = config
                    .source
                    .r2
                    .as_ref()
                    .ok_or("source.type is r2 but source.r2 is not configured")?;
                // The prefix is validated, and stored back in canonical form, before
                // anything reads the namespace: an unusable namespace must stop the
                // run, not a listing in the middle of one.
                let prefix = R2SourcePrefix::new(&r2.prefix)
                    .map_err(|error| format!("source.r2.prefix is unusable: {error}"))?;
                config.source.r2.as_mut().expect("checked above").prefix =
                    prefix.as_str().to_owned();
            }
        }
        config.state.path = absolute(base, &config.state.path)?;
        config.git.repository = absolute(base, &config.git.repository)?;
        // Public scope is validated with the rest of the configuration: an unusable
        // rule must be reported before a publication is under way, not during one.
        if let Some(public) = &config.public {
            PublicExclusionRules::new(public.exclude.clone())
                .map_err(|error| format!("public.exclude contains an unusable rule: {error}"))?;
        }
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
        // The private backup target is validated once, while the workspace is
        // loaded, so an unusable destination stops the run before a Snapshot is even
        // taken. A disabled section is inert: it may omit everything, which is what
        // makes `enabled: false` a safe way to keep a target documented.
        if let Some(backup) = &mut config.backup
            && backup.enabled
        {
            let git = backup
                .git
                .as_mut()
                .ok_or("backup.git must be configured when backup is enabled")?;
            let repository = git
                .repository
                .clone()
                .filter(|path| !path.as_os_str().is_empty())
                .ok_or("backup.git.repository is required when backup is enabled")?;
            git.repository = Some(absolute(base, &repository)?);
            let remote = git.remote.as_deref().unwrap_or_default();
            if remote.trim().is_empty() || remote.contains(['\0', '\n', '\r']) {
                return Err("backup.git.remote must be a non-empty name".into());
            }
            let branch = git.branch.as_deref().unwrap_or_default();
            if branch.trim().is_empty() {
                return Err("backup.git.branch must be non-empty".into());
            }
            // The branch is the one ref a backup compare-and-swaps, so it is
            // validated with the same rule the publisher applies to its own ref.
            GitRefTarget::new(remote, branch).map_err(|error| {
                format!("backup.git.branch must be a fully qualified safe ref: {error}")
            })?;
            // A backup always stores binary objects through Git LFS, so the engine
            // cannot run one without an endpoint. That is a configuration fact, not a
            // run-time discovery: a workspace that could never complete a backup is
            // refused while it is loaded, exactly like an unusable publication target.
            let lfs = backup.lfs.as_ref().filter(|lfs| lfs.enabled).ok_or(
                "backup.lfs must be configured and enabled when backup is enabled; \
                 binary objects are stored in Git LFS",
            )?;
            // Only the variable *names* are validated here. The credential values are
            // read from the environment at run time and never stored, recorded or
            // printed.
            if lfs
                .username_env
                .as_deref()
                .is_none_or(|name| name.trim().is_empty())
            {
                return Err(
                    "backup.lfs.username_env must be non-empty when backup.lfs is enabled".into(),
                );
            }
            if lfs
                .token_env
                .as_deref()
                .is_none_or(|name| name.trim().is_empty())
            {
                return Err(
                    "backup.lfs.token_env must be non-empty when backup.lfs is enabled".into(),
                );
            }
            if let Some(url) = &lfs.batch_url
                && !is_absolute_http_url(url)
            {
                return Err(
                    format!("backup.lfs.batch_url must be an absolute http(s) URL: {url}").into(),
                );
            }
        }
        // A source and a publication target that share one namespace on one bucket
        // would make this engine read its own output back as input. That is refused
        // here, before any publication can create the loop.
        config.check_namespace_overlap()?;
        Ok(Self {
            config,
            config_path: path,
        })
    }
    /// The configured source kind.
    fn source_kind(&self) -> SourceType {
        self.config.source.kind()
    }

    /// The local source root, refusing an R2 workspace.
    fn local_source_path(&self) -> Result<&Path, Box<dyn Error>> {
        self.config
            .source
            .path
            .as_deref()
            .ok_or_else(|| "this workspace reads an R2 source and has no local source path".into())
    }

    /// Where a human-readable description of the configured source comes from.
    fn source_description(&self) -> String {
        match self.source_kind() {
            SourceType::Local => self
                .config
                .source
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unconfigured local source>".to_owned()),
            SourceType::R2 => match &self.config.source.r2 {
                Some(r2) => format!("r2:{}/{}/{}", r2.endpoint, r2.bucket, r2.prefix),
                None => "<unconfigured r2 source>".to_owned(),
            },
        }
    }

    /// Builds the configured R2 source reader, reading its secret from the
    /// environment. The secret never reaches the engine or a durable record.
    fn r2_source(&self, store: LocalContentStore) -> Result<R2Source, Box<dyn Error>> {
        let r2 = self
            .config
            .source
            .r2
            .as_ref()
            .ok_or("source.type is r2 but source.r2 is not configured")?;
        let prefix = R2SourcePrefix::new(&r2.prefix)
            .map_err(|error| format!("source.r2.prefix is unusable: {error}"))?;
        let mut config = R2ObjectStoreConfig::new(
            r2.endpoint.clone(),
            r2.bucket.clone(),
            r2.access_key_id.clone(),
            R2SecretKey::new(env::var(&r2.secret_access_key_env).map_err(|_| {
                format!(
                    "source.r2.secret_access_key_env names {}, which is not set",
                    r2.secret_access_key_env
                )
            })?)?,
        )?;
        if let Some(region) = &r2.region {
            config = config.with_region(region.clone())?;
        }
        if let Some(seconds) = r2.timeout_seconds {
            config = config.with_timeout(Duration::from_secs(seconds));
        }
        let materializations =
            SqliteSourceMaterializationStore::open(self.source_materializations_db())?;
        Ok(R2Source::new(config, prefix, store, materializations)?)
    }

    fn source_materializations_db(&self) -> PathBuf {
        self.config
            .state
            .path
            .join("source-materializations.sqlite3")
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
    /// The validated, canonical public scope for this workspace.
    fn public_scope(&self) -> Result<PublicExclusionRules, Box<dyn Error>> {
        let Some(public) = &self.config.public else {
            return Ok(PublicExclusionRules::empty());
        };
        PublicExclusionRules::new(public.exclude.clone())
            .map_err(|error| format!("public.exclude contains an unusable rule: {error}").into())
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

    /// Where durable backup intents live, beside the publication runs.
    fn backup_db(&self) -> PathBuf {
        self.config.state.path.join("backup-runs.sqlite3")
    }

    /// Whether this workspace has an enabled backup target.
    ///
    /// Every backup command checks this first, so a workspace without a `backup:`
    /// section keeps behaving exactly as it did before backups existed.
    fn backup_enabled(&self) -> bool {
        self.config
            .backup
            .as_ref()
            .is_some_and(|backup| backup.enabled)
    }

    /// The enabled backup Git target, if this workspace has one.
    fn backup_git(&self) -> Result<&BackupGitConfig, Box<dyn Error>> {
        self.config
            .backup
            .as_ref()
            .filter(|backup| backup.enabled)
            .and_then(|backup| backup.git.as_ref())
            .ok_or_else(|| "backup.git must be configured when backup is enabled".into())
    }

    /// The local backup repository, already absolutized while the workspace loaded.
    fn backup_git_repository(&self) -> Result<&Path, Box<dyn Error>> {
        self.backup_git()?
            .repository
            .as_deref()
            .ok_or_else(|| "backup.git.repository is required when backup is enabled".into())
    }

    /// The remote and fully qualified ref one backup compare-and-swaps.
    fn backup_target(&self) -> Result<GitRefTarget, Box<dyn Error>> {
        let git = self.backup_git()?;
        let remote = git
            .remote
            .as_deref()
            .ok_or("backup.git.remote must be non-empty when backup is enabled")?;
        let branch = git
            .branch
            .as_deref()
            .ok_or("backup.git.branch must be a fully qualified ref when backup is enabled")?;
        Ok(GitRefTarget::new(remote, branch)?)
    }

    /// The enabled LFS endpoint description, if this workspace has one.
    /// The enabled LFS block.
    ///
    /// `Workspace::load` already refuses an enabled backup without one, so this is a
    /// defensive guard rather than the place the requirement is discovered.
    fn backup_lfs(&self) -> Result<&BackupLfsConfig, Box<dyn Error>> {
        self.config
            .backup
            .as_ref()
            .filter(|backup| backup.enabled)
            .and_then(|backup| backup.lfs.as_ref())
            .filter(|lfs| lfs.enabled)
            .ok_or_else(|| {
                "backup.lfs must be enabled to run a backup; binary objects are stored in Git LFS"
                    .into()
            })
    }

    /// Builds the configured LFS endpoint, reading the credentials from the
    /// environment. This is the only place a backup credential is read, and it never
    /// reaches the engine, a durable record or a report: a missing variable is
    /// reported by name, and the value is never printed.
    fn backup_lfs_remote(&self) -> Result<LfsHttpRemote, Box<dyn Error>> {
        let lfs = self.backup_lfs()?;
        let username_env = lfs
            .username_env
            .as_deref()
            .ok_or("backup.lfs.username_env must be non-empty when backup.lfs is enabled")?;
        let token_env = lfs
            .token_env
            .as_deref()
            .ok_or("backup.lfs.token_env must be non-empty when backup.lfs is enabled")?;
        let username = env::var(username_env).map_err(|_| {
            format!("backup.lfs.username_env names {username_env}, which is not set")
        })?;
        let token = env::var(token_env)
            .map_err(|_| format!("backup.lfs.token_env names {token_env}, which is not set"))?;
        let batch_url = match &lfs.batch_url {
            Some(url) => url.clone(),
            None => self.derive_backup_batch_url()?,
        };
        let timeout = Duration::from_secs(
            lfs.timeout_seconds
                .unwrap_or(DEFAULT_BACKUP_LFS_TIMEOUT_SECONDS),
        );
        let config = LfsHttpConfig::new(batch_url, username, LfsToken::new(token)?, timeout)?;
        Ok(LfsHttpRemote::new(config)?)
    }

    /// Derives the LFS batch endpoint from the backup remote URL.
    ///
    /// A remote URL already names where the repository lives, and a Git LFS server
    /// serves the batch API from `<remote>/info/lfs`. A remote that is not an
    /// absolute http(s) URL cannot be derived and is refused rather than guessed.
    fn derive_backup_batch_url(&self) -> Result<String, Box<dyn Error>> {
        let git = self.backup_git()?;
        let remote = git
            .remote
            .as_deref()
            .ok_or("backup.git.remote must be non-empty when backup is enabled")?;
        let output = Command::new("git")
            .current_dir(self.backup_git_repository()?)
            .args(["remote", "get-url", remote])
            .output()
            .map_err(|error| format!("git is unavailable: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "could not read the URL of backup remote {remote}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if url.is_empty() {
            return Err(format!("backup remote {remote} has no URL").into());
        }
        Ok(format!("{}/info/lfs", url.trim_end_matches('/')))
    }

    /// The commit identity one backup freezes, using the configured values and the
    /// documented defaults for whichever were omitted.
    fn backup_commit_metadata(&self) -> Result<GitCommitMetadata, Box<dyn Error>> {
        let git = self.backup_git()?;
        Ok(backup_commit_metadata(
            git.author_name
                .as_deref()
                .unwrap_or(DEFAULT_BACKUP_AUTHOR_NAME),
            git.author_email
                .as_deref()
                .unwrap_or(DEFAULT_BACKUP_AUTHOR_EMAIL),
            git.message.as_deref().unwrap_or(DEFAULT_BACKUP_MESSAGE),
        )?)
    }
}

impl Config {
    /// Refuses a source and a publication target that share one namespace.
    ///
    /// Two namespaces overlap when one is a prefix of the other on the same bucket
    /// of the same endpoint. Reading published assets back as source objects, or
    /// publishing source objects on top of the source, is a loop this engine will
    /// not create for itself.
    fn check_namespace_overlap(&self) -> Result<(), Box<dyn Error>> {
        let Some(source) = self.source.r2.as_ref() else {
            return Ok(());
        };
        let Some(assets) = self.assets.as_ref().and_then(|assets| assets.r2.as_ref()) else {
            return Ok(());
        };
        let same_account = source.endpoint.eq_ignore_ascii_case(&assets.endpoint);
        if !same_account || source.bucket != assets.bucket {
            return Ok(());
        }
        let source_prefix = R2SourcePrefix::new(&source.prefix)?.as_str().to_owned();
        let publication_prefix = self.publication_namespace();
        if namespaces_overlap(&source_prefix, &publication_prefix) {
            return Err(format!(
                "source.r2 prefix {source_prefix} overlaps the publication namespace                  {publication_prefix} on bucket {}; a source must not read this engine's own output",
                source.bucket
            )
            .into());
        }
        Ok(())
    }

    /// Where published assets live inside the bucket, as a canonical prefix.
    fn publication_namespace(&self) -> String {
        format!("{ASSET_OBJECT_KEY_PREFIX}/")
    }
}

/// Whether one canonical object namespace contains, or is contained by, another.
///
/// Both prefixes are canonical and end in `/`, so the relation is a plain string
/// prefix test — and a namespace never overlaps itself by accident: two identical
/// prefixes are exactly the loop this check exists to refuse.
fn namespaces_overlap(left: &str, right: &str) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Whether one configured URL is an absolute http(s) URL with a host.
///
/// The LFS adapter enforces the same rule when it builds an endpoint; applying it
/// while the workspace loads means an unusable `batch_url` is reported before a
/// backup starts rather than in the middle of one.
fn is_absolute_http_url(value: &str) -> bool {
    reqwest::Url::parse(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
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
        "backup" => backup(Workspace::load(config_path)?, &args[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => Err(format!("unknown command: {command}").into()),
    }
}

fn print_help() {
    println!(
        "Mineral Publisher\n\nUsage:\n  mineral [--config PATH] init\n  mineral [--config PATH] publish\n  mineral [--config PATH] status\n  mineral [--config PATH] review list\n  mineral [--config PATH] review show <document:ID|asset:ID>\n  mineral [--config PATH] review approve <document:ID|asset:ID>\n  mineral [--config PATH] review reject <document:ID|asset:ID>\n  mineral [--config PATH] backup\n  mineral [--config PATH] backup status\n  mineral [--config PATH] backup verify\n  mineral [--config PATH] backup init\n  mineral [--config PATH] doctor"
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
    if workspace.source_kind() == SourceType::Local {
        fs::create_dir_all(workspace.local_source_path()?)?;
    }
    fs::create_dir_all(&workspace.config.state.path)?;
    fs::create_dir_all(workspace.cas())?;
    open_stores(&workspace)?;
    println!(
        "Initialized Mineral workspace\n  config: {}\n  state: {}\n  source: {}",
        workspace.config_path.display(),
        workspace.config.state.path.display(),
        workspace.source_description()
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
    SqliteSourceMaterializationStore::open(workspace.source_materializations_db())?;
    // The backup store only exists for a workspace that backs up, so a workspace
    // without a `backup:` section is never given an empty database it never uses.
    if workspace.backup_enabled() {
        SqliteBackupRunStore::open(workspace.backup_db())?;
    }
    Ok(())
}

fn snapshot(workspace: &Workspace, store: &LocalContentStore) -> Result<Snapshot, Box<dyn Error>> {
    let source_id = SourceId::new(workspace.config.source.id.clone())?;
    match workspace.source_kind() {
        SourceType::Local => {
            let source = LocalSource::new(
                workspace.local_source_path()?,
                source_id.clone(),
                store.clone(),
            );
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
            let source = workspace.r2_source(store.clone())?;
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
    let public_scope = workspace.public_scope()?;
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
        public_scope: &public_scope,
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
    let documents = SqliteReviewRunStore::open(workspace.document_db())?;
    let assets = SqliteAssetReviewRunStore::open(workspace.asset_db())?;
    let human = SqliteHumanReviewStore::open(workspace.human_db())?;
    let runs = SqlitePublishRunStore::open(workspace.publish_db())?;
    SqliteSourceMaterializationStore::open(workspace.source_materializations_db())?;
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
    let content_store = LocalContentStore::new(workspace.cas());
    let snapshot = snapshot(&workspace, &content_store)?;
    let store = SqliteBackupRunStore::open(workspace.backup_db())?;
    let repository_path = workspace.backup_git_repository()?;
    let repository = GitBackupRepository::new(repository_path, content_store.clone())?;
    let remote = GitRemoteAdapter::new(repository_path)?;
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
    let repository = workspace.backup_git_repository()?;
    match observe_backup_ref(repository, &target) {
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
    let store = SqliteBackupRunStore::open(workspace.backup_db())?;
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
    let repository_path = workspace.backup_git_repository()?;
    let repository =
        GitBackupRepository::new(repository_path, LocalContentStore::new(workspace.cas()))?;
    let remote = GitRemoteAdapter::new(repository_path)?;
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
    let repository = workspace.backup_git_repository()?;
    match observe_backup_ref(repository, &target)? {
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
            // `Command::output` closes stdin, so `git mktree` reads an empty list and
            // prints the empty tree the root commit points at.
            let tree = backup_git_stdout(repository, &["mktree"])?;
            let commit = backup_root_commit(repository, &tree, &metadata)?;
            push_backup_root_commit(repository, &target, &commit)?;
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

/// Runs a `git` command whose stdout is one value, failing closed on a non-zero exit.
fn backup_git_stdout(repository: &Path, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .map_err(|error| format!("git is unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Creates the empty root commit with the frozen backup identity.
fn backup_root_commit(
    repository: &Path,
    tree: &str,
    metadata: &GitCommitMetadata,
) -> Result<GitCommitOid, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(repository)
        .env("GIT_AUTHOR_NAME", metadata.author_name())
        .env("GIT_AUTHOR_EMAIL", metadata.author_email())
        .env("GIT_COMMITTER_NAME", metadata.author_name())
        .env("GIT_COMMITTER_EMAIL", metadata.author_email())
        .args(["commit-tree", tree, "-m", metadata.message()])
        .output()
        .map_err(|error| format!("git is unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(GitCommitOid::new(commit)?)
}

/// Pushes the bootstrap commit onto the backup branch.
fn push_backup_root_commit(
    repository: &Path,
    target: &GitRefTarget,
    commit: &GitCommitOid,
) -> Result<(), Box<dyn Error>> {
    let refspec = format!("{}:{}", commit.as_str(), target.destination_ref());
    let output = Command::new("git")
        .current_dir(repository)
        .args(["push", target.remote_name(), &refspec])
        .output()
        .map_err(|error| format!("git is unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git push failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

/// Allocates backup-attempt identities from UUID v4 entropy.
///
/// A backup id is frozen into the durable intent before any remote effect, so it
/// must be globally unique rather than sequential: a restart resuming an intent and
/// a fresh run must never collide on one identity.
struct UuidBackupRunIdGenerator;

impl BackupRunIdGenerator for UuidBackupRunIdGenerator {
    fn next_id(&self) -> BackupRunId {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&bytes[..8]);
        BackupRunId::new(sqlite_positive_id(u64::from_be_bytes(prefix)))
            .expect("UUID-derived id is nonzero")
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
    use std::{
        error::Error,
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::{
        Config, DEFAULT_CONFIG, LfsHttpConfig, LfsHttpRemote, LfsToken, LocalContentStore,
        SourceType, Workspace, backup, sqlite_positive_id,
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
        Workspace::load(path)
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
        assert!(
            workspace
                .r2_source(LocalContentStore::new(workspace.cas()))
                .is_err()
        );
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

        let error = match workspace.r2_source(LocalContentStore::new(workspace.cas())) {
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
        Workspace::load(path)
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
}
