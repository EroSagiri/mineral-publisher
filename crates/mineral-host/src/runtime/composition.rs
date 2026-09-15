//! Building the host's capabilities: who is constructed, and with which
//! implementation.
//!
//! Every adapter the layers above use is built here and nowhere else. A use case
//! asks the runtime for a store, a source, a target or a reviewer; it never
//! names a SQLite store, an object store, an HTTP client or `git` itself. That is
//! what keeps an application module readable as a workflow, and what makes a
//! different implementation a change in one file.
//!
//! Credentials arrive as [`SecretName`]s from the validated configuration and
//! are resolved through a [`SecretProvider`]. Nothing here reads the process
//! environment, so nothing here can leak a value into a log or a report.

use std::{
    fmt,
    process::Command,
    sync::{Arc, OnceLock, atomic::AtomicUsize},
    time::Duration,
    time::SystemTime,
};

use mineral_core::backup::BackupRunId;
use mineral_core::source::{DEFAULT_MAX_SCAN_ATTEMPTS, stabilize_scan};

use crate::{
    asset::{ConfiguredAssetTarget, R2ObjectStore, R2ObjectStoreConfig, R2SecretKey},
    backup::{
        git_backup::{
            GitBackupRepository, backup_commit_metadata as git_backup_commit_metadata,
            observe_backup_ref as observe_backup_ref_with,
        },
        lfs_http::{LfsHttpConfig, LfsHttpRemote, LfsToken},
    },
    config::SourceType,
    config::{
        DEFAULT_BACKUP_AUTHOR_EMAIL, DEFAULT_BACKUP_AUTHOR_NAME,
        DEFAULT_BACKUP_LFS_TIMEOUT_SECONDS, DEFAULT_BACKUP_MESSAGE, ReviewConfig, SecretName,
        SecretProvider, SecretValue, ValidatedConfig,
    },
    domain::{Sha256, Snapshot, SnapshotFile, SnapshotId, SourceId},
    policy::{ReviewCandidate, Reviewer, ReviewerError, ReviewerReport},
    publisher::{GitCommitMetadata, GitCommitOid, GitRefTarget, GitRemoteAdapter, RemoteRefState},
    reviewer::{
        DeepSeekApiKey, DeepSeekAssetReviewer, DeepSeekAssetReviewerConfig,
        DeepSeekMarkdownReviewer, DeepSeekMarkdownReviewerConfig,
    },
    source::{LocalSource, r2::R2Source, r2::R2SourcePrefix},
    storage::{
        LocalContentStore, SqliteAssetObservationStore, SqliteAssetReviewRunStore,
        SqliteBackupRunStore, SqliteDeliveryProjectionStore, SqliteHumanReviewStore,
        SqlitePublishRunStore, SqliteRemoteObservationStore, SqliteReviewRunStore,
        SqliteSourceMaterializationStore,
    },
    workflow::{AssetReviewCandidate, AssetReviewer, AssetReviewerError, AssetReviewerReport},
};

use super::progress::Progress;
use super::workspace::RuntimeError;
use crate::backup::application::BackupRunIdGenerator;
use sha2::Digest;

/// Resolves one credential, naming the configuration key that asked for it.
///
/// The key is what a message names when the variable is missing, which is the
/// only useful thing to say: a credential that is absent has no value to report.
pub fn credential(
    secrets: &dyn SecretProvider,
    key: &'static str,
    name: SecretName,
) -> Result<SecretValue, RuntimeError> {
    secrets
        .resolve(&name)
        .map_err(|_| RuntimeError::Credential { key, name })
}

/// Wraps an adapter's own construction failure.
///
/// The adapter knows the most specific thing to say about why it could not be
/// built, so the message is carried rather than rewritten.
fn connection(error: impl fmt::Display) -> RuntimeError {
    RuntimeError::Connection {
        message: error.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Durable stores
// ---------------------------------------------------------------------------
//
// Each store is opened by the command that uses it, so a command never creates
// a database it does not read. Opening is idempotent and creates the schema on
// first use.

macro_rules! store_opener {
    ($name:ident, $store:ty, $file:literal, $doc:literal) => {
        #[doc = $doc]
        pub fn $name(config: &ValidatedConfig) -> Result<$store, RuntimeError> {
            <$store>::open(config.state_file($file)).map_err(connection)
        }
    };
}

store_opener!(
    open_document_reviews,
    SqliteReviewRunStore,
    "document-reviews.sqlite3",
    "The durable Markdown review runs."
);
store_opener!(
    open_asset_reviews,
    SqliteAssetReviewRunStore,
    "asset-reviews.sqlite3",
    "The durable asset review runs."
);
store_opener!(
    open_human_reviews,
    SqliteHumanReviewStore,
    "human-reviews.sqlite3",
    "The durable human decisions."
);
store_opener!(
    open_publish_runs,
    SqlitePublishRunStore,
    "publish-runs.sqlite3",
    "The durable publication runs."
);
store_opener!(
    open_remote_observations,
    SqliteRemoteObservationStore,
    "remote-observations.sqlite3",
    "The durable observations of the publication ref."
);
store_opener!(
    open_delivery_projections,
    SqliteDeliveryProjectionStore,
    "delivery-projections.sqlite3",
    "The durable delivery projections."
);
store_opener!(
    open_asset_observations,
    SqliteAssetObservationStore,
    "asset-observations.sqlite3",
    "The durable observations of delivered assets."
);
store_opener!(
    open_source_materializations,
    SqliteSourceMaterializationStore,
    "source-materializations.sqlite3",
    "The durable record of materialized source objects."
);
store_opener!(
    open_backup_runs,
    SqliteBackupRunStore,
    "backup-runs.sqlite3",
    "The durable backup intents."
);

/// Opens every store a workspace can have, so a fresh workspace has all of them.
///
/// The backup store only exists for a workspace that backs up, so a workspace
/// without a `backup:` section is never given an empty database it never uses.
pub fn open_all_stores(config: &ValidatedConfig) -> Result<(), RuntimeError> {
    open_document_reviews(config)?;
    open_asset_reviews(config)?;
    open_human_reviews(config)?;
    open_publish_runs(config)?;
    open_remote_observations(config)?;
    open_delivery_projections(config)?;
    open_asset_observations(config)?;
    open_source_materializations(config)?;
    if config.backup_enabled() {
        open_backup_runs(config)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// The local source root, as a reader.
pub fn local_source(
    config: &ValidatedConfig,
    store: LocalContentStore,
    source_id: SourceId,
) -> Result<LocalSource, RuntimeError> {
    Ok(LocalSource::new(
        config.local_source_path()?,
        source_id,
        store,
    ))
}

/// The configured R2 source, with its secret resolved from the provider.
///
/// The secret never reaches the engine or a durable record: it is resolved here,
/// handed to the adapter, and dropped with it.
pub fn r2_source(
    config: &ValidatedConfig,
    secrets: &dyn SecretProvider,
    store: LocalContentStore,
) -> Result<R2Source, RuntimeError> {
    let r2 = config
        .source
        .r2
        .as_ref()
        .ok_or_else(|| connection("source.type is r2 but source.r2 is not configured"))?;
    let prefix = R2SourcePrefix::new(&r2.prefix)
        .map_err(|error| connection(format!("source.r2.prefix is unusable: {error}")))?;
    let secret = credential(
        secrets,
        "source.r2.secret_access_key",
        config.source_r2_secret_name()?,
    )?;
    let mut endpoint = R2ObjectStoreConfig::new(
        r2.endpoint.clone(),
        r2.bucket.clone(),
        r2.access_key_id.clone(),
        R2SecretKey::new(secret.expose().to_owned()).map_err(connection)?,
    )
    .map_err(connection)?;
    if let Some(region) = &r2.region {
        endpoint = endpoint.with_region(region.clone()).map_err(connection)?;
    }
    if let Some(seconds) = r2.timeout_seconds {
        endpoint = endpoint.with_timeout(Duration::from_secs(seconds));
    }
    let materializations = open_source_materializations(config)?;
    R2Source::new(endpoint, prefix, store, materializations).map_err(connection)
}

// ---------------------------------------------------------------------------
// Asset target
// ---------------------------------------------------------------------------

/// The configured asset target, with its secret resolved from the provider.
pub fn asset_target(
    config: &ValidatedConfig,
    secrets: &dyn SecretProvider,
) -> Result<ConfiguredAssetTarget, RuntimeError> {
    let assets = config.assets()?;
    if let Some(target_path) = &assets.target_path {
        return Ok(ConfiguredAssetTarget::filesystem(target_path));
    }
    let r2 = assets.r2.as_ref().ok_or_else(|| {
        connection("assets must configure either target_path or r2 before publishing")
    })?;
    let secret = credential(
        secrets,
        "assets.r2.secret_access_key",
        config.assets_r2_secret_name()?,
    )?;
    let mut endpoint = R2ObjectStoreConfig::new(
        r2.endpoint.clone(),
        r2.bucket.clone(),
        r2.access_key_id.clone(),
        R2SecretKey::new(secret.expose().to_owned()).map_err(connection)?,
    )
    .map_err(connection)?;
    if let Some(region) = &r2.region {
        endpoint = endpoint.with_region(region.clone()).map_err(connection)?;
    }
    if let Some(seconds) = r2.timeout_seconds {
        endpoint = endpoint.with_timeout(Duration::from_secs(seconds));
    }
    let spool = config.state_file("asset-spool");
    let store = R2ObjectStore::new(endpoint, spool).map_err(connection)?;
    Ok(ConfiguredAssetTarget::r2(store))
}

// ---------------------------------------------------------------------------
// Backup target
// ---------------------------------------------------------------------------

/// The backup repository, a Git repository that stores binary objects in LFS.
pub fn backup_repository(
    config: &ValidatedConfig,
    store: LocalContentStore,
) -> Result<GitBackupRepository<LocalContentStore>, RuntimeError> {
    GitBackupRepository::new(config.backup_git_repository()?, store).map_err(connection)
}

/// The commit identity one backup freezes, using the configured values and the
/// documented defaults for whichever were omitted.
pub fn backup_commit_metadata(config: &ValidatedConfig) -> Result<GitCommitMetadata, RuntimeError> {
    let git = config.backup_git()?;
    git_backup_commit_metadata(
        git.author_name
            .as_deref()
            .unwrap_or(DEFAULT_BACKUP_AUTHOR_NAME),
        git.author_email
            .as_deref()
            .unwrap_or(DEFAULT_BACKUP_AUTHOR_EMAIL),
        git.message.as_deref().unwrap_or(DEFAULT_BACKUP_MESSAGE),
    )
    .map_err(connection)
}

/// The configured LFS endpoint, with its credentials resolved from the provider.
pub fn backup_lfs_remote(
    config: &ValidatedConfig,
    secrets: &dyn SecretProvider,
) -> Result<LfsHttpRemote, RuntimeError> {
    let lfs = config.backup_lfs()?;
    let (username_name, token_name) = config.backup_lfs_credential_names()?;
    let username = credential(secrets, "backup.lfs.username_env", username_name)?;
    let token = credential(secrets, "backup.lfs.token_env", token_name)?;
    let batch_url = match &lfs.batch_url {
        Some(url) => url.clone(),
        None => derive_backup_batch_url(config)?,
    };
    let timeout = Duration::from_secs(
        lfs.timeout_seconds
            .unwrap_or(DEFAULT_BACKUP_LFS_TIMEOUT_SECONDS),
    );
    let endpoint = LfsHttpConfig::new(
        batch_url,
        username.expose().to_owned(),
        LfsToken::new(token.expose().to_owned()).map_err(connection)?,
        timeout,
    )
    .map_err(connection)?;
    LfsHttpRemote::new(endpoint).map_err(connection)
}

/// Derives the LFS batch endpoint from the backup remote URL.
///
/// A remote URL already names where the repository lives, and a Git LFS server
/// serves the batch API from `<remote>/info/lfs`. A remote that is not an
/// absolute http(s) URL cannot be derived and is refused rather than guessed.
pub fn derive_backup_batch_url(config: &ValidatedConfig) -> Result<String, RuntimeError> {
    let git = config.backup_git()?;
    let remote = git
        .remote
        .as_deref()
        .ok_or_else(|| connection("backup.git.remote must be non-empty when backup is enabled"))?;
    let output = Command::new("git")
        .current_dir(config.backup_git_repository()?)
        .args(["remote", "get-url", remote])
        .output()
        .map_err(|error| connection(format!("git is unavailable: {error}")))?;
    if !output.status.success() {
        return Err(connection(format!(
            "could not read the URL of backup remote {remote}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if url.is_empty() {
        return Err(connection(format!("backup remote {remote} has no URL")));
    }
    Ok(format!("{}/info/lfs", url.trim_end_matches('/')))
}

// ---------------------------------------------------------------------------
// Review providers
// ---------------------------------------------------------------------------

/// The Markdown reviewer configuration one endpoint request is built from.
pub fn markdown_config(
    config: &ReviewConfig,
    key: DeepSeekApiKey,
) -> Result<DeepSeekMarkdownReviewerConfig, RuntimeError> {
    DeepSeekMarkdownReviewerConfig::new(
        &config.api_base_url,
        &config.markdown_model,
        key,
        Duration::from_secs(config.timeout_seconds),
        2 * 1024 * 1024,
        64 * 1024,
    )
    .map_err(connection)
}

/// The asset reviewer configuration one endpoint request is built from.
pub fn asset_config(
    config: &ReviewConfig,
    key: DeepSeekApiKey,
) -> Result<DeepSeekAssetReviewerConfig, RuntimeError> {
    DeepSeekAssetReviewerConfig::new(
        &config.api_base_url,
        key,
        Duration::from_secs(config.timeout_seconds),
        8 * 1024 * 1024,
        64 * 1024,
    )
    .map_err(connection)?
    .with_model(&config.asset_model)
    .map_err(connection)
}

/// The contract identity of the Markdown reviewer under one configuration.
///
/// It is derived by constructing the reviewer, because the prompt *is* the
/// reviewer: an identity that did not hash it could not notice that it changed.
pub fn markdown_contract_hash(config: &ReviewConfig) -> Result<Sha256, RuntimeError> {
    let reviewer = DeepSeekMarkdownReviewer::new(
        markdown_config(
            config,
            DeepSeekApiKey::new("contract-only").map_err(connection)?,
        )?,
        LocalContentStore::new(std::env::temp_dir().join("mineral-contract-cas")),
    )
    .map_err(connection)?;
    Ok(reviewer.prompt_sha256())
}

/// The contract identity of the asset reviewer under one configuration.
pub fn asset_contract_hash(config: &ReviewConfig) -> Result<Sha256, RuntimeError> {
    let reviewer = DeepSeekAssetReviewer::new(
        asset_config(
            config,
            DeepSeekApiKey::new("contract-only").map_err(connection)?,
        )?,
        LocalContentStore::new(std::env::temp_dir().join("mineral-contract-cas")),
    )
    .map_err(connection)?;
    Ok(reviewer.prompt_sha256())
}

/// A Markdown reviewer that builds its endpoint on the first item it sees.
///
/// A publication that never reaches the provider must not need its credential,
/// and one that does must fail with a message that names the variable rather
/// than the transport. Resolving lazily through the provider makes both true at
/// once.
pub struct LazyMarkdownReviewer {
    config: ReviewConfig,
    api_key_name: SecretName,
    secrets: Arc<dyn SecretProvider>,
    store: LocalContentStore,
    progress: Arc<dyn Progress>,
    completed: AtomicUsize,
    reviewer: OnceLock<Result<DeepSeekMarkdownReviewer, String>>,
}

impl Reviewer for LazyMarkdownReviewer {
    fn review(&self, candidate: &ReviewCandidate) -> Result<ReviewerReport, ReviewerError> {
        self.progress
            .detail(&format!("      Markdown review: {}", candidate.path()));
        let reviewer = self.reviewer.get_or_init(|| {
            (|| -> Result<_, RuntimeError> {
                let key = DeepSeekApiKey::new(
                    credential(
                        self.secrets.as_ref(),
                        "review.api_key",
                        self.api_key_name.clone(),
                    )?
                    .expose()
                    .to_owned(),
                )
                .map_err(connection)?;
                DeepSeekMarkdownReviewer::new(
                    markdown_config(&self.config, key)?,
                    self.store.clone(),
                )
                .map_err(connection)
            })()
            .map_err(|error| error.to_string())
        });
        let result = reviewer
            .as_ref()
            .map_err(|error| ReviewerError::new(format!("review provider unavailable: {error}")))
            .and_then(|reviewer| reviewer.review(candidate));
        let completed = self
            .completed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.progress
            .detail(&format!("      Markdown reviews completed: {completed}"));
        result
    }
}

/// An asset reviewer that builds its endpoint on the first item it sees.
pub struct LazyAssetReviewer {
    config: ReviewConfig,
    api_key_name: SecretName,
    secrets: Arc<dyn SecretProvider>,
    store: LocalContentStore,
    progress: Arc<dyn Progress>,
    completed: AtomicUsize,
    reviewer: OnceLock<Result<DeepSeekAssetReviewer, String>>,
}

impl AssetReviewer for LazyAssetReviewer {
    fn review(
        &self,
        candidate: &AssetReviewCandidate,
    ) -> Result<AssetReviewerReport, AssetReviewerError> {
        self.progress
            .detail(&format!("[3/4] Asset review: {}", candidate.path()));
        let reviewer = self.reviewer.get_or_init(|| {
            (|| -> Result<_, RuntimeError> {
                let key = DeepSeekApiKey::new(
                    credential(
                        self.secrets.as_ref(),
                        "review.api_key",
                        self.api_key_name.clone(),
                    )?
                    .expose()
                    .to_owned(),
                )
                .map_err(connection)?;
                DeepSeekAssetReviewer::new(asset_config(&self.config, key)?, self.store.clone())
                    .map_err(connection)
            })()
            .map_err(|error| error.to_string())
        });
        let result = reviewer
            .as_ref()
            .map_err(|error| {
                AssetReviewerError::new(format!("review provider unavailable: {error}"))
            })
            .and_then(|reviewer| reviewer.review(candidate));
        let completed = self
            .completed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.progress
            .detail(&format!("      Asset reviews completed: {completed}"));
        result
    }
}

/// The Markdown reviewer one workspace publishes with.
pub fn markdown_reviewer(
    config: &ValidatedConfig,
    secrets: Arc<dyn SecretProvider>,
    store: LocalContentStore,
    progress: Arc<dyn Progress>,
) -> Result<LazyMarkdownReviewer, RuntimeError> {
    Ok(LazyMarkdownReviewer {
        config: config.review().clone(),
        api_key_name: config.review_api_key_name()?,
        secrets,
        store,
        progress,
        completed: AtomicUsize::new(0),
        reviewer: OnceLock::new(),
    })
}

/// The asset reviewer one workspace publishes with.
pub fn asset_reviewer(
    config: &ValidatedConfig,
    secrets: Arc<dyn SecretProvider>,
    store: LocalContentStore,
    progress: Arc<dyn Progress>,
) -> Result<LazyAssetReviewer, RuntimeError> {
    Ok(LazyAssetReviewer {
        config: config.review().clone(),
        api_key_name: config.review_api_key_name()?,
        secrets,
        store,
        progress,
        completed: AtomicUsize::new(0),
        reviewer: OnceLock::new(),
    })
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------
//
// Running `git` is a host capability, so it happens here and nowhere above.

/// The Git remote of the backup repository.
pub fn git_remote(config: &ValidatedConfig) -> Result<GitRemoteAdapter, RuntimeError> {
    GitRemoteAdapter::new(config.backup_git_repository()?).map_err(connection)
}

/// Observes the backup ref without changing it.
///
/// The only network call is the `ls-remote` behind the observation.
pub fn observe_backup_ref(
    config: &ValidatedConfig,
    target: &GitRefTarget,
) -> Result<RemoteRefState, RuntimeError> {
    observe_backup_ref_with(config.backup_git_repository()?, target).map_err(connection)
}

/// Whether the publication ref is reachable from the publication repository.
///
/// Presence only: this never fetches and never writes, so a diagnostic stays
/// usable while the remote is unreachable.
pub fn publication_ref_present(config: &ValidatedConfig) -> bool {
    Command::new("git")
        .current_dir(&config.git.repository)
        .args([
            "ls-remote",
            "--exit-code",
            &config.git.remote,
            &config.git.reference,
        ])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Creates the empty root commit the very first backup is built on.
///
/// The engine builds every backup on the commit the ref already holds, so the
/// first one needs a base derived from nothing. This is the one place the host
/// creates history rather than continuing it.
pub fn backup_root_commit(
    config: &ValidatedConfig,
    metadata: &GitCommitMetadata,
) -> Result<GitCommitOid, RuntimeError> {
    let repository = config.backup_git_repository()?;
    // `Command::output` closes stdin, so `git mktree` reads an empty list and
    // prints the empty tree the root commit points at.
    let tree = git_stdout(repository, &["mktree"])?;
    let output = Command::new("git")
        .current_dir(repository)
        .env("GIT_AUTHOR_NAME", metadata.author_name())
        .env("GIT_AUTHOR_EMAIL", metadata.author_email())
        .env("GIT_COMMITTER_NAME", metadata.author_name())
        .env("GIT_COMMITTER_EMAIL", metadata.author_email())
        .args(["commit-tree", &tree, "-m", metadata.message()])
        .output()
        .map_err(|error| connection(format!("git is unavailable: {error}")))?;
    if !output.status.success() {
        return Err(connection(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    GitCommitOid::new(commit).map_err(connection)
}

/// Pushes the bootstrap commit onto the backup branch.
pub fn push_backup_root_commit(
    config: &ValidatedConfig,
    target: &GitRefTarget,
    commit: &GitCommitOid,
) -> Result<(), RuntimeError> {
    let refspec = format!("{}:{}", commit.as_str(), target.destination_ref());
    let output = Command::new("git")
        .current_dir(config.backup_git_repository()?)
        .args(["push", target.remote_name(), &refspec])
        .output()
        .map_err(|error| connection(format!("git is unavailable: {error}")))?;
    if !output.status.success() {
        return Err(connection(format!(
            "git push failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Runs a `git` command whose stdout is one value, failing closed on a non-zero exit.
fn git_stdout(repository: &std::path::Path, arguments: &[&str]) -> Result<String, RuntimeError> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .map_err(|error| connection(format!("git is unavailable: {error}")))?;
    if !output.status.success() {
        return Err(connection(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------
//
// An identity that is frozen into a durable record before any remote effect
// must be globally unique: a restart resuming an intent and a fresh run must
// never collide on one identity, so these allocate from UUID entropy rather
// than from a counter.

/// SQLite INTEGER is signed even though the domain IDs are `u64`. Keep every
/// production-generated identity in the positive range so persistence can never
/// reject a valid generated ID.
pub fn sqlite_positive_id(value: u64) -> u64 {
    (value & i64::MAX as u64).max(1)
}

fn random_u64() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    sqlite_positive_id(u64::from_be_bytes(
        bytes[..8].try_into().expect("fixed UUID width"),
    ))
}

/// Allocates backup-attempt identities from UUID v4 entropy.
pub struct UuidBackupRunIdGenerator;

impl BackupRunIdGenerator for UuidBackupRunIdGenerator {
    fn next_id(&self) -> BackupRunId {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&bytes[..8]);
        BackupRunId::new(sqlite_positive_id(u64::from_be_bytes(prefix)))
            .expect("UUID-derived id is nonzero")
    }
}

/// Allocates Markdown review identities from UUID v4 entropy.
pub struct RandomDocumentIds;

impl crate::workflow::ReviewRunIdGenerator for RandomDocumentIds {
    type Error = std::convert::Infallible;

    fn next_id(&mut self) -> Result<crate::policy::ReviewRunId, Self::Error> {
        Ok(crate::policy::ReviewRunId::new(random_u64()).expect("UUID-derived id is nonzero"))
    }
}

/// Allocates asset review identities from UUID v4 entropy.
pub struct RandomAssetIds;

impl crate::workflow::AssetReviewRunIdGenerator for RandomAssetIds {
    type Error = std::convert::Infallible;

    fn next_id(&mut self) -> Result<crate::workflow::AssetReviewRunId, Self::Error> {
        Ok(crate::workflow::AssetReviewRunId::new(random_u64())
            .expect("UUID-derived id is nonzero"))
    }
}

/// Allocates human decision identities from UUID v4 entropy.
pub fn random_human_id()
-> Result<crate::workflow::HumanReviewId, crate::workflow::HumanReviewRecordError> {
    crate::workflow::HumanReviewId::new(random_u64())
}

// ---------------------------------------------------------------------------
// Reading the source
// ---------------------------------------------------------------------------

/// Reads one complete source state as an immutable Snapshot.
///
/// The identity is derived from the source id and every file's path, size and
/// content identity — never from the source kind — so identical bytes from a
/// local directory and from R2 describe the same state.
pub fn snapshot(
    config: &ValidatedConfig,
    secrets: &dyn SecretProvider,
    store: &LocalContentStore,
    progress: &dyn Progress,
) -> Result<Snapshot, RuntimeError> {
    let source_id = SourceId::new(config.source.id.clone()).map_err(connection)?;
    match config.source_kind() {
        SourceType::Local => {
            let source = local_source(config, store.clone(), source_id.clone())?;
            let provisional = source
                .snapshot(SnapshotId::new(1).map_err(connection)?, SystemTime::now())
                .map_err(connection)?;
            let id = snapshot_id(&source_id, provisional.files());
            source
                .snapshot(SnapshotId::new(id).map_err(connection)?, SystemTime::now())
                .map_err(connection)
        }
        SourceType::R2 => {
            // One stabilized scan, one assembled state. The identity is computed
            // from the materialized bytes only, so the same bytes from a local
            // directory and from R2 describe the same source state.
            let source = r2_source(config, secrets, store.clone())?;
            let stabilized = stabilize_scan(&source, DEFAULT_MAX_SCAN_ATTEMPTS)
                .map_err(|error| connection(format!("could not read the R2 source: {error}")))?;
            progress.detail(&format!(
                "[1/4] R2 source {} prefix {}: {} file(s), {} read, {} reused, inventory {}",
                source.describe(),
                source.prefix(),
                stabilized.materialized().len(),
                source.fetched_objects(),
                source.reused_objects(),
                stabilized.inventory_identity(),
            ));
            let provisional = stabilized
                .snapshot(
                    SnapshotId::new(1).map_err(connection)?,
                    SystemTime::now(),
                    source_id.clone(),
                )
                .map_err(connection)?;
            let id = snapshot_id(&source_id, provisional.files());
            stabilized
                .snapshot(
                    SnapshotId::new(id).map_err(connection)?,
                    SystemTime::now(),
                    source_id,
                )
                .map_err(connection)
        }
    }
}

/// The deterministic snapshot identity of one complete source state.
pub fn snapshot_id(source_id: &SourceId, files: &[SnapshotFile]) -> u64 {
    let mut hasher = sha2::Sha256::new();
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
