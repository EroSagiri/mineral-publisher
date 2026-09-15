//! The composition root: one validated configuration, one secret provider, and
//! the capabilities built from them.
//!
//! A use case is handed a `WorkspaceRuntime` and asks it for what it needs. It
//! never learns whether the source is a directory or a bucket, whether a store
//! is SQLite, or where a credential came from — those answers exist exactly
//! once, here.
//!
//! Wall-clock time and process credentials are runtime concerns for the same
//! reason: the layers above take them as inputs so that a test can supply its
//! own, which is what makes a use case testable without a network or an
//! environment.

use std::{ops::Deref, path::PathBuf, sync::Arc};

use crate::{
    asset::ConfiguredAssetTarget,
    backup::{git_backup::GitBackupRepository, lfs_http::LfsHttpRemote},
    config::{ConfigError, InlineSecretProvider, SecretName, SecretProvider, ValidatedConfig},
    domain::{Sha256, Snapshot, SourceId},
    publisher::{GitCommitMetadata, GitCommitOid, GitRefTarget, GitRemoteAdapter, RemoteRefState},
    source::{LocalSource, r2::R2Source},
    storage::{
        LocalContentStore, SqliteAssetObservationStore, SqliteAssetReviewRunStore,
        SqliteBackupRunStore, SqliteDeliveryProjectionStore, SqliteHumanReviewStore,
        SqlitePublishRunStore, SqliteRemoteObservationStore, SqliteReviewRunStore,
        SqliteSourceMaterializationStore,
    },
};

use super::{composition, progress::Progress};

/// Every construction failure the runtime can report.
///
/// The variants are the questions a caller answers differently: the
/// configuration is wrong (fix the file), a credential is missing (supply it),
/// or an adapter could not be built from an otherwise usable configuration
/// (fix the destination). A web host maps them to different responses without
/// parsing a message.
#[derive(Debug)]
pub enum RuntimeError {
    /// The configuration could not be read, parsed or validated.
    Configuration(ConfigError),
    /// A credential the configuration names is not available.
    ///
    /// The variant carries the configuration key that named it and the variable
    /// itself — never a value.
    Credential { key: &'static str, name: SecretName },
    /// An adapter could not be built from an otherwise usable configuration.
    Connection { message: String },
}

impl RuntimeError {
    /// The configuration key or file this error is about, when it names one.
    pub fn is_about(&self, key: &str) -> bool {
        match self {
            Self::Configuration(error) => error.is_about(key),
            Self::Credential {
                key: credential, ..
            } => credential.contains(key),
            Self::Connection { message } => message.contains(key),
        }
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(error) => write!(formatter, "{error}"),
            Self::Credential { key, name } => {
                write!(formatter, "{key} names {name}, which is not set")
            }
            Self::Connection { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Credential { .. } | Self::Connection { .. } => None,
        }
    }
}

impl From<ConfigError> for RuntimeError {
    fn from(error: ConfigError) -> Self {
        Self::Configuration(error)
    }
}

/// One workspace, ready to work: configuration, credentials and capabilities.
pub struct WorkspaceRuntime {
    /// The validated configuration. Reading a setting goes through it.
    pub config: ValidatedConfig,
    /// Where the configuration was read from.
    pub config_path: PathBuf,
    secrets: Arc<dyn SecretProvider>,
}

/// What a runtime *is*, in a report: its configuration and where it came from.
///
/// The secret provider is named as a capability, never rendered: a provider
/// holds credential values, and a `Debug` dump must be safe to log.
impl std::fmt::Debug for WorkspaceRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceRuntime")
            .field("config", &self.config)
            .field("config_path", &self.config_path)
            .field("secrets", &"<secret provider>")
            .finish()
    }
}

/// Reading a setting goes through the validated configuration, so a caller that
/// holds a runtime never has to ask whether the configuration is usable.
impl Deref for WorkspaceRuntime {
    type Target = ValidatedConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl WorkspaceRuntime {
    /// Loads a workspace and resolves inline credentials from the configuration.
    pub fn load(path: PathBuf) -> Result<Self, RuntimeError> {
        let config = crate::config::load(&path)?;
        let mut provider = InlineSecretProvider::new();
        for (name, value) in config.inline_secrets() {
            provider = provider.with(name, value);
        }
        let secrets: Arc<dyn SecretProvider> = Arc::new(provider);
        Ok(Self {
            config,
            config_path: path,
            secrets,
        })
    }

    /// Loads a workspace with an explicit secret provider.
    ///
    /// This is the constructor a test or an embedding host uses: nothing about
    /// the workspace changes, only where its credentials come from.
    pub fn with_secrets(
        path: PathBuf,
        secrets: Arc<dyn SecretProvider>,
    ) -> Result<Self, RuntimeError> {
        let config = crate::config::load(&path)?;
        Ok(Self {
            config,
            config_path: path,
            secrets,
        })
    }

    /// Assembles a runtime from a configuration that is already validated.
    pub fn new(
        config: ValidatedConfig,
        config_path: PathBuf,
        secrets: Arc<dyn SecretProvider>,
    ) -> Self {
        Self {
            config,
            config_path,
            secrets,
        }
    }

    /// The provider credentials are resolved through.
    pub fn secrets(&self) -> &dyn SecretProvider {
        self.secrets.as_ref()
    }

    /// The content-addressed store this workspace writes through.
    pub fn content_store(&self) -> LocalContentStore {
        LocalContentStore::new(self.config.cas())
    }

    /// Creates the directories a run writes into.
    ///
    /// A workspace that was never initialized has no state directory, and every
    /// store would fail on the same missing parent. Preparing once, here, keeps
    /// that from being discovered in the middle of a run.
    pub fn prepare(&self) -> Result<(), RuntimeError> {
        for directory in [self.config.state_path(), &self.config.cas()] {
            std::fs::create_dir_all(directory).map_err(|error| RuntimeError::Connection {
                message: format!("could not create {}: {error}", directory.display()),
            })?;
        }
        Ok(())
    }

    /// The local source root, as a reader.
    pub fn local_source(&self, source_id: SourceId) -> Result<LocalSource, RuntimeError> {
        composition::local_source(&self.config, self.content_store(), source_id)
    }

    /// The configured R2 source, with its secret resolved from the provider.
    pub fn r2_source(&self) -> Result<R2Source, RuntimeError> {
        composition::r2_source(&self.config, self.secrets(), self.content_store())
    }

    /// The configured asset target, with its secret resolved from the provider.
    pub fn asset_target(&self) -> Result<ConfiguredAssetTarget, RuntimeError> {
        composition::asset_target(&self.config, self.secrets())
    }

    /// The backup repository, a Git repository that stores binary objects in LFS.
    pub fn backup_repository(
        &self,
    ) -> Result<GitBackupRepository<LocalContentStore>, RuntimeError> {
        composition::backup_repository(&self.config, self.content_store())
    }

    /// The commit identity one backup freezes.
    pub fn backup_commit_metadata(&self) -> Result<GitCommitMetadata, RuntimeError> {
        composition::backup_commit_metadata(&self.config)
    }

    /// The configured LFS endpoint, with its credentials resolved.
    pub fn backup_lfs_remote(&self) -> Result<LfsHttpRemote, RuntimeError> {
        composition::backup_lfs_remote(&self.config, self.secrets())
    }

    /// The LFS batch endpoint derived from the backup remote URL.
    pub fn derive_backup_batch_url(&self) -> Result<String, RuntimeError> {
        composition::derive_backup_batch_url(&self.config)
    }

    /// The Git remote of the backup repository.
    pub fn backup_git_remote(&self) -> Result<GitRemoteAdapter, RuntimeError> {
        composition::git_remote(&self.config)
    }

    /// Observes the backup ref without changing it.
    pub fn observe_backup_ref(
        &self,
        target: &GitRefTarget,
    ) -> Result<RemoteRefState, RuntimeError> {
        composition::observe_backup_ref(&self.config, target)
    }

    /// Whether the publication ref is reachable. Presence only: never a write.
    pub fn publication_ref_present(&self) -> bool {
        composition::publication_ref_present(&self.config)
    }

    /// Creates the empty root commit the very first backup is built on.
    pub fn backup_root_commit(
        &self,
        metadata: &GitCommitMetadata,
    ) -> Result<GitCommitOid, RuntimeError> {
        composition::backup_root_commit(&self.config, metadata)
    }

    /// Pushes the bootstrap commit onto the backup branch.
    pub fn push_backup_root_commit(
        &self,
        target: &GitRefTarget,
        commit: &GitCommitOid,
    ) -> Result<(), RuntimeError> {
        composition::push_backup_root_commit(&self.config, target, commit)
    }

    /// Reads one complete source state as an immutable Snapshot.
    pub fn snapshot(&self, progress: &dyn Progress) -> Result<Snapshot, RuntimeError> {
        composition::snapshot(
            &self.config,
            self.secrets(),
            &self.content_store(),
            progress,
        )
    }

    /// The Markdown reviewer one publication runs.
    pub fn markdown_reviewer(
        &self,
        progress: Arc<dyn Progress>,
    ) -> Result<composition::LazyMarkdownReviewer, RuntimeError> {
        composition::markdown_reviewer(
            &self.config,
            Arc::clone(&self.secrets),
            self.content_store(),
            progress,
        )
    }

    /// The asset reviewer one publication runs.
    pub fn asset_reviewer(
        &self,
        progress: Arc<dyn Progress>,
    ) -> Result<composition::LazyAssetReviewer, RuntimeError> {
        composition::asset_reviewer(
            &self.config,
            Arc::clone(&self.secrets),
            self.content_store(),
            progress,
        )
    }

    /// The contract identity of the Markdown reviewer under this workspace.
    pub fn markdown_contract_hash(&self) -> Result<Sha256, RuntimeError> {
        composition::markdown_contract_hash(self.config.review())
    }

    /// The contract identity of the asset reviewer under this workspace.
    pub fn asset_contract_hash(&self) -> Result<Sha256, RuntimeError> {
        composition::asset_contract_hash(self.config.review())
    }

    /// The durable Markdown review runs.
    pub fn document_reviews(&self) -> Result<SqliteReviewRunStore, RuntimeError> {
        composition::open_document_reviews(&self.config)
    }

    /// The durable asset review runs.
    pub fn asset_reviews(&self) -> Result<SqliteAssetReviewRunStore, RuntimeError> {
        composition::open_asset_reviews(&self.config)
    }

    /// The durable human decisions.
    pub fn human_reviews(&self) -> Result<SqliteHumanReviewStore, RuntimeError> {
        composition::open_human_reviews(&self.config)
    }

    /// The durable publication runs.
    pub fn publish_runs(&self) -> Result<SqlitePublishRunStore, RuntimeError> {
        composition::open_publish_runs(&self.config)
    }

    /// The durable observations of the publication ref.
    pub fn remote_observations(&self) -> Result<SqliteRemoteObservationStore, RuntimeError> {
        composition::open_remote_observations(&self.config)
    }

    /// The durable delivery projections.
    pub fn delivery_projections(&self) -> Result<SqliteDeliveryProjectionStore, RuntimeError> {
        composition::open_delivery_projections(&self.config)
    }

    /// The durable observations of delivered assets.
    pub fn asset_observations(&self) -> Result<SqliteAssetObservationStore, RuntimeError> {
        composition::open_asset_observations(&self.config)
    }

    /// The durable record of materialized source objects.
    pub fn source_materializations(
        &self,
    ) -> Result<SqliteSourceMaterializationStore, RuntimeError> {
        composition::open_source_materializations(&self.config)
    }

    /// The durable backup intents.
    pub fn backup_runs(&self) -> Result<SqliteBackupRunStore, RuntimeError> {
        composition::open_backup_runs(&self.config)
    }

    /// Opens every store this workspace can have, so a fresh workspace has all
    /// of them and a workspace without a `backup:` section is never given an
    /// empty database it never uses.
    pub fn open_stores(&self) -> Result<(), RuntimeError> {
        composition::open_all_stores(&self.config)
    }
}
