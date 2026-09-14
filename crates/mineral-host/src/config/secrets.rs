//! Credentials are *named* by a configuration file and *resolved* by a provider.
//!
//! Splitting the two is what keeps a secret out of the layers that must never
//! hold one. A validated configuration carries only [`SecretName`]s; the
//! application layer asks a [`SecretProvider`] for the value at the moment it
//! builds an adapter, and a [`SecretValue`] refuses to reveal itself in a log,
//! a report or a `Debug` dump. The environment is one provider, not the only
//! possible one, so a test or a future web host can supply its own.

use std::{collections::BTreeMap, env, error::Error, fmt};

/// The name of one credential, exactly as a configuration file spells it.
///
/// A name is a variable name, not a value: it is safe to print, and printing it
/// is how a missing credential is reported without ever disclosing one.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SecretName(String);

impl SecretName {
    /// Validates a configured credential name.
    ///
    /// A name that is empty or carries a control character cannot be a variable
    /// name on any platform this host runs on, and is refused while the
    /// configuration is validated rather than discovered as a failed lookup.
    pub fn new(name: impl Into<String>) -> Result<Self, SecretError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(SecretError::UnusableName {
                reason: "it is empty".to_owned(),
            });
        }
        if name.contains(['\0', '\n', '\r', '=']) {
            return Err(SecretError::UnusableName {
                reason: "it carries a control character or an `=`".to_owned(),
            });
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A resolved credential value.
///
/// The value is reachable only through [`SecretValue::expose`], so a stray
/// `{:?}` on any structure holding one prints a redaction instead of a token.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a resolved value.
    ///
    /// An empty value is refused: a provider that returns one has not resolved
    /// anything, and a request signed with an empty credential fails far from
    /// the configuration mistake that caused it.
    pub fn new(value: impl Into<String>) -> Result<Self, SecretError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SecretError::EmptyValue);
        }
        Ok(Self(value))
    }

    /// The credential itself. Call this only where a request is signed.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue(<redacted>)")
    }
}

/// Where a runtime resolves the credentials a workspace names.
///
/// The trait is the seam the whole layering rests on: the application layer
/// depends on this and never on `std::env`, so a web host can resolve its
/// credentials from a request-scoped store and a test can resolve them from
/// memory.
///
/// `Debug` is a supertrait on purpose. A provider holds values, so every
/// implementation must be safe to render in a dump or a log; requiring it here
/// means a new provider cannot be added without answering that question.
pub trait SecretProvider: Send + Sync + fmt::Debug {
    /// Resolves one credential, failing closed when it is not available.
    fn resolve(&self, name: &SecretName) -> Result<SecretValue, SecretError>;

    /// Whether one credential is available, without resolving it.
    ///
    /// `doctor` reports presence and must never read a value to do it.
    fn is_present(&self, name: &SecretName) -> bool;
}

/// Resolves credentials from the process environment.
///
/// This is the only place in the host that reads a credential from the
/// environment, and it reads it by name at the moment it is needed.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvSecretProvider;

impl SecretProvider for EnvSecretProvider {
    fn resolve(&self, name: &SecretName) -> Result<SecretValue, SecretError> {
        match env::var(name.as_str()) {
            Ok(value) => SecretValue::new(value),
            Err(_) => Err(SecretError::Missing { name: name.clone() }),
        }
    }

    fn is_present(&self, name: &SecretName) -> bool {
        env::var_os(name.as_str()).is_some()
    }
}

/// Resolves credentials from memory.
///
/// Tests and embedded hosts use this to supply a credential without touching the
/// process environment — which edition 2024 makes `unsafe`, and which this
/// project forbids.
#[derive(Clone, Default)]
pub struct StaticSecretProvider {
    values: BTreeMap<String, String>,
}

/// A provider holds values, so its `Debug` names the variables it can resolve
/// and never what they hold. A dump of a runtime must be safe to log.
impl fmt::Debug for StaticSecretProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticSecretProvider")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl StaticSecretProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supplies one credential. The value never leaves this provider except
    /// through [`SecretProvider::resolve`].
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(name.into(), value.into());
        self
    }
}

impl SecretProvider for StaticSecretProvider {
    fn resolve(&self, name: &SecretName) -> Result<SecretValue, SecretError> {
        match self.values.get(name.as_str()) {
            Some(value) => SecretValue::new(value.clone()),
            None => Err(SecretError::Missing { name: name.clone() }),
        }
    }

    fn is_present(&self, name: &SecretName) -> bool {
        self.values.contains_key(name.as_str())
    }
}

/// Why a credential could not be named or resolved.
///
/// The variants carry a *name*, never a value, so an error can be rendered,
/// logged and returned to a caller without disclosing anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    /// A configured credential name is not a usable variable name.
    UnusableName { reason: String },
    /// The provider has no value for a name the configuration requires.
    Missing { name: SecretName },
    /// The provider returned an empty value.
    EmptyValue,
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnusableName { reason } => {
                write!(formatter, "credential name is unusable: {reason}")
            }
            Self::Missing { name } => write!(formatter, "the variable {name} is not set"),
            Self::EmptyValue => formatter.write_str("the credential value is empty"),
        }
    }
}

impl Error for SecretError {}
