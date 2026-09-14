//! Three layers, one direction: file → [`RawConfig`] → [`ValidatedConfig`] →
//! secrets.
//!
//! ```text
//! mineral.toml / mineral.yaml
//!       │  load::load          extension chooses the syntax
//!       ▼
//! RawConfig                     what the file said, and nothing more
//!       │  validate::from_raw  pure: absolutize, canonicalize, refuse
//!       ▼
//! ValidatedConfig               usable by construction
//!       │  secrets::SecretProvider
//!       ▼
//! Runtime                       adapters built with resolved credentials
//! ```
//!
//! Nothing in this module reads a credential, opens a database, or touches the
//! network. Validation may read the current directory to absolutize a relative
//! path; that is the only ambient input it takes.

pub mod load;
pub mod model;
pub mod secrets;
#[cfg(test)]
mod tests;
pub mod validate;

pub use load::{ConfigFormat, load, parse};
pub use model::{
    DEFAULT_BACKUP_AUTHOR_EMAIL, DEFAULT_BACKUP_AUTHOR_NAME, DEFAULT_BACKUP_LFS_TIMEOUT_SECONDS,
    DEFAULT_BACKUP_MESSAGE, DEFAULT_CONFIG, DEFAULT_CONFIG_TOML, RawConfig, SourceType,
};
pub use secrets::{
    EnvSecretProvider, SecretError, SecretName, SecretProvider, SecretValue, StaticSecretProvider,
};
pub use validate::{ConfigError, ValidatedConfig};
