use std::{error::Error, fmt};

/// Stable identity of one configured publication target.
///
/// This is the name the audit trail records (for example `public-production`).
/// How that name maps to a concrete repository, remote, ref, or bucket is
/// runtime configuration owned by the host or Cloudflare adapter — never by the
/// engine, which only needs a stable identity it can compare and persist.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PublishTargetId(String);

impl PublishTargetId {
    pub fn new(value: impl Into<String>) -> Result<Self, PublishTargetIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PublishTargetIdError::Empty);
        }
        if value.contains(['\0', '\n', '\r']) {
            return Err(PublishTargetIdError::InvalidCharacters);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PublishTargetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishTargetIdError {
    Empty,
    InvalidCharacters,
}

impl fmt::Display for PublishTargetIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("publish target id cannot be empty"),
            Self::InvalidCharacters => {
                formatter.write_str("publish target id cannot contain control characters")
            }
        }
    }
}

impl Error for PublishTargetIdError {}

/// An opaque, runtime-resolved locator for the publication target.
///
/// The engine treats this as a string: it never splits it, canonicalizes it, or
/// interprets it as a filesystem path. A native runtime stores the canonical
/// local repository path here; a Cloudflare runtime would store its own locator
/// (for example a remote URL or bucket reference).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryLocator(String);

impl RepositoryLocator {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryLocatorError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RepositoryLocatorError::Empty);
        }
        if value.contains('\0') {
            return Err(RepositoryLocatorError::InvalidCharacters);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryLocatorError {
    Empty,
    InvalidCharacters,
}

impl fmt::Display for RepositoryLocatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("repository locator cannot be empty"),
            Self::InvalidCharacters => {
                formatter.write_str("repository locator cannot contain NUL characters")
            }
        }
    }
}

impl Error for RepositoryLocatorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_empty_and_control_characters() {
        assert!(PublishTargetId::new("  ").is_err());
        assert!(PublishTargetId::new("public\nproduction").is_err());
        assert!(RepositoryLocator::new("").is_err());
        assert!(RepositoryLocator::new("repo\0name").is_err());
    }

    #[test]
    fn identifiers_are_compared_by_exact_value() {
        assert_eq!(
            PublishTargetId::new("public-production").unwrap(),
            PublishTargetId::new("public-production").unwrap()
        );
        assert_ne!(
            PublishTargetId::new("public-production").unwrap(),
            PublishTargetId::new("private-backup").unwrap()
        );
        assert_eq!(
            RepositoryLocator::new("/srv/public-repo").unwrap().as_str(),
            "/srv/public-repo"
        );
    }
}
