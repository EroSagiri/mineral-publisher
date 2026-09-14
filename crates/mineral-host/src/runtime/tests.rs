//! The runtime's own tests.
//!
//! They show the two things the composition root exists to promise: every
//! capability is built from a validated configuration plus a secret provider,
//! and a caller can supply that provider itself. Nothing here writes to the
//! process environment or reaches the network.

use std::{fs, path::PathBuf, sync::Arc};

use crate::{
    config::{RawConfig, StaticSecretProvider, ValidatedConfig},
    runtime::WorkspaceRuntime,
};

/// One scratch directory per test, holding a workspace configuration.
fn workspace(name: &str, text: &str) -> (PathBuf, ValidatedConfig) {
    let directory =
        std::env::temp_dir().join(format!("mineral-runtime-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("mineral.yaml");
    fs::write(&path, text).unwrap();
    let model: RawConfig = serde_yaml_ng::from_str(text).unwrap();
    let config = ValidatedConfig::from_raw(model, &path).unwrap();
    (path, config)
}

/// A workspace whose backup target names two credential variables.
const BACKUP_WORKSPACE: &str = "source:\n  id: local-vault\n  path: ./vault\nstate:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\nassets:\n  public_base_url: https://assets.example.com\n  target_path: ./asset-target\nreview:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key_env: MINERAL_DEEPSEEK_API_KEY\n  timeout_seconds: 45\nbackup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_RUNTIME_TEST_USER\n    token_env: MINERAL_RUNTIME_TEST_TOKEN\n";

/// A caller supplies the credentials, so the runtime never consults the
/// environment. This is the seam a web host or a test uses.
#[test]
fn a_runtime_resolves_credentials_through_the_provider_it_is_given() {
    let (path, config) = workspace("provider", BACKUP_WORKSPACE);
    let secrets = Arc::new(
        StaticSecretProvider::new()
            .with("MINERAL_RUNTIME_TEST_USER", "mineral-backup")
            .with("MINERAL_RUNTIME_TEST_TOKEN", "runtime-secret-token"),
    ) as Arc<dyn crate::config::SecretProvider>;
    let runtime = WorkspaceRuntime::new(config, path, Arc::clone(&secrets));

    let lfs = runtime.backup_lfs_remote().unwrap();
    // The username is how an operator recognises the endpoint; the token must not
    // appear anywhere an endpoint can be described.
    assert!(
        lfs.describe().contains("mineral-backup"),
        "{}",
        lfs.describe()
    );
    assert!(!lfs.describe().contains("runtime-secret-token"));

    // Nothing in a dump of the runtime exposes the value either.
    let dump = format!("{runtime:?}");
    assert!(dump.contains("MINERAL_RUNTIME_TEST_TOKEN"), "{dump}");
    assert!(!dump.contains("runtime-secret-token"), "{dump}");
    assert!(!format!("{secrets:?}").contains("runtime-secret-token"));
    assert!(format!("{secrets:?}").contains("MINERAL_RUNTIME_TEST_TOKEN"));
}

/// A credential the configuration names but the provider cannot resolve fails
/// closed, and the message names the variable rather than a transport.
#[test]
fn a_missing_credential_is_refused_by_name_before_anything_is_connected() {
    let (path, config) = workspace("missing", BACKUP_WORKSPACE);
    let runtime = WorkspaceRuntime::new(config, path, Arc::new(StaticSecretProvider::new()));

    let error = match runtime.backup_lfs_remote() {
        Err(error) => error.to_string(),
        Ok(_) => panic!("an unresolved credential must be refused"),
    };
    assert!(error.contains("MINERAL_RUNTIME_TEST_USER"), "{error}");
    assert!(error.contains("backup.lfs.username_env"), "{error}");
}

/// The environment provider is the production default, and it fails closed by
/// name for a variable no test process set.
#[test]
fn the_default_provider_is_the_environment() {
    let (path, _) = workspace("environment", BACKUP_WORKSPACE);
    // Loading never resolves a credential, because validation is pure: a
    // workspace that names a variable the process has not set still loads.
    let runtime = WorkspaceRuntime::load(path).expect("loading resolves no credential");
    let error = match runtime.backup_lfs_remote() {
        Err(error) => error.to_string(),
        Ok(_) => panic!("an unset credential variable must be refused"),
    };
    assert!(error.contains("MINERAL_RUNTIME_TEST_USER"), "{error}");
}

/// Every store the runtime opens lands in the workspace's state directory, so
/// the composition root — not the caller — decides where state lives.
#[test]
fn the_runtime_owns_where_durable_state_lives() {
    let (path, config) = workspace("state", BACKUP_WORKSPACE);
    let state = config.state_path().to_path_buf();
    // Opening a store never creates the workspace; `mineral init` does that.
    fs::create_dir_all(&state).unwrap();
    let runtime = WorkspaceRuntime::new(config, path, Arc::new(StaticSecretProvider::new()));

    runtime.document_reviews().unwrap();
    runtime.backup_runs().unwrap();

    assert!(state.join("document-reviews.sqlite3").is_file());
    assert!(state.join("backup-runs.sqlite3").is_file());
    assert_eq!(runtime.content_store().root(), state.join("cas").as_path());
}

/// A workspace with no `backup:` section is never given a backup database.
#[test]
fn a_workspace_without_a_backup_target_gets_no_backup_store() {
    let text = BACKUP_WORKSPACE.split("backup:\n").next().unwrap();
    let (path, config) = workspace("no-backup", text);
    let state = config.state_path().to_path_buf();
    fs::create_dir_all(&state).unwrap();
    let runtime = WorkspaceRuntime::new(config, path, Arc::new(StaticSecretProvider::new()));

    assert!(!runtime.backup_enabled());
    runtime.open_stores().unwrap();

    assert!(state.join("document-reviews.sqlite3").is_file());
    assert!(!state.join("backup-runs.sqlite3").exists());
}
