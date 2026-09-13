use std::fmt;

use crate::domain::{ContentPath, ContentPathError};

/// The namespace one R2 source reads from.
///
/// A prefix is required and canonical: it is a canonical relative path (the same
/// contract a content path follows, so every key under it maps to a canonical
/// [`ContentPath`]) and it always ends in `/`. A configured value without the
/// trailing slash is canonicalized to the namespace form (`vault` → `vault/`);
/// anything that is not already canonical is refused rather than normalized,
/// because two spellings that mean the same namespace on one machine must not mean
/// two namespaces in a durable binding.
///
/// The bucket root is available, but only when it is asked for explicitly: the
/// empty prefix means "every object in the bucket", while `/`, `//` and a missing
/// value are still refused, so a source never becomes read-wide by accident. The
/// composition root additionally refuses a root source that shares its bucket with
/// the publication namespace, because that would make this engine read its own
/// output back as input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct R2SourcePrefix(String);

/// The explicit spelling of the bucket root.
pub const ROOT_SOURCE_PREFIX: &str = "";

impl R2SourcePrefix {
    pub fn new(configured: &str) -> Result<Self, R2SourcePrefixError> {
        if configured == ROOT_SOURCE_PREFIX {
            return Ok(Self(String::new()));
        }
        if configured.chars().any(char::is_control) {
            return Err(R2SourcePrefixError::ControlCharacter);
        }
        let body = configured.strip_suffix('/').unwrap_or(configured);
        if body.is_empty() {
            // `/` and `//` name the root ambiguously; `""` is the explicit spelling.
            return Err(R2SourcePrefixError::NotCanonical(
                ContentPathError::NotCanonical,
            ));
        }
        let path = ContentPath::new(body).map_err(R2SourcePrefixError::NotCanonical)?;
        Ok(Self(format!("{}/", path.as_str())))
    }

    /// The canonical prefix: empty for the bucket root, otherwise ending in `/`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this source reads the whole bucket.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
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
    /// A control character cannot be compared or sent reliably.
    ControlCharacter,
    /// The prefix is not a canonical relative path (the bucket root is `""`).
    NotCanonical(ContentPathError),
}

impl fmt::Display for R2SourcePrefixError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlCharacter => {
                formatter.write_str("an R2 source prefix must not contain control characters")
            }
            Self::NotCanonical(error) => {
                write!(
                    formatter,
                    "an R2 source prefix must be a canonical path, or the explicit empty \
                     prefix for the bucket root: {error}"
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
    fn the_bucket_root_is_available_only_as_the_explicit_empty_prefix() {
        let root = R2SourcePrefix::new(ROOT_SOURCE_PREFIX).unwrap();

        assert!(root.is_root());
        assert_eq!(root.as_str(), "");
        assert_eq!(root.content_path("a/b.md").unwrap().as_str(), "a/b.md");
        assert_eq!(
            root.content_path(".history/x.md").unwrap().as_str(),
            ".history/x.md",
            "a root source still maps every key to a canonical path"
        );
        let path = ContentPath::new("notes/a.md").unwrap();
        assert_eq!(root.object_key(&path), "notes/a.md");

        // Every other spelling of "the whole bucket" is refused, so a source never
        // becomes read-wide by accident.
        for configured in ["/", "//", "///"] {
            assert!(
                R2SourcePrefix::new(configured).is_err(),
                "{configured:?} was accepted as a root prefix"
            );
        }
    }

    #[test]
    fn a_root_prefix_still_refuses_keys_that_are_not_canonical_paths() {
        let root = R2SourcePrefix::new(ROOT_SOURCE_PREFIX).unwrap();

        for key in [
            "a//b.md",
            "./a.md",
            "../a.md",
            "a/../b.md",
            "back\\slash.md",
            "/absolute.md",
        ] {
            assert!(root.content_path(key).is_err(), "{key:?} was accepted");
        }
    }

    #[test]
    fn an_unusable_prefix_is_refused_instead_of_normalized() {
        for configured in [
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
