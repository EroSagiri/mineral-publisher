//! A configuration that has been checked, normalized and frozen.
//!
//! Nothing downstream of this module sees a [`RawConfig`] that has not passed
//! through [`ValidatedConfig::from_raw`]. That is the whole point of the type:
//! every path is absolute, every ref is a legal fully qualified ref, every
//! namespace has been proven not to overlap another, and every rule that could
//! only fail later has already been refused. A use case never has to ask
//! "is this configuration usable?" — the type says it is.
//!
//! Validation is pure. It reads the file system's *current directory* to
//! absolutize relative paths and nothing else: no credential is resolved, no
//! network is touched, no adapter is built.

use std::{
    env,
    error::Error,
    fmt, io,
    ops::Deref,
    path::{Path, PathBuf},
};

use crate::{
    publisher::{GitCommitMetadata, GitRefTarget, PublishTargetId},
    source::r2::R2SourcePrefix,
    workflow::{ASSET_OBJECT_KEY_PREFIX, AssetDeliveryConfig, PublicExclusionRules},
};

use super::{
    model::{
        AssetsConfig, BackupConfig, BackupGitConfig, BackupLfsConfig, RawConfig, ReviewConfig,
        SourceType,
    },
    secrets::SecretName,
};

/// One workspace configuration that has been validated and normalized.
///
/// It dereferences to the model it was built from, so reading a setting is
/// `config.git.remote` exactly as it was when the model was the only type. What
/// dereferencing cannot do is *construct* one: the only way to hold a
/// `ValidatedConfig` is to have passed validation.
#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    model: RawConfig,
    path: PathBuf,
}

impl Deref for ValidatedConfig {
    type Target = RawConfig;

    fn deref(&self) -> &Self::Target {
        &self.model
    }
}

impl ValidatedConfig {
    /// Validates and normalizes a model read from `path`.
    ///
    /// The order of the checks is part of the contract: a workspace that is
    /// wrong in several ways is refused for the first reason a reader of the file
    /// would reach, and every message names the configuration key it is about.
    pub fn from_raw(model: RawConfig, path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let base = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let mut model = model;
        if model.review.markdown_concurrency == 0 || model.review.asset_concurrency == 0 {
            return Err(ConfigError::invalid(
                "review concurrency must be at least 1",
            ));
        }
        // One source, chosen explicitly. A local source still requires its root, and
        // an R2 source requires a managed prefix and must not name a local root:
        // whichever one is configured decides where the Snapshot comes from.
        match model.source.kind() {
            SourceType::Local => {
                if model.source.r2.is_some() {
                    return Err(ConfigError::invalid(
                        "source.r2 is configured but source.type is local; set type: r2",
                    ));
                }
                let Some(source_path) = model.source.path.clone() else {
                    return Err(ConfigError::invalid(
                        "source.path is required for a local source",
                    ));
                };
                model.source.path = Some(absolute(&base, &source_path)?);
            }
            SourceType::R2 => {
                if model.source.path.is_some() {
                    return Err(ConfigError::invalid(
                        "source.path is not used by an R2 source; configure source.r2 instead",
                    ));
                }
                let r2 = model.source.r2.as_ref().ok_or_else(|| {
                    ConfigError::invalid("source.type is r2 but source.r2 is not configured")
                })?;
                // The prefix is validated, and stored back in canonical form, before
                // anything reads the namespace: an unusable namespace must stop the
                // run, not a listing in the middle of one.
                let prefix = R2SourcePrefix::new(&r2.prefix).map_err(|error| {
                    ConfigError::invalid(format!("source.r2.prefix is unusable: {error}"))
                })?;
                model.source.r2.as_mut().expect("checked above").prefix =
                    prefix.as_str().to_owned();
            }
        }
        model.state.path = absolute(&base, &model.state.path)?;
        model.git.repository = absolute(&base, &model.git.repository)?;
        // Public scope is validated with the rest of the configuration: an unusable
        // rule must be reported before a publication is under way, not during one.
        if let Some(public) = &model.public {
            PublicExclusionRules::new(public.exclude.clone()).map_err(|error| {
                ConfigError::invalid(format!("public.exclude contains an unusable rule: {error}"))
            })?;
        }
        if let Some(assets) = &mut model.assets {
            // One target, chosen explicitly: a workspace that names both (or
            // neither) must fail before a publication picks one for it.
            match (&assets.target_path, &assets.r2) {
                (Some(_), Some(_)) => {
                    return Err(ConfigError::invalid(
                        "assets.target_path and assets.r2 are both configured; choose one",
                    ));
                }
                (None, None) => {
                    return Err(ConfigError::invalid(
                        "assets must configure either target_path or r2 before publishing",
                    ));
                }
                _ => {}
            }
            if let Some(target_path) = &assets.target_path {
                assets.target_path = Some(absolute(&base, target_path)?);
            }
        }
        // The private backup target is validated once, while the workspace is
        // loaded, so an unusable destination stops the run before a Snapshot is even
        // taken. A disabled section is inert: it may omit everything, which is what
        // makes `enabled: false` a safe way to keep a target documented.
        if let Some(backup) = &mut model.backup
            && backup.enabled
        {
            let git = backup.git.as_mut().ok_or_else(|| {
                ConfigError::invalid("backup.git must be configured when backup is enabled")
            })?;
            let repository = git
                .repository
                .clone()
                .filter(|path| !path.as_os_str().is_empty())
                .ok_or_else(|| {
                    ConfigError::invalid("backup.git.repository is required when backup is enabled")
                })?;
            git.repository = Some(absolute(&base, &repository)?);
            let remote = git.remote.as_deref().unwrap_or_default();
            if remote.trim().is_empty() || remote.contains(['\0', '\n', '\r']) {
                return Err(ConfigError::invalid(
                    "backup.git.remote must be a non-empty name",
                ));
            }
            let branch = git.branch.as_deref().unwrap_or_default();
            if branch.trim().is_empty() {
                return Err(ConfigError::invalid("backup.git.branch must be non-empty"));
            }
            // The branch is the one ref a backup compare-and-swaps, so it is
            // validated with the same rule the publisher applies to its own ref.
            GitRefTarget::new(remote, branch).map_err(|error| {
                ConfigError::invalid(format!(
                    "backup.git.branch must be a fully qualified safe ref: {error}"
                ))
            })?;
            // A backup always stores binary objects through Git LFS, so the engine
            // cannot run one without an endpoint. That is a configuration fact, not a
            // run-time discovery: a workspace that could never complete a backup is
            // refused while it is loaded, exactly like an unusable publication target.
            let lfs = backup
                .lfs
                .as_ref()
                .filter(|lfs| lfs.enabled)
                .ok_or_else(|| {
                    ConfigError::invalid(
                        "backup.lfs must be configured and enabled when backup is enabled; \
                     binary objects are stored in Git LFS",
                    )
                })?;
            // Only the variable *names* are validated here. The credential values are
            // read from the environment at run time and never stored, recorded or
            // printed.
            if lfs
                .username_env
                .as_deref()
                .is_none_or(|name| name.trim().is_empty())
            {
                return Err(ConfigError::invalid(
                    "backup.lfs.username_env must be non-empty when backup.lfs is enabled",
                ));
            }
            if lfs
                .token_env
                .as_deref()
                .is_none_or(|name| name.trim().is_empty())
            {
                return Err(ConfigError::invalid(
                    "backup.lfs.token_env must be non-empty when backup.lfs is enabled",
                ));
            }
            if let Some(url) = &lfs.batch_url
                && !is_absolute_http_url(url)
            {
                return Err(ConfigError::invalid(format!(
                    "backup.lfs.batch_url must be an absolute http(s) URL: {url}"
                )));
            }
        }
        // A source and a publication target that share one namespace on one bucket
        // would make this engine read its own output back as input. That is refused
        // here, before any publication can create the loop.
        check_namespace_overlap(&model)?;
        Ok(Self { model, path })
    }

    /// Where the configuration was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory relative paths in the file were resolved against.
    pub fn base_dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Which kind of source this workspace reads.
    pub fn source_kind(&self) -> SourceType {
        self.model.source.kind()
    }

    /// The local source root, refusing an R2 workspace.
    pub fn local_source_path(&self) -> Result<&Path, ConfigError> {
        self.model.source.path.as_deref().ok_or_else(|| {
            ConfigError::invalid("this workspace reads an R2 source and has no local source path")
        })
    }

    /// Where a human-readable description of the configured source comes from.
    pub fn source_description(&self) -> String {
        match self.source_kind() {
            SourceType::Local => self
                .model
                .source
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unconfigured local source>".to_owned()),
            SourceType::R2 => match &self.model.source.r2 {
                Some(r2) => format!("r2:{}/{}/{}", r2.endpoint, r2.bucket, r2.prefix),
                None => "<unconfigured r2 source>".to_owned(),
            },
        }
    }

    /// The validated, canonical public scope for this workspace.
    pub fn public_scope(&self) -> Result<PublicExclusionRules, ConfigError> {
        let Some(public) = &self.model.public else {
            return Ok(PublicExclusionRules::empty());
        };
        PublicExclusionRules::new(public.exclude.clone()).map_err(|error| {
            ConfigError::invalid(format!("public.exclude contains an unusable rule: {error}"))
        })
    }

    /// The asset delivery description, refusing a workspace without a target.
    pub fn asset_delivery(&self) -> Result<AssetDeliveryConfig, ConfigError> {
        let assets = self.assets()?;
        AssetDeliveryConfig::new(assets.public_base_url.clone())
            .map_err(|error| ConfigError::invalid(error.to_string()))
    }

    /// The configured asset target, refusing a workspace without one.
    pub fn assets(&self) -> Result<&AssetsConfig, ConfigError> {
        self.model.assets.as_ref().ok_or_else(|| {
            ConfigError::invalid(
                "assets.public_base_url and one asset target must be configured before publishing",
            )
        })
    }

    /// The stable identity of the logical publication target.
    pub fn publish_target_id(&self) -> Result<PublishTargetId, ConfigError> {
        self.model
            .git
            .publish_target_id()
            .map_err(ConfigError::invalid)
    }

    /// The ref one publication compare-and-swaps.
    pub fn publish_target(&self) -> Result<GitRefTarget, ConfigError> {
        GitRefTarget::new(&self.model.git.remote, &self.model.git.reference)
            .map_err(|error| ConfigError::invalid(error.to_string()))
    }

    /// The commit identity one publication freezes.
    pub fn publish_commit_metadata(&self) -> Result<GitCommitMetadata, ConfigError> {
        GitCommitMetadata::new(
            &self.model.git.author_name,
            &self.model.git.author_email,
            &self.model.git.message,
        )
        .map_err(|error| ConfigError::invalid(error.to_string()))
    }

    /// Whether this workspace has an enabled backup target.
    ///
    /// Every backup command checks this first, so a workspace without a `backup:`
    /// section keeps behaving exactly as it did before backups existed.
    pub fn backup_enabled(&self) -> bool {
        self.model
            .backup
            .as_ref()
            .is_some_and(|backup| backup.enabled)
    }

    /// The enabled backup section, if this workspace has one.
    pub fn backup(&self) -> Result<&BackupConfig, ConfigError> {
        self.model
            .backup
            .as_ref()
            .filter(|backup| backup.enabled)
            .ok_or_else(|| ConfigError::invalid("backup is not enabled for this workspace"))
    }

    /// The enabled backup Git target, if this workspace has one.
    pub fn backup_git(&self) -> Result<&BackupGitConfig, ConfigError> {
        self.model
            .backup
            .as_ref()
            .filter(|backup| backup.enabled)
            .and_then(|backup| backup.git.as_ref())
            .ok_or_else(|| {
                ConfigError::invalid("backup.git must be configured when backup is enabled")
            })
    }

    /// The local backup repository, already absolutized while the workspace loaded.
    pub fn backup_git_repository(&self) -> Result<&Path, ConfigError> {
        self.backup_git()?.repository.as_deref().ok_or_else(|| {
            ConfigError::invalid("backup.git.repository is required when backup is enabled")
        })
    }

    /// The remote and fully qualified ref one backup compare-and-swaps.
    pub fn backup_target(&self) -> Result<GitRefTarget, ConfigError> {
        let git = self.backup_git()?;
        let remote = git.remote.as_deref().ok_or_else(|| {
            ConfigError::invalid("backup.git.remote must be non-empty when backup is enabled")
        })?;
        let branch = git.branch.as_deref().ok_or_else(|| {
            ConfigError::invalid(
                "backup.git.branch must be a fully qualified ref when backup is enabled",
            )
        })?;
        GitRefTarget::new(remote, branch).map_err(|error| ConfigError::invalid(error.to_string()))
    }

    /// The enabled LFS endpoint description, if this workspace has one.
    ///
    /// `from_raw` already refuses an enabled backup without one, so this is a
    /// defensive guard rather than the place the requirement is discovered.
    pub fn backup_lfs(&self) -> Result<&BackupLfsConfig, ConfigError> {
        self.model
            .backup
            .as_ref()
            .filter(|backup| backup.enabled)
            .and_then(|backup| backup.lfs.as_ref())
            .filter(|lfs| lfs.enabled)
            .ok_or_else(|| {
                ConfigError::invalid(
                    "backup.lfs must be enabled to run a backup; binary objects are stored in Git LFS",
                )
            })
    }

    /// The two credential *names* a backup resolves through a secret provider.
    ///
    /// Returning names rather than values is what keeps validation pure: the
    /// values are resolved later, by the layer that is allowed to hold them.
    pub fn backup_lfs_credential_names(&self) -> Result<(SecretName, SecretName), ConfigError> {
        let lfs = self.backup_lfs()?;
        let username = lfs.username_env.as_deref().ok_or_else(|| {
            ConfigError::invalid(
                "backup.lfs.username_env must be non-empty when backup.lfs is enabled",
            )
        })?;
        let token = lfs.token_env.as_deref().ok_or_else(|| {
            ConfigError::invalid(
                "backup.lfs.token_env must be non-empty when backup.lfs is enabled",
            )
        })?;
        Ok((
            secret_name("backup.lfs.username_env", username)?,
            secret_name("backup.lfs.token_env", token)?,
        ))
    }

    /// The credential name an R2 source resolves.
    pub fn source_r2_secret_name(&self) -> Result<SecretName, ConfigError> {
        let name = self
            .model
            .source
            .r2
            .as_ref()
            .map(|r2| r2.secret_access_key_env.as_str())
            .ok_or_else(|| {
                ConfigError::invalid("source.type is r2 but source.r2 is not configured")
            })?;
        secret_name("source.r2.secret_access_key_env", name)
    }

    /// The credential name an asset target resolves.
    pub fn assets_r2_secret_name(&self) -> Result<SecretName, ConfigError> {
        let name = self
            .assets()?
            .r2
            .as_ref()
            .map(|r2| r2.secret_access_key_env.as_str())
            .ok_or_else(|| {
                ConfigError::invalid(
                    "assets must configure either target_path or r2 before publishing",
                )
            })?;
        secret_name("assets.r2.secret_access_key_env", name)
    }

    /// The credential name the review provider resolves.
    pub fn review_api_key_name(&self) -> Result<SecretName, ConfigError> {
        secret_name("review.api_key_env", &self.model.review.api_key_env)
    }

    /// The review settings, which every reviewer construction reads.
    pub fn review(&self) -> &ReviewConfig {
        &self.model.review
    }

    /// Where durable engine state lives.
    pub fn state_path(&self) -> &Path {
        &self.model.state.path
    }

    /// The content-addressed store root.
    pub fn cas(&self) -> PathBuf {
        self.state_path().join("cas")
    }

    /// Where one durable store lives, named by the file it occupies.
    pub fn state_file(&self, name: &str) -> PathBuf {
        self.state_path().join(name)
    }

    /// The durable Markdown review runs.
    pub fn document_db(&self) -> PathBuf {
        self.state_file("document-reviews.sqlite3")
    }

    /// The durable asset review runs.
    pub fn asset_db(&self) -> PathBuf {
        self.state_file("asset-reviews.sqlite3")
    }

    /// The durable human decisions.
    pub fn human_db(&self) -> PathBuf {
        self.state_file("human-reviews.sqlite3")
    }

    /// The durable publication runs.
    pub fn publish_db(&self) -> PathBuf {
        self.state_file("publish-runs.sqlite3")
    }

    /// The durable observations of the publication ref.
    pub fn observation_db(&self) -> PathBuf {
        self.state_file("remote-observations.sqlite3")
    }

    /// The durable delivery projections.
    pub fn delivery_db(&self) -> PathBuf {
        self.state_file("delivery-projections.sqlite3")
    }

    /// The durable observations of delivered assets.
    pub fn asset_observations_db(&self) -> PathBuf {
        self.state_file("asset-observations.sqlite3")
    }

    /// The durable record of materialized source objects.
    pub fn source_materializations_db(&self) -> PathBuf {
        self.state_file("source-materializations.sqlite3")
    }

    /// The durable backup intents.
    pub fn backup_db(&self) -> PathBuf {
        self.state_file("backup-runs.sqlite3")
    }
}

/// Parses one configured credential name, naming the key that carried it.
fn secret_name(key: &str, value: &str) -> Result<SecretName, ConfigError> {
    SecretName::new(value).map_err(|error| ConfigError::invalid(format!("{key}: {error}")))
}

/// Refuses a source and a publication target that share one namespace.
///
/// Two namespaces overlap when one is a prefix of the other on the same bucket
/// of the same endpoint. Reading published assets back as source objects, or
/// publishing source objects on top of the source, is a loop this engine will
/// not create for itself.
fn check_namespace_overlap(model: &RawConfig) -> Result<(), ConfigError> {
    let Some(source) = model.source.r2.as_ref() else {
        return Ok(());
    };
    let Some(assets) = model.assets.as_ref().and_then(|assets| assets.r2.as_ref()) else {
        return Ok(());
    };
    let same_account = source.endpoint.eq_ignore_ascii_case(&assets.endpoint);
    if !same_account || source.bucket != assets.bucket {
        return Ok(());
    }
    let source_prefix = R2SourcePrefix::new(&source.prefix)
        .map_err(|error| ConfigError::invalid(format!("source.r2.prefix is unusable: {error}")))?
        .as_str()
        .to_owned();
    let publication_prefix = format!("{ASSET_OBJECT_KEY_PREFIX}/");
    if namespaces_overlap(&source_prefix, &publication_prefix) {
        return Err(ConfigError::invalid(format!(
            "source.r2 prefix {source_prefix} overlaps the publication namespace                  {publication_prefix} on bucket {}; a source must not read this engine's own output",
            source.bucket
        )));
    }
    Ok(())
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

/// Resolves one configured path against the directory the configuration lives in.
fn absolute(base: &Path, value: &Path) -> Result<PathBuf, ConfigError> {
    Ok(if value.is_absolute() {
        value.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|source| ConfigError::CurrentDirectory { source })?
            .join(base)
            .join(value)
    })
}

/// Why a configuration could not be read, parsed or validated.
///
/// The variants separate "the file could not be read", "the file is not a
/// configuration" and "the configuration is not usable", because a caller
/// answers those three differently — and a web host maps them to different
/// responses without parsing a message.
#[derive(Debug)]
pub enum ConfigError {
    /// The configuration file could not be read.
    Read { path: PathBuf, source: io::Error },
    /// The configuration file is not valid in its own format.
    Format { path: PathBuf, message: String },
    /// The configuration is syntactically fine but not usable.
    Invalid { message: String },
    /// The process has no usable current directory to resolve paths against.
    CurrentDirectory { source: io::Error },
}

impl ConfigError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    /// The configuration key or file this error is about, when it names one.
    pub fn is_about(&self, key: &str) -> bool {
        match self {
            Self::Format { message, .. } | Self::Invalid { message } => message.contains(key),
            Self::Read { path, .. } => path.to_string_lossy().contains(key),
            Self::CurrentDirectory { .. } => false,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Format { path, message } => write!(formatter, "{}: {message}", path.display()),
            Self::Invalid { message } => formatter.write_str(message),
            Self::CurrentDirectory { source } => write!(formatter, "{source}"),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::CurrentDirectory { source } => Some(source),
            Self::Format { .. } | Self::Invalid { .. } => None,
        }
    }
}
