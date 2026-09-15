//! Runtime construction tests using TOML workspace files.

use std::{fs, path::PathBuf, sync::Arc};

use crate::{
    config::{RawConfig, StaticSecretProvider, ValidatedConfig},
    runtime::WorkspaceRuntime,
};

const WORKSPACE: &str = r#"
[source]
id = "local-vault"
path = "./vault"

[state]
path = "./.mineral"

[git]
repository = "./publication"
remote = "origin"
reference = "refs/heads/main"
author_name = "Bot"
author_email = "bot@example.invalid"
message = "Publish Mineral content"

[assets]
public_base_url = "https://assets.example.com"
target_path = "./asset-target"

[review]
api_base_url = "https://api.deepseek.com"
markdown_model = "deepseek-flash"
asset_model = "deepseek-flash"
timeout_seconds = 45
"#;

fn workspace(name: &str, text: &str) -> (PathBuf, ValidatedConfig) {
    let directory = std::env::temp_dir().join(format!("mineral-runtime-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("mineral.toml");
    fs::write(&path, text).unwrap();
    let model: RawConfig = toml::from_str(text).unwrap();
    (path.clone(), ValidatedConfig::from_raw(model, &path).unwrap())
}

#[test]
fn runtime_loads_a_toml_workspace() {
    let (path, config) = workspace("toml", WORKSPACE);
    assert!(config.state_path().is_absolute());
    assert_eq!(WorkspaceRuntime::load(path).unwrap().source_kind(), crate::config::SourceType::Local);
}

#[test]
fn runtime_uses_the_supplied_secret_provider() {
    let text = WORKSPACE.replace(
        "timeout_seconds = 45",
        "api_key = \"runtime-test-key\"\ntimeout_seconds = 45",
    );
    let (path, config) = workspace("provider", &text);
    let runtime = WorkspaceRuntime::new(
        config,
        path,
        Arc::new(StaticSecretProvider::new().with("__mineral_inline_review_api_key", "runtime-test-key")),
    );
    assert!(runtime.markdown_reviewer(Arc::new(crate::runtime::NoProgress)).is_ok());
}

#[test]
fn non_toml_workspace_names_are_refused() {
    let (path, _) = workspace("extension", WORKSPACE);
    let unsupported = path.with_extension("conf");
    fs::rename(&path, &unsupported).unwrap();
    assert!(WorkspaceRuntime::load(unsupported).is_err());
}
