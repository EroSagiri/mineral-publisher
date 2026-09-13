use std::{error::Error, fmt};

/// The revision encoding this engine writes today.
///
/// A durable revision always carries its encoding version, so a future adapter can
/// change what it records without an old row being read as if it meant the new
/// thing.
pub const SOURCE_REVISION_VERSION: &str = "v1";

/// How long one encoded revision may be, in bytes.
///
/// An adapter records a handful of short remote facts, never a document or a
/// digest of one; the bound exists so a damaged or hostile row cannot be used to
/// allocate without limit.
pub const MAX_SOURCE_REVISION_LENGTH: usize = 512;

/// Opaque identity of one observed remote source revision.
///
/// The engine can compare, persist and validate a revision, and that is all: it
/// never interprets one. A revision is **not** a content hash — a remote store's
/// ETag, object version or upload time describe a generation of a remote object,
/// not the bytes this engine hashed — so the engine deliberately offers no
/// conversion from a revision to a [`crate::domain::Sha256`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceRevision(String);

impl SourceRevision {
    /// Builds one revision from its adapter-owned payload.
    ///
    /// The payload is opaque to the engine: it is stored, compared and validated,
    /// never decoded.
    pub fn versioned(payload: impl AsRef<str>) -> Result<Self, SourceRevisionError> {
        let payload = payload.as_ref();
        Self::new(format!("{SOURCE_REVISION_VERSION}:{payload}"))
    }

    /// Validates one already-encoded revision.
    pub fn new(encoded: impl Into<String>) -> Result<Self, SourceRevisionError> {
        let encoded = encoded.into();
        if encoded.is_empty() {
            return Err(SourceRevisionError::Empty);
        }
        if encoded.len() > MAX_SOURCE_REVISION_LENGTH {
            return Err(SourceRevisionError::TooLong);
        }
        if encoded.chars().any(char::is_control) {
            return Err(SourceRevisionError::ControlCharacter);
        }
        Ok(Self(encoded))
    }

    /// Reads a durable revision, refusing an encoding this engine does not know.
    pub fn decode(encoded: &str) -> Result<Self, SourceRevisionError> {
        let revision = Self::new(encoded)?;
        if revision.version() != SOURCE_REVISION_VERSION {
            return Err(SourceRevisionError::UnknownVersion);
        }
        Ok(revision)
    }

    /// The durable encoding, `"<version>:<payload>"`.
    pub fn encoded(&self) -> &str {
        &self.0
    }

    /// The encoding version this revision was written with.
    pub fn version(&self) -> &str {
        self.0.split_once(':').map_or("", |(version, _)| version)
    }

    /// The adapter-owned payload, without its version prefix.
    pub fn payload(&self) -> &str {
        self.0
            .split_once(':')
            .map_or(&self.0, |(_, payload)| payload)
    }
}

impl fmt::Display for SourceRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why an encoded revision is not usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceRevisionError {
    /// An empty revision cannot be compared with anything.
    Empty,
    /// A revision longer than [`MAX_SOURCE_REVISION_LENGTH`] is not a revision.
    TooLong,
    /// A control character cannot be stored or compared reliably.
    ControlCharacter,
    /// The revision was written by an encoding this engine does not know.
    UnknownVersion,
}

impl fmt::Display for SourceRevisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("source revision must not be empty"),
            Self::TooLong => formatter.write_str("source revision is too long"),
            Self::ControlCharacter => {
                formatter.write_str("source revision must not contain control characters")
            }
            Self::UnknownVersion => {
                formatter.write_str("source revision uses an unknown encoding version")
            }
        }
    }
}

impl Error for SourceRevisionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_round_trips_through_its_versioned_encoding() {
        let revision = SourceRevision::versioned("etag=abc;size=3").unwrap();

        assert_eq!(revision.encoded(), "v1:etag=abc;size=3");
        assert_eq!(revision.version(), "v1");
        assert_eq!(revision.payload(), "etag=abc;size=3");
        assert_eq!(
            SourceRevision::decode(revision.encoded()).unwrap(),
            revision
        );
    }

    /// A remote fact that looks like a content digest is still not a content
    /// digest: the engine only accepts it as an opaque revision.
    #[test]
    fn a_digest_shaped_remote_fact_is_only_a_revision() {
        let etag = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        assert_eq!(
            SourceRevision::decode(etag).unwrap_err(),
            SourceRevisionError::UnknownVersion
        );
        let revision = SourceRevision::versioned(etag).unwrap();
        assert_eq!(revision.payload(), etag);
        assert_eq!(revision.version(), SOURCE_REVISION_VERSION);
    }

    #[test]
    fn an_unknown_encoding_or_damaged_row_fails_closed() {
        assert_eq!(
            SourceRevision::decode("v2:etag=abc").unwrap_err(),
            SourceRevisionError::UnknownVersion
        );
        assert_eq!(
            SourceRevision::new("").unwrap_err(),
            SourceRevisionError::Empty
        );
        assert_eq!(
            SourceRevision::new("v1:bad\nrevision").unwrap_err(),
            SourceRevisionError::ControlCharacter
        );
        assert_eq!(
            SourceRevision::new("x".repeat(MAX_SOURCE_REVISION_LENGTH + 1)).unwrap_err(),
            SourceRevisionError::TooLong
        );
    }
}
