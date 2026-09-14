//! What a configuration file says, and nothing more.
//!
//! Every type here is a plain `serde` model: it carries the text a file wrote,
//! not a decision about whether that text is usable. `super::validate` turns one
//! of these into a [`super::ValidatedConfig`], and only that type is allowed to
//! reach the rest of the host. Keeping the two apart is what lets a test build a
//! configuration without a file system, and what makes "the file said X" a
//! different question from "X is allowed".

use std::path::PathBuf;

use serde::Deserialize;

/// The configuration a fresh workspace is born with, in YAML.
///
/// It is the documented shape of a workspace: every section, every default and
/// every credential *name* in one place, with the optional sections commented out
/// so that an untouched workspace behaves exactly as one written before they
/// existed.
pub const DEFAULT_CONFIG: &str = r#"source:
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

/// The same workspace, in TOML.
///
/// The two templates describe exactly the same configuration; only the surface
/// syntax differs. A configuration file's format is chosen by its extension, so a
/// workspace written in either language is loaded by the same validation.
pub const DEFAULT_CONFIG_TOML: &str = r#"# Mineral Publisher workspace.
# The YAML template in `mineral.yaml` describes the same file; either language
# loads through the same validation.

[source]
# One active source. `type` defaults to a local directory tree.
type = "local"
id = "local-vault"
path = "./vault"
# An R2 source instead reads one managed prefix of a bucket:
# type = "r2"
# [source.r2]
# endpoint = "https://<account>.r2.cloudflarestorage.com"
# bucket = "mineral-vault"
# prefix = "vault/"
# access_key_id = "<access key id>"
# secret_access_key_env = "MINERAL_R2_SECRET_ACCESS_KEY"
# region = "auto"
# timeout_seconds = 300

[state]
path = "./.mineral"

[git]
repository = "./publication"
remote = "origin"
reference = "refs/heads/main"
author_name = "Mineral Publisher"
author_email = "publisher@example.invalid"
message = "Publish Mineral content"

[assets]
public_base_url = "https://assets.example.com"
# Exactly one target: the native store on this machine, or an S3-compatible
# bucket. The secret key is never written here, only the variable that holds it.
target_path = "./asset-target"
# [assets.r2]
# endpoint = "https://<account>.r2.cloudflarestorage.com"
# bucket = "mineral-assets"
# access_key_id = "<access key id>"
# secret_access_key_env = "MINERAL_R2_SECRET_ACCESS_KEY"
# region = "auto"
# timeout_seconds = 300

[public]
# Source paths that stay in the Snapshot and in the content store but never enter
# the public candidate set: no privacy scan, no review, no publication.
exclude = []

[review]
api_base_url = "https://api.deepseek.com"
markdown_model = "deepseek-flash"
asset_model = "deepseek-flash"
api_key_env = "MINERAL_DEEPSEEK_API_KEY"
timeout_seconds = 45
markdown_concurrency = 4
asset_concurrency = 2

# An optional private backup of every Snapshot, byte-faithful and restorable.
# [backup]
# enabled = true
# [backup.git]
# repository = "./backup-repo"
# remote = "origin"
# branch = "refs/heads/mineral-backup"
# author_name = "Mineral Backup"
# author_email = "backup@example.invalid"
# message = "Backup knowledge snapshot"
# [backup.lfs]
# enabled = true
# batch_url = "https://github.com/<owner>/<repo>.git/info/lfs"
# username_env = "MINERAL_BACKUP_LFS_USER"
# token_env = "MINERAL_BACKUP_LFS_TOKEN"
# timeout_seconds = 300
"#;

/// One workspace, exactly as a file wrote it.
///
/// `deny_unknown_fields` is what makes a typo a refusal instead of a silently
/// ignored setting: a workspace that misspells `public_base_url` must not publish
/// with the one it did not mean.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub source: SourceConfig,
    pub state: StateConfig,
    pub git: GitConfig,
    pub review: ReviewConfig,
    /// Where delivered binary assets are served from. Optional in the file so
    /// `status`, `doctor` and `review` keep working for workspaces created before
    /// delivery existed; publication fails closed when it is absent.
    #[serde(default)]
    pub assets: Option<AssetsConfig>,
    /// Which source paths the public publication may consider. Absent means the
    /// scope excludes nothing, which is exactly how every earlier workspace behaved.
    #[serde(default)]
    pub public: Option<PublicConfig>,
    /// The optional private backup target. Absent means this workspace never backs
    /// up, which is exactly how every workspace behaved before backups existed.
    #[serde(default)]
    pub backup: Option<BackupConfig>,
}

/// Which kind of namespace one workspace reads its source from.
///
/// It defaults to a local directory tree, which is what every workspace written
/// before R2 sources existed means.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    #[default]
    Local,
    R2,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: Option<SourceType>,
    /// The local source root. Required for a local source, and refused for an R2
    /// source, so a workspace cannot silently keep reading a directory.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub r2: Option<SourceR2Config>,
}

impl SourceConfig {
    pub fn kind(&self) -> SourceType {
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
pub struct SourceR2Config {
    pub endpoint: String,
    pub bucket: String,
    /// The one managed namespace this source reads. Required and non-empty.
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key_env: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    pub repository: PathBuf,
    pub remote: String,
    pub reference: String,
    /// Stable audit identity of the logical publication target. When it is
    /// absent the derived `{remote}:{reference}` identity is used, which is also
    /// what publication runs recorded before this field existed are migrated to.
    #[serde(default)]
    pub publish_target_id: Option<String>,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
}

impl GitConfig {
    pub fn publish_target_id(&self) -> Result<crate::publisher::PublishTargetId, String> {
        let identity = match &self.publish_target_id {
            Some(identity) => identity.clone(),
            None => format!("{}:{}", self.remote, self.reference),
        };
        crate::publisher::PublishTargetId::new(identity).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetsConfig {
    /// Absolute HTTPS base URL every published asset URL is built from.
    pub public_base_url: String,
    /// Where the native runtime places published objects.
    #[serde(default)]
    pub target_path: Option<PathBuf>,
    /// An S3-compatible bucket, for runtimes that publish to object storage.
    #[serde(default)]
    pub r2: Option<R2Config>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct R2Config {
    /// Absolute endpoint of the service, without the bucket and without a
    /// trailing slash.
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    /// The name of the environment variable that holds the secret access key.
    /// The key itself never belongs in a configuration file.
    pub secret_access_key_env: String,
    /// R2 accepts `auto`; a generic S3 endpoint may need its own region.
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// The public publication scope.
///
/// The rules are validated while the workspace is loaded, so an unusable pattern
/// stops the run before it can publish anything, and the canonical rules are what
/// the publication records as its provenance.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicConfig {
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Defaults for a backup commit identity the configuration does not spell out.
pub const DEFAULT_BACKUP_AUTHOR_NAME: &str = "Mineral Backup";
pub const DEFAULT_BACKUP_AUTHOR_EMAIL: &str = "backup@example.invalid";
pub const DEFAULT_BACKUP_MESSAGE: &str = "Backup knowledge snapshot";
/// How long one backup LFS request may take when the configuration stays silent.
pub const DEFAULT_BACKUP_LFS_TIMEOUT_SECONDS: u64 = 300;

/// The optional private backup target.
///
/// A backup stores a byte-faithful restorable copy of every Snapshot in a Git
/// repository, with binary objects in Git LFS. The section is optional and defaults
/// to disabled, so a workspace that never mentions it behaves exactly as it did
/// before backups existed, and a section present only to document a future target
/// cannot start one by accident.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    /// Whether this workspace backs up at all. Validation and execution only ever
    /// happen for an enabled section; a disabled one is inert.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub git: Option<BackupGitConfig>,
    #[serde(default)]
    pub lfs: Option<BackupLfsConfig>,
}

/// The Git repository one backup writes to.
///
/// Every field is optional at the type level because a disabled section may omit
/// them; an enabled section requires the repository, and the remote, branch and
/// commit identity are validated or defaulted while the workspace loads.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupGitConfig {
    /// The local Git repository holding the backup ref. Required and absolutized
    /// when the backup is enabled, exactly like `git.repository`.
    #[serde(default)]
    pub repository: Option<PathBuf>,
    #[serde(default)]
    pub remote: Option<String>,
    /// The fully qualified destination ref, for example `refs/heads/mineral-backup`.
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub author_email: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The Git LFS endpoint binary backup objects are stored through.
///
/// Only the *names* of the credential variables live here: the username and token
/// values are read from the environment when a backup runs and never reach a
/// configuration field, a durable record or a report.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupLfsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The endpoint root. Absent means it is derived from the Git remote URL.
    #[serde(default)]
    pub batch_url: Option<String>,
    /// The name of the environment variable holding the LFS username.
    #[serde(default)]
    pub username_env: Option<String>,
    /// The name of the environment variable holding the LFS token.
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    pub api_base_url: String,
    pub markdown_model: String,
    pub asset_model: String,
    pub api_key_env: String,
    pub timeout_seconds: u64,
    #[serde(default = "default_markdown_concurrency")]
    pub markdown_concurrency: usize,
    #[serde(default = "default_asset_concurrency")]
    pub asset_concurrency: usize,
}

fn default_markdown_concurrency() -> usize {
    4
}

fn default_asset_concurrency() -> usize {
    2
}
