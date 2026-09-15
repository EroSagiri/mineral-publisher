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
const BACKUP_WORKSPACE: &str = "source:\n  id: local-vault\n  path: ./vault\nstate:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\nassets:\n  public_base_url: https://assets.example.com\n  target_path: ./asset-target\nreview:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key: runtime-test-key\n  timeout_seconds: 45\nbackup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_RUNTIME_TEST_USER\n    token_env: MINERAL_RUNTIME_TEST_TOKEN\n";

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

/// Configuration and construction behaviour, exercised through the runtime.
///
/// These began as CLI tests, because the CLI used to be the only place a
/// workspace could be built. They are about the configuration layer and the
/// composition root, so they now live with them — and they prove that neither
/// needs a command line to be tested.
#[cfg(test)]
mod workspace_configuration {
    use std::{
        error::Error,
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use crate::{
        backup::lfs_http::{LfsHttpConfig, LfsHttpRemote, LfsToken},
        config::{DEFAULT_CONFIG, RawConfig as Config, SourceType},
        runtime::{WorkspaceRuntime as Workspace, composition::sqlite_positive_id},
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
        let r2 = "  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-assets\n    access_key_id: AKIDEXAMPLE\n";

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
            "  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-assets\n    access_key_id: AKIDEXAMPLE\n",
        );
        let path = directory.join("r2.yml");
        std::fs::write(&path, text).unwrap();
        let workspace = Workspace::load(path).unwrap();

        let error = workspace
            .asset_target()
            .err()
            .expect("an unset secret must be refused")
            .to_string();

        assert!(error.contains("MINERAL_R2_SECRET_ACCESS_KEY"), "{error}");
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
            "source:\n{source}state:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\n{assets}review:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  timeout_seconds: 45\n"
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
            "  id: r2-vault\n  type: r2\n  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-vault\n    prefix: {prefix}\n    access_key_id: AKIDEXAMPLE\n"
        )
    }

    fn r2_assets_block(bucket: &str) -> String {
        format!(
            "assets:\n  public_base_url: https://assets.example.com\n  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: {bucket}\n    access_key_id: AKIDEXAMPLE\n"
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
        assert!(error.contains("MINERAL_R2_SECRET_ACCESS_KEY"), "{error}");
    }

    #[test]
    fn a_source_must_choose_exactly_one_kind() {
        // `type: local` cannot also carry an R2 namespace.
        let local_with_r2 = source_workspace(
            "  id: local-vault\n  path: ./vault\n  type: local\n  r2:\n    endpoint: https://account.r2.cloudflarestorage.com\n    bucket: mineral-vault\n    prefix: vault/\n    access_key_id: A\n",
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
            "source:\n{source}state:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\n{assets}{backup}review:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  timeout_seconds: 45\n"
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

        // The use case reports "not configured" rather than failing.
        let outcome = crate::application::backup::backup(
            &workspace,
            crate::application::backup::BackupRequest::now().unwrap(),
            &crate::runtime::NoProgress,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            crate::application::backup::BackupResult::NotConfigured
        ));
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
