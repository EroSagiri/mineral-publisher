//! Reading the TOML workspace configuration file.
//!
//! Format is chosen by extension and never guessed from content: a file that
//! claims to be TOML is parsed as TOML, so a syntax error is reported against
//! the language the author wrote rather than the one a sniffing heuristic
//! happened to prefer.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    model::RawConfig,
    validate::{ConfigError, ValidatedConfig},
};

/// The supported configuration syntax.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigFormat {
    /// TOML, read from a `.toml` file.
    Toml,
}

impl ConfigFormat {
    /// The format a path's extension selects, if it selects one.
    pub fn of(path: &Path) -> Option<Self> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("toml") => Some(Self::Toml),
            _ => None,
        }
    }

    /// The extension a fresh file in this format is written with.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Toml => "toml",
        }
    }

    /// The commented template a fresh workspace is born with.
    pub fn template(self) -> &'static str {
        match self {
            Self::Toml => super::model::DEFAULT_CONFIG_TOML,
        }
    }
}

/// Reads, parses, validates and normalizes one configuration file.
pub fn load(path: &Path) -> Result<ValidatedConfig, ConfigError> {
    let format = ConfigFormat::of(path).ok_or_else(|| ConfigError::Format {
        path: path.to_path_buf(),
        message: "configuration must be a .toml file".to_owned(),
    })?;
    parse(path, format)
}

/// Reads and parses one configuration file in a known format.
pub fn parse(path: &Path, format: ConfigFormat) -> Result<ValidatedConfig, ConfigError> {
    let bytes = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let model: RawConfig = match format {
        ConfigFormat::Toml => toml::from_str(&bytes).map_err(|error| ConfigError::Format {
            path: PathBuf::from(path),
            message: error.to_string(),
        })?,
    };
    ValidatedConfig::from_raw(model, path)
}
