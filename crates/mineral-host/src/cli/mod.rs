//! The command line: parse arguments, call a use case, render its outcome.
//!
//! This module is an entry point, not a layer. It decides nothing about what a
//! publication or a backup *is*; it turns words into a request, hands the
//! request to the application layer, and turns the outcome into text.
//!
//! ```text
//! args.rs       words          →  Invocation
//! commands.rs   Invocation     →  application request
//! output.rs     application outcome → text
//! ```
//!
//! Deleting this directory would remove the terminal, not the capability: every
//! use case it calls is a library function, reachable and tested without it.

mod args;
mod commands;
mod output;

use std::{env, error::Error};

use mineral_publisher::runtime::WorkspaceRuntime;

use args::Invocation;

pub fn run() -> Result<(), Box<dyn Error>> {
    match args::parse(env::args().skip(1))? {
        Invocation::Help => {
            args::print_help();
            Ok(())
        }
        Invocation::Init { path, format } => commands::init(&path, format),
        Invocation::Publish { config } => commands::publish(WorkspaceRuntime::load(config)?),
        Invocation::Status { config } => commands::status(WorkspaceRuntime::load(config)?),
        Invocation::Doctor { config } => commands::doctor(WorkspaceRuntime::load(config)?),
        Invocation::Review { config, args } => {
            commands::review(WorkspaceRuntime::load(config)?, &args)
        }
        Invocation::Backup { config, args } => {
            commands::backup(WorkspaceRuntime::load(config)?, &args)
        }
        Invocation::Web {
            config,
            bind,
            assets,
        } => commands::web(
            WorkspaceRuntime::load(config)?,
            bind.as_deref(),
            assets.as_deref(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use mineral_publisher::{config::ConfigFormat, runtime::WorkspaceRuntime};

    use super::{
        args::{Invocation, parse},
        commands::init,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// The command line is a total function from words to an invocation.
    #[test]
    fn arguments_become_an_invocation() {
        assert!(matches!(
            parse(args(&[]).into_iter()).unwrap(),
            Invocation::Help
        ));
        assert!(matches!(
            parse(args(&["--config", "w.yaml", "publish"]).into_iter()).unwrap(),
            Invocation::Publish { config } if config == *"w.yaml"
        ));
        // TOML is the only supported default configuration format.
        assert!(matches!(
            parse(args(&["status"]).into_iter()).unwrap(),
            Invocation::Status { config } if config == *"mineral.toml"
        ));
        // `--toml` chooses both the language and, when unnamed, the file.
        assert!(matches!(
            parse(args(&["init", "--toml"]).into_iter()).unwrap(),
            Invocation::Init { path, format }
                if path == *"mineral.toml" && format == ConfigFormat::Toml
        ));
        assert!(matches!(
            parse(args(&["review", "show", "document:1"]).into_iter()).unwrap(),
            Invocation::Review { args, .. } if args == ["show", "document:1"]
        ));
        assert!(matches!(
            parse(args(&["backup", "verify"]).into_iter()).unwrap(),
            Invocation::Backup { args, .. } if args == ["verify"]
        ));
    }

    /// An unusable command line is refused rather than guessed.
    #[test]
    fn an_unusable_command_line_is_refused() {
        assert!(parse(args(&["--config"]).into_iter()).is_err());
        assert!(parse(args(&["explode"]).into_iter()).is_err());
    }

    /// `init` writes the language the file name promises, and refuses to write
    /// one whose name promises a different language.
    ///
    /// A `.yaml` filename is refused because TOML is the only configuration format.
    #[test]
    fn init_writes_the_language_the_file_name_promises() {
        let directory =
            std::env::temp_dir().join(format!("mineral-cli-init-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();

        let toml = directory.join("workspace.toml");
        init(&toml, ConfigFormat::Toml).unwrap();
        let text = std::fs::read_to_string(&toml).unwrap();
        assert!(text.contains("[source]"), "{text}");
        assert_eq!(
            WorkspaceRuntime::load(toml.clone()).unwrap().source_kind(),
            mineral_publisher::config::SourceType::Local
        );

        let mismatched = directory.join("workspace.yaml");
        let error = init(&mismatched, ConfigFormat::Toml)
            .expect_err("a .yaml name must not be written as TOML")
            .to_string();
        assert!(error.contains("toml"), "{error}");
        assert!(!mismatched.exists());

        let error = init(&toml, ConfigFormat::Toml)
            .expect_err("an existing configuration must never be overwritten")
            .to_string();
        assert!(error.contains("already exists"), "{error}");

        let _ = std::fs::remove_dir_all(&directory);
    }
}
