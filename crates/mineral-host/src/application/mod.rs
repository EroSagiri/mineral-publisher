//! The application layer: one use case per module.
//!
//! Every module here answers one question — publish, back up, review, report
//! status, scan for health — with a structured request and a structured
//! outcome. Three rules hold across all of them, and they are what make the
//! layer reusable by an entry point that is not a terminal:
//!
//! 1. **No printing.** A use case reports progress through a
//!    [`crate::runtime::Progress`] sink and everything else through its outcome.
//! 2. **No ambient input.** The clock arrives in the request, credentials come
//!    from the runtime's secret provider, and nothing reads `std::env`.
//! 3. **Typed failure.** A caller distinguishes "the configuration is wrong"
//!    from "the credential is missing" from "the remote moved" by matching an
//!    enum, never by parsing a message.
//!
//! The outcome carries everything a renderer needs, so the CLI turns it into
//! text and a web host would turn the same value into JSON.

pub mod backup;
pub mod doctor;
pub mod publish;
pub mod review;
pub mod status;
#[cfg(test)]
mod tests;

use std::{error::Error, fmt};

use crate::{config::ConfigError, runtime::RuntimeError};

/// Why one use case could not complete.
///
/// The variants are the distinctions a caller acts on: a runtime failure is a
/// deployment problem, an operation failure is a data or transport problem, an
/// unsupported workspace is a configuration choice, and a missing or conflicting
/// subject is a request that can be corrected.
#[derive(Debug)]
pub enum ApplicationError {
    /// The workspace could not be loaded or a capability could not be built.
    Runtime(RuntimeError),
    /// A durable store, the engine or a remote refused the work.
    ///
    /// The cause is boxed rather than enumerated because the engine's own error
    /// types are generic over the stores they were built from; the *step* that
    /// failed is what a caller classifies on.
    Operation {
        operation: &'static str,
        source: Box<dyn Error>,
    },
    /// This workspace does not support the requested use case.
    Unsupported { message: String },
    /// A subject named by the request does not exist.
    NotFound { subject: String },
    /// A subject exists but is in a state that forbids the change.
    Conflict { message: String },
}

impl ApplicationError {
    /// Wraps a failure from one named step.
    pub fn operation(operation: &'static str, source: impl Error + 'static) -> Self {
        Self::Operation {
            operation,
            source: Box::new(source),
        }
    }

    /// Whether this failure is about a named configuration key or subject.
    pub fn is_about(&self, key: &str) -> bool {
        match self {
            Self::Runtime(error) => error.is_about(key),
            Self::Operation { operation, source } => {
                operation.contains(key) || source.to_string().contains(key)
            }
            Self::Unsupported { message } | Self::Conflict { message } => message.contains(key),
            Self::NotFound { subject } => subject.contains(key),
        }
    }
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Operation { operation, source } => {
                write!(formatter, "{operation} failed: {source}")
            }
            Self::Unsupported { message } => formatter.write_str(message),
            Self::NotFound { subject } => write!(formatter, "{subject} not found"),
            Self::Conflict { message } => formatter.write_str(message),
        }
    }
}

impl Error for ApplicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Operation { source, .. } => Some(source.as_ref()),
            Self::Unsupported { .. } | Self::NotFound { .. } | Self::Conflict { .. } => None,
        }
    }
}

impl From<RuntimeError> for ApplicationError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// A validated-configuration refusal is a runtime failure: the workspace itself
/// is not usable for the requested work.
impl From<ConfigError> for ApplicationError {
    fn from(error: ConfigError) -> Self {
        Self::Runtime(RuntimeError::from(error))
    }
}
