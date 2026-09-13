use std::{error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::domain::Sha256;

/// Version of the source-identity encoding.
///
/// It is hashed into every identity, so a change to how a namespace is described
/// produces different identities instead of silently re-interpreting an old
/// durable binding.
pub const SOURCE_IDENTITY_VERSION: u8 = 1;

/// Which kind of namespace a source identity describes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SourceKind {
    /// A directory tree on a local filesystem.
    LocalFilesystem,
    /// A namespace inside an S3-compatible object store.
    ObjectStore,
}

impl SourceKind {
    /// The stable, credential-free name hashed into a source identity.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalFilesystem => "local-filesystem",
            Self::ObjectStore => "object-store",
        }
    }
}

/// Stable identity of one source namespace.
///
/// It answers one question: "which remote namespace did this fact come from?" Two
/// namespaces that a runtime resolves to different objects must have different
/// identities, and two descriptions of the same namespace must hash to the same
/// identity on every machine.
///
/// A namespace descriptor is canonical, credential-free text supplied by the
/// adapter (for an object store: endpoint host, bucket and prefix). Credentials are
/// never part of it, and neither is anything that changes between runs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceIdentity(Sha256);

impl SourceIdentity {
    /// Derives the identity of one described namespace.
    pub fn of(kind: SourceKind, canonical_namespace: &str) -> Result<Self, SourceIdentityError> {
        if canonical_namespace.trim().is_empty() {
            return Err(SourceIdentityError::EmptyNamespace);
        }
        if canonical_namespace.chars().any(char::is_control) {
            return Err(SourceIdentityError::ControlCharacter);
        }

        let mut hasher = Sha256Hasher::new();
        hasher.update(b"mineral.source-identity");
        hasher.update([SOURCE_IDENTITY_VERSION]);
        for part in [kind.as_str(), canonical_namespace] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        Ok(Self(Sha256::new(hasher.finalize().into())))
    }

    /// Rebuilds the identity a durable record stored.
    ///
    /// An identity is opaque bytes: a reader cannot re-derive it without the
    /// descriptor, and it must not try to.
    pub const fn rehydrate(identity: Sha256) -> Self {
        Self(identity)
    }

    pub fn as_sha256(self) -> Sha256 {
        self.0
    }
}

impl fmt::Display for SourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Why a namespace descriptor cannot identify a source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceIdentityError {
    /// A namespace with no description cannot be distinguished from any other.
    EmptyNamespace,
    /// A control character in a namespace descriptor cannot be compared reliably.
    ControlCharacter,
}

impl fmt::Display for SourceIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyNamespace => formatter.write_str("source namespace must not be empty"),
            Self::ControlCharacter => {
                formatter.write_str("source namespace must not contain control characters")
            }
        }
    }
}

impl Error for SourceIdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespace_identity_is_stable_and_kind_scoped() {
        let first = SourceIdentity::of(SourceKind::ObjectStore, "bucket/vault/").unwrap();
        let same = SourceIdentity::of(SourceKind::ObjectStore, "bucket/vault/").unwrap();
        let other_prefix = SourceIdentity::of(SourceKind::ObjectStore, "bucket/backup/").unwrap();
        let other_bucket = SourceIdentity::of(SourceKind::ObjectStore, "other/vault/").unwrap();
        let other_kind = SourceIdentity::of(SourceKind::LocalFilesystem, "bucket/vault/").unwrap();

        assert_eq!(first, same);
        assert_ne!(first, other_prefix);
        assert_ne!(first, other_bucket);
        assert_ne!(first, other_kind);
    }

    #[test]
    fn an_empty_or_control_descriptor_is_refused() {
        assert_eq!(
            SourceIdentity::of(SourceKind::ObjectStore, "  ").unwrap_err(),
            SourceIdentityError::EmptyNamespace
        );
        assert_eq!(
            SourceIdentity::of(SourceKind::ObjectStore, "bucket/\u{7}vault").unwrap_err(),
            SourceIdentityError::ControlCharacter
        );
    }
}
