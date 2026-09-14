//! Words to an invocation.
//!
//! The command line is parsed once, here, into a value. Nothing below this
//! module looks at `std::env::args`, so a command can be exercised by building
//! the invocation a caller would have typed.

use std::{error::Error, path::PathBuf};

use mineral_publisher::config::ConfigFormat;

/// One parsed command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invocation {
    /// Print the usage text.
    Help,
    /// Create a fresh workspace in one language.
    Init { path: PathBuf, format: ConfigFormat },
    /// Publish the current source state.
    Publish { config: PathBuf },
    /// Report the workspace's state.
    Status { config: PathBuf },
    /// Scan the workspace for health.
    Doctor { config: PathBuf },
    /// Inspect or decide the human review queue.
    Review { config: PathBuf, args: Vec<String> },
    /// Run or inspect the private backup.
    Backup { config: PathBuf, args: Vec<String> },
}

/// Parses the arguments after the program name.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Invocation, Box<dyn Error>> {
    let mut args = args.collect::<Vec<_>>();
    let (config_path, configured) = if args.first().is_some_and(|arg| arg == "--config") {
        if args.len() < 2 {
            return Err("--config requires a path".into());
        }
        let value = PathBuf::from(args.remove(1));
        args.remove(0);
        (value, true)
    } else {
        (default_config_path(), false)
    };
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(Invocation::Help);
    };
    let rest = args[1..].to_vec();
    match command {
        "init" => {
            // A fresh workspace may be written in either language. The flag also
            // decides the name of the file when the operator did not choose one,
            // so `mineral init --toml` cannot silently write YAML to
            // `mineral.toml`.
            let format = if rest.iter().any(|arg| arg == "--toml") {
                ConfigFormat::Toml
            } else {
                ConfigFormat::Yaml
            };
            let path = if format == ConfigFormat::Toml && !configured {
                PathBuf::from(format!("mineral.{}", format.extension()))
            } else {
                config_path
            };
            Ok(Invocation::Init { path, format })
        }
        "publish" => Ok(Invocation::Publish {
            config: config_path,
        }),
        "status" => Ok(Invocation::Status {
            config: config_path,
        }),
        "doctor" => Ok(Invocation::Doctor {
            config: config_path,
        }),
        "review" => Ok(Invocation::Review {
            config: config_path,
            args: rest,
        }),
        "backup" => Ok(Invocation::Backup {
            config: config_path,
            args: rest,
        }),
        "help" | "--help" | "-h" => Ok(Invocation::Help),
        _ => Err(format!("unknown command: {command}").into()),
    }
}

/// The configuration a command uses when the operator names none.
///
/// `mineral.yaml` stays the default, exactly as it always was. A workspace that
/// was created with `mineral init --toml` is found too, so the language a file is
/// written in never has to be repeated on every command line.
pub fn default_config_path() -> PathBuf {
    let yaml = PathBuf::from("mineral.yaml");
    if !yaml.exists() {
        let toml = PathBuf::from("mineral.toml");
        if toml.exists() {
            return toml;
        }
    }
    yaml
}

pub fn print_help() {
    println!("{}", usage());
}

/// The usage text, as data, so a test can assert on it without a terminal.
pub fn usage() -> String {
    [
        "Mineral Publisher",
        "",
        "Usage:",
        "  mineral [--config PATH] init [--toml]",
        "  mineral [--config PATH] publish",
        "  mineral [--config PATH] status",
        "  mineral [--config PATH] review list",
        "  mineral [--config PATH] review show <document:ID|asset:ID>",
        "  mineral [--config PATH] review approve <document:ID|asset:ID>",
        "  mineral [--config PATH] review reject <document:ID|asset:ID>",
        "  mineral [--config PATH] backup",
        "  mineral [--config PATH] backup status",
        "  mineral [--config PATH] backup verify",
        "  mineral [--config PATH] backup init",
        "  mineral [--config PATH] doctor",
    ]
    .join("\n")
}
