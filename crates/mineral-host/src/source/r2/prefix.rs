use std::fmt;

use crate::domain::{ContentPath, ContentPathError};

/// The managed namespace one R2 source reads from.
///
/// A prefix is required and canonical: it is non-empty, it is a canonical relative
/// path (the same contract a content path follows, so every key under it maps to a
/// canonical [`ContentPath`]), and it always ends in `/`. A configured value
/// without the trailing slash is canonicalized to the namespace form
/// (`vault` → `vault/`); anything that is not already canonical is refused rather
/// than normalized, because two spellings that mean the same namespace on one
/// machine must not mean two namespaces in a durable binding.
///
/// First version: a source may never be the bucket root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct R2SourcePrefix(String);

impl R2SourcePrefix {
    pub fn new(configured: &str) -> Result<Self, R2SourcePrefixError> {
        if configured.is_empty() {
            return Err(R2SourcePrefixError::Empty);
        }
        if configured.chars().any(char::is_control) {
            return Err(R2SourcePrefixError::ControlCharacter);
        }
        let body = configured.strip_suffix('/').unwrap_or(configured);
        if body.is_empty() {
            // The bucket root is deliberately not a valid source namespace.
            return Err(R2SourcePrefixError::Empty);
        }
        let path = ContentPath::new(body).map_err(R2SourcePrefixError::NotCanonical)?;
        Ok(Self(format!("{}/", path.as_str())))
    }

    /// The canonical prefix, always ending in `/`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Maps one remote key to its logical source path.
    ///
    /// The mapping is an exact suffix strip: no normalization, no separator
    /// rewriting, no trimming. A key that is not literally under the prefix, or
    /// whose suffix is not a canonical path, is refused — the whole inventory fails
    /// closed rather than skipping an object the operator believes is published.
    pub fn content_path(&self, key: &str) -> Result<ContentPath, R2SourceKeyError> {
        let suffix = key
            .strip_prefix(self.as_str())
            .ok_or(R2SourceKeyError::OutsidePrefix)?;
        ContentPath::new(suffix).map_err(R2SourceKeyError::NotCanonical)
    }

    /// Rebuilds the exact key a logical path came from.
    ///
    /// It is the inverse of [`content_path`](Self::content_path) by construction;
    /// readers assert the round trip anyway, so a mapping that ever stops being
    /// injective cannot silently read the wrong object.
    pub fn object_key(&self, path: &ContentPath) -> String {
        format!("{}{}", self.0, path.as_str())
    }
}

impl fmt::Display for R2SourcePrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a configured prefix is not a usable source namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum R2SourcePrefixError {
    /// An empty prefix would make the whole bucket the source.
    Empty,
    /// A control character cannot be compared or sent reliably.
    ControlCharacter,
    /// The prefix is not a canonical relative path.
    NotCanonical(ContentPathError),
}

impl fmt::Display for R2SourcePrefixError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str(
                "an R2 source prefix is required and may not be empty or cover the whole bucket",
            ),
            Self::ControlCharacter => {
                formatter.write_str("an R2 source prefix must not contain control characters")
            }
            Self::NotCanonical(error) => {
                write!(
                    formatter,
                    "an R2 source prefix must be a canonical path: {error}"
                )
            }
        }
    }
}

impl std::error::Error for R2SourcePrefixError {}

/// Why one remote key cannot become a logical source path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum R2SourceKeyError {
    /// The key is not under the configured prefix.
    OutsidePrefix,
    /// The suffix is not a canonical content path.
    NotCanonical(ContentPathError),
}

impl fmt::Display for R2SourceKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsidePrefix => {
                formatter.write_str("remote key is not under the configured source prefix")
            }
            Self::NotCanonical(error) => write!(
                formatter,
                "remote key does not map to a canonical source path: {error}"
            ),
        }
    }
}

impl std::error::Error for R2SourceKeyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(configured: &str) -> R2SourcePrefix {
        R2SourcePrefix::new(configured).unwrap()
    }

    #[test]
    fn a_prefix_without_a_trailing_slash_is_canonicalized_to_the_namespace_form() {
        assert_eq!(prefix("vault").as_str(), "vault/");
        assert_eq!(prefix("vault/").as_str(), "vault/");
        assert_eq!(prefix("a/b").as_str(), "a/b/");
    }

    #[test]
    fn an_unusable_prefix_is_refused_instead_of_normalized() {
        for configured in [
            "",
            "/",
            "//",
            "vault//",
            "vault/./",
            "vault/../",
            "/absolute",
            "back\\slash",
            "C:/vault",
            "va\u{0}ult",
            "vault\n",
        ] {
            assert!(
                R2SourcePrefix::new(configured).is_err(),
                "{configured:?} was accepted"
            );
        }
    }

    #[test]
    fn keys_map_by_an_exact_suffix_strip() {
        let prefix = prefix("vault/");

        assert_eq!(
            prefix.content_path("vault/index.md").unwrap().as_str(),
            "index.md"
        );
        assert_eq!(
            prefix.content_path("vault/notes/a.md").unwrap().as_str(),
            "notes/a.md"
        );
        assert_eq!(
            prefix.content_path("vault/旅行/照片.png").unwrap().as_str(),
            "旅行/照片.png"
        );
    }

    #[test]
    fn a_key_that_is_not_a_canonical_suffix_fails_closed() {
        let prefix = prefix("vault/");

        for key in [
            "vault//empty-segment.md",
            "vault/./dot.md",
            "vault/../escape.md",
            "vault/back\\slash.md",
            "vaultX/outside.md",
            "other/index.md",
        ] {
            assert!(prefix.content_path(key).is_err(), "{key:?} was accepted");
        }
    }

    #[test]
    fn the_key_of_a_path_is_the_inverse_of_the_path_of_a_key() {
        let prefix = prefix("vault/");

        for key in [
            "vault/index.md",
            "vault/notes/a.md",
            "vault/旅行/照片.png",
            "vault/empty.md",
        ] {
            let path = prefix.content_path(key).unwrap();
            assert_eq!(prefix.object_key(&path), key);
        }
    }
}
