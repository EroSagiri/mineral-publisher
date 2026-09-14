//! The configuration layer's own tests.
//!
//! They are deliberately file-system-light: the only thing a validated
//! configuration needs from the world is a directory to resolve relative paths
//! against, so a test can construct one from a string and never write a file.

use std::{fs, path::PathBuf};

use super::{
    ConfigFormat, DEFAULT_CONFIG, DEFAULT_CONFIG_TOML, RawConfig, SourceType, StaticSecretProvider,
    ValidatedConfig, load,
    model::SourceConfig,
    secrets::{EnvSecretProvider, SecretName, SecretProvider, SecretValue},
};

/// One scratch directory per test process, following the convention the rest of
/// the host's tests already use.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("mineral-config-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// The two templates describe the same workspace.
///
/// This is the promise TOML support makes: the language is surface syntax, and
/// nothing downstream can tell which one a workspace was written in. Comparing
/// the *validated* forms is what makes that a real claim rather than a claim
/// about two similar-looking strings.
#[test]
fn the_toml_and_yaml_templates_describe_the_same_workspace() {
    let directory = scratch("templates");
    let yaml = directory.join("mineral.yaml");
    let toml = directory.join("mineral.toml");
    fs::write(&yaml, DEFAULT_CONFIG).unwrap();
    fs::write(&toml, DEFAULT_CONFIG_TOML).unwrap();

    let from_yaml = load(&yaml).unwrap();
    let from_toml = load(&toml).unwrap();

    assert_eq!(from_yaml.source_kind(), from_toml.source_kind());
    assert_eq!(from_yaml.source.id, from_toml.source.id);
    assert_eq!(from_yaml.state.path, from_toml.state.path);
    assert_eq!(from_yaml.git.repository, from_toml.git.repository);
    assert_eq!(from_yaml.git.remote, from_toml.git.remote);
    assert_eq!(from_yaml.git.reference, from_toml.git.reference);
    assert_eq!(from_yaml.git.author_email, from_toml.git.author_email);
    assert_eq!(
        from_yaml.assets.as_ref().unwrap().public_base_url,
        from_toml.assets.as_ref().unwrap().public_base_url
    );
    assert_eq!(
        from_yaml.assets.as_ref().unwrap().target_path,
        from_toml.assets.as_ref().unwrap().target_path
    );
    assert_eq!(
        from_yaml.public.as_ref().unwrap().exclude,
        from_toml.public.as_ref().unwrap().exclude
    );
    assert_eq!(
        from_yaml.review.markdown_model,
        from_toml.review.markdown_model
    );
    assert_eq!(from_yaml.review.asset_model, from_toml.review.asset_model);
    assert_eq!(
        from_yaml.review.markdown_concurrency,
        from_toml.review.markdown_concurrency
    );
    assert_eq!(from_yaml.backup_enabled(), from_toml.backup_enabled());

    let _ = fs::remove_dir_all(&directory);
}

/// A relative path is resolved against the directory the file lives in, in
/// either language, so the two templates produce byte-identical paths.
#[test]
fn both_languages_resolve_relative_paths_the_same_way() {
    let directory = scratch("relative");
    let yaml = directory.join("mineral.yaml");
    let toml = directory.join("mineral.toml");
    fs::write(&yaml, DEFAULT_CONFIG).unwrap();
    fs::write(&toml, DEFAULT_CONFIG_TOML).unwrap();

    let from_yaml = load(&yaml).unwrap();
    let from_toml = load(&toml).unwrap();

    assert!(from_yaml.state.path.is_absolute());
    assert_eq!(from_yaml.state.path, from_toml.state.path);
    assert_eq!(
        from_yaml.source.path.as_deref().unwrap().parent(),
        Some(directory.as_path())
    );

    let _ = fs::remove_dir_all(&directory);
}

/// The extension chooses the syntax; a file that promises neither is refused
/// rather than sniffed.
#[test]
fn a_configuration_file_must_name_its_language() {
    let directory = scratch("extension");
    let path = directory.join("mineral.conf");
    fs::write(&path, DEFAULT_CONFIG).unwrap();

    let error = load(&path).expect_err("an unknown extension must be refused");
    assert!(error.to_string().contains("toml"), "{error}");

    let _ = fs::remove_dir_all(&directory);
}

/// A typo is a refusal, in either language: a workspace must not silently
/// publish with the setting the author did not mean.
#[test]
fn a_misspelled_key_is_refused_in_either_language() {
    let directory = scratch("typo");

    let yaml = directory.join("mineral.yaml");
    fs::write(
        &yaml,
        DEFAULT_CONFIG.replace("  id: local-vault\n", "  ident: local-vault\n"),
    )
    .unwrap();
    let toml = directory.join("mineral.toml");
    fs::write(
        &toml,
        DEFAULT_CONFIG_TOML.replace("id = \"local-vault\"\n", "ident = \"local-vault\"\n"),
    )
    .unwrap();

    for path in [&yaml, &toml] {
        let error = load(path).expect_err("a misspelled key must be refused");
        assert!(
            error.to_string().contains("ident"),
            "{}: {error}",
            path.display()
        );
    }

    let _ = fs::remove_dir_all(&directory);
}

/// A validated configuration can be produced from a model with no file at all.
///
/// This is what makes the layer usable by a caller that already holds settings —
/// a test, an embedding host or a future web request — and it is why validation
/// is a pure function rather than a step inside file reading.
#[test]
fn a_model_can_be_validated_without_a_file() {
    let model: RawConfig = serde_yaml_ng::from_str(DEFAULT_CONFIG).unwrap();
    let config = ValidatedConfig::from_raw(model, "in-memory.yaml").unwrap();

    assert_eq!(config.source_kind(), SourceType::Local);
    assert!(config.state_path().is_absolute());
    assert_eq!(config.cas(), config.state_path().join("cas"));
    assert!(!config.backup_enabled());
    assert!(config.backup_git().is_err());
}

/// The validated configuration exposes credential *names*, never values.
#[test]
fn a_validated_configuration_names_credentials_and_never_carries_them() {
    let text = format!(
        "{DEFAULT_CONFIG}backup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_CONFIG_TEST_USER\n    token_env: MINERAL_CONFIG_TEST_TOKEN\n"
    );
    let model: RawConfig = serde_yaml_ng::from_str(&text).unwrap();
    let config = ValidatedConfig::from_raw(model, "in-memory.yaml").unwrap();

    let (username, token) = config.backup_lfs_credential_names().unwrap();
    assert_eq!(username.as_str(), "MINERAL_CONFIG_TEST_USER");
    assert_eq!(token.as_str(), "MINERAL_CONFIG_TEST_TOKEN");
    assert_eq!(
        config.review_api_key_name().unwrap().as_str(),
        "MINERAL_DEEPSEEK_API_KEY"
    );

    // The dump a log or an error report would carry names the variables and
    // nothing else.
    let dump = format!("{config:?}");
    assert!(dump.contains("MINERAL_CONFIG_TEST_TOKEN"), "{dump}");
}

/// Two languages, one rule: an empty credential name is refused while the
/// configuration is validated, in either file.
#[test]
fn an_unusable_credential_name_is_refused_while_the_configuration_loads() {
    let directory = scratch("secret-name");
    let toml = directory.join("mineral.toml");
    let text = format!(
        "{DEFAULT_CONFIG_TOML}\n[backup]\nenabled = true\n\n[backup.git]\nrepository = \"./backup-repo\"\nremote = \"origin\"\nbranch = \"refs/heads/mineral-backup\"\n\n[backup.lfs]\nenabled = true\nbatch_url = \"https://github.com/owner/repo.git/info/lfs\"\nusername_env = \"\"\ntoken_env = \"T\"\n"
    );
    fs::write(&toml, text).unwrap();

    let error = load(&toml).expect_err("an empty credential variable name must be refused");
    assert!(
        error.to_string().contains("backup.lfs.username_env"),
        "{error}"
    );

    let _ = fs::remove_dir_all(&directory);
}

/// A secret value never renders itself, whatever structure holds it.
#[test]
fn a_secret_value_never_appears_in_a_debug_dump() {
    let value = SecretValue::new("super-secret-token").unwrap();
    assert_eq!(value.expose(), "super-secret-token");
    assert!(!format!("{value:?}").contains("super-secret-token"));
    assert!(format!("{value:?}").contains("redacted"));

    // The name is safe to print, and printing it is how a missing credential is
    // reported without disclosing one.
    let name = SecretName::new("MINERAL_TEST_TOKEN").unwrap();
    assert_eq!(format!("{name}"), "MINERAL_TEST_TOKEN");
}

/// The static provider is the seam a test or an embedding host uses instead of
/// the process environment, and it answers both questions the trait asks.
#[test]
fn a_static_provider_resolves_values_and_reports_presence_by_name() {
    let provider = StaticSecretProvider::new().with("MINERAL_TEST_TOKEN", "value");
    let present = SecretName::new("MINERAL_TEST_TOKEN").unwrap();
    let absent = SecretName::new("MINERAL_TEST_UNSET_TOKEN").unwrap();

    assert_eq!(provider.resolve(&present).unwrap().expose(), "value");
    assert!(provider.is_present(&present));
    assert!(!provider.is_present(&absent));

    let error = provider.resolve(&absent).unwrap_err();
    assert!(
        error.to_string().contains("MINERAL_TEST_UNSET_TOKEN"),
        "{error}"
    );
    assert!(!error.to_string().contains("value"), "{error}");
}

/// The environment provider is the production one, and it fails closed by name.
#[test]
fn the_environment_provider_fails_closed_by_name() {
    let provider = EnvSecretProvider;
    let absent = SecretName::new("MINERAL_CONFIG_TEST_UNSET_VARIABLE").unwrap();
    assert!(!provider.is_present(&absent));
    let error = provider.resolve(&absent).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("MINERAL_CONFIG_TEST_UNSET_VARIABLE"),
        "{error}"
    );
}

/// An empty credential value is not a credential.
#[test]
fn an_empty_secret_value_is_refused() {
    assert!(SecretValue::new("").is_err());
    assert!(SecretName::new("   ").is_err());

    let provider = StaticSecretProvider::new().with("EMPTY", "");
    let name = SecretName::new("EMPTY").unwrap();
    assert!(provider.resolve(&name).is_err());
}

/// The format helper is what `mineral init` uses to name a fresh file, so the
/// name and the language are asked of one place.
#[test]
fn a_format_knows_its_extension_and_its_template() {
    assert_eq!(
        ConfigFormat::of(&PathBuf::from("a.toml")),
        Some(ConfigFormat::Toml)
    );
    assert_eq!(
        ConfigFormat::of(&PathBuf::from("a.yaml")),
        Some(ConfigFormat::Yaml)
    );
    assert_eq!(
        ConfigFormat::of(&PathBuf::from("a.yml")),
        Some(ConfigFormat::Yaml)
    );
    assert_eq!(ConfigFormat::of(&PathBuf::from("a.conf")), None);
    assert_eq!(ConfigFormat::Toml.extension(), "toml");
    assert!(ConfigFormat::Toml.template().contains("[source]"));
    assert!(ConfigFormat::Yaml.template().contains("source:"));

    // Either template parses as its own language.
    let _: RawConfig = toml::from_str(ConfigFormat::Toml.template()).unwrap();
    let _: RawConfig = serde_yaml_ng::from_str(ConfigFormat::Yaml.template()).unwrap();
}

/// A model with a source kind that contradicts its fields is refused by name.
#[test]
fn validation_refuses_a_contradictory_source() {
    let mut model: RawConfig = serde_yaml_ng::from_str(DEFAULT_CONFIG).unwrap();
    model.source = SourceConfig {
        id: "local-vault".to_owned(),
        kind: Some(SourceType::R2),
        path: Some(PathBuf::from("./vault")),
        r2: None,
    };
    let error = ValidatedConfig::from_raw(model, "in-memory.yaml")
        .expect_err("an R2 source with a local path must be refused");
    assert!(error.is_about("source.path"), "{error}");
}
