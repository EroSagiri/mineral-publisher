use std::error::Error;

use crate::domain::Sha256;

use crate::workflow::{AssetContentType, AssetObjectKey, PublishedAsset};

/// What one asset target reports about one frozen object key.
///
/// A runtime reports facts; it never reports a verdict. Whether an object is
/// acceptable is decided by [`PublishedAsset::judge`] in the engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetTargetState {
    Missing,
    Present(AssetTargetFacts),
}

/// The facts one asset target observed for one object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetTargetFacts {
    object_key: AssetObjectKey,
    size: u64,
    content_type: AssetContentType,
    bytes: AssetByteIdentity,
}

impl AssetTargetFacts {
    pub fn new(
        object_key: AssetObjectKey,
        size: u64,
        content_type: AssetContentType,
        bytes: AssetByteIdentity,
    ) -> Self {
        Self {
            object_key,
            size,
            content_type,
            bytes,
        }
    }

    pub fn object_key(&self) -> &AssetObjectKey {
        &self.object_key
    }

    /// The size the target reports for the object it serves.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The media type the target reports for the object it serves.
    pub fn content_type(&self) -> &AssetContentType {
        &self.content_type
    }

    pub fn bytes(&self) -> AssetByteIdentity {
        self.bytes
    }
}

/// How a target came to know the byte identity of an object.
///
/// An object store's ETag is **not** a SHA-256 of its content, so it must never
/// appear here. The only values a target may report are ones it derived from the
/// object's own bytes, or metadata this publisher wrote alongside the object —
/// and the two are deliberately different variants, because they support
/// different claims about the bytes the target serves right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetByteIdentity {
    /// The runtime read the object's bytes and hashed them.
    Verified(Sha256),
    /// The runtime read metadata this publisher stored with the object. It is an
    /// attestation of what the publisher believed it placed — evidence about the
    /// object's *current* bytes it is not.
    Attested(Sha256),
    /// The runtime could not produce any byte identity for the object.
    Unavailable,
}

/// The engine's judgement of one target fact set against one frozen asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetVerification {
    /// The target holds nothing under the frozen object key.
    Missing,
    /// The frozen object key holds exactly the frozen representation, proven from
    /// the object's own bytes.
    Ready,
    /// The frozen object key holds something else. A content-addressed key can
    /// only ever mean one thing, so this is corruption and never something to
    /// overwrite.
    Conflict(AssetTargetConflict),
    /// The object is present with matching metadata, but the target cannot prove
    /// which bytes it currently serves.
    Unverifiable,
}

impl AssetVerification {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// The exact way the frozen object key disagreed with the frozen facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetTargetConflict {
    ObjectKey {
        expected: AssetObjectKey,
        observed: AssetObjectKey,
    },
    Size {
        expected: u64,
        observed: u64,
    },
    ContentType {
        expected: AssetContentType,
        observed: AssetContentType,
    },
    Bytes {
        expected: Sha256,
        observed: Sha256,
    },
}

impl PublishedAsset {
    /// Judges one target fact set against this asset's frozen publication facts.
    ///
    /// Every comparison happens here, in the engine: the object key, the size, the
    /// media type, and — when the target can report one — the identity of the
    /// bytes actually served. A publication is authorized by a verified byte
    /// identity, never by an attestation and never by a key name.
    pub fn judge(&self, state: &AssetTargetState) -> AssetVerification {
        let AssetTargetState::Present(facts) = state else {
            return AssetVerification::Missing;
        };
        if facts.object_key != *self.object_key() {
            return AssetVerification::Conflict(AssetTargetConflict::ObjectKey {
                expected: self.object_key().clone(),
                observed: facts.object_key.clone(),
            });
        }
        if facts.size != self.published_size() {
            return AssetVerification::Conflict(AssetTargetConflict::Size {
                expected: self.published_size(),
                observed: facts.size,
            });
        }
        if facts.content_type != *self.published_content_type() {
            return AssetVerification::Conflict(AssetTargetConflict::ContentType {
                expected: self.published_content_type().clone(),
                observed: facts.content_type.clone(),
            });
        }
        match facts.bytes {
            AssetByteIdentity::Verified(observed) if observed == self.published_sha256() => {
                AssetVerification::Ready
            }
            AssetByteIdentity::Verified(observed) => {
                AssetVerification::Conflict(AssetTargetConflict::Bytes {
                    expected: self.published_sha256(),
                    observed,
                })
            }
            // The publisher's own metadata claims the right bytes, and it
            // disagrees about anything else. A claim is not a proof, so a match
            // still cannot authorize a publication.
            AssetByteIdentity::Attested(observed) if observed == self.published_sha256() => {
                AssetVerification::Unverifiable
            }
            AssetByteIdentity::Attested(observed) => {
                AssetVerification::Conflict(AssetTargetConflict::Bytes {
                    expected: self.published_sha256(),
                    observed,
                })
            }
            AssetByteIdentity::Unavailable => AssetVerification::Unverifiable,
        }
    }

    /// Proves that `bytes` are exactly this asset's frozen published
    /// representation.
    ///
    /// The returned value is the only thing an [`crate::publication::asset::AssetTarget`]
    /// may be asked to place, and it borrows the very bytes that were checked, so
    /// a target cannot upload a second read that was never verified.
    ///
    /// This is the V1 in-memory seam: a runtime whose source is a stream adds a
    /// streamed constructor here without weakening the invariant that only a
    /// verified representation is ever published.
    pub fn verify_bytes<'a>(
        &'a self,
        bytes: &'a [u8],
    ) -> Result<VerifiedAssetContent<'a>, AssetContentError> {
        let actual_size = bytes.len() as u64;
        if actual_size != self.published_size() {
            return Err(AssetContentError::SizeMismatch {
                logical_path: self.logical_path().clone(),
                expected: self.published_size(),
                actual: actual_size,
            });
        }
        let actual = Sha256::digest(bytes);
        if actual != self.published_sha256() {
            return Err(AssetContentError::IdentityMismatch {
                logical_path: self.logical_path().clone(),
                expected: self.published_sha256(),
                actual,
            });
        }
        Ok(VerifiedAssetContent { asset: self, bytes })
    }
}

/// Bytes whose identity has been proven against one asset's frozen facts.
///
/// The fields are private and the only constructor is
/// [`PublishedAsset::verify_bytes`], so an adapter cannot fabricate this value;
/// receiving one is a guarantee that the slice hashes to `published_sha256` and
/// is `published_size` long.
#[derive(Clone, Copy, Debug)]
pub struct VerifiedAssetContent<'a> {
    asset: &'a PublishedAsset,
    bytes: &'a [u8],
}

impl<'a> VerifiedAssetContent<'a> {
    /// The frozen facts these bytes were verified against.
    pub fn asset(&self) -> &'a PublishedAsset {
        self.asset
    }

    /// The verified representation itself.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Why bytes read from the immutable content store cannot be published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetContentError {
    SizeMismatch {
        logical_path: crate::domain::ContentPath,
        expected: u64,
        actual: u64,
    },
    IdentityMismatch {
        logical_path: crate::domain::ContentPath,
        expected: Sha256,
        actual: Sha256,
    },
}

impl std::fmt::Display for AssetContentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch {
                logical_path,
                expected,
                actual,
            } => write!(
                formatter,
                "published bytes for {logical_path} are {actual} long, not {expected}"
            ),
            Self::IdentityMismatch {
                logical_path,
                expected,
                actual,
            } => write!(
                formatter,
                "published bytes for {logical_path} hash to {actual}, not {expected}"
            ),
        }
    }
}

impl Error for AssetContentError {}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{ContentPath, Sha256},
        workflow::{AssetContentType, AssetDeliveryConfig, PublishedAsset},
    };

    use super::*;

    const BODY: &[u8] = b"published bytes";

    fn asset() -> PublishedAsset {
        PublishedAsset::from_parts_for_test(
            ContentPath::new("img/a.png").unwrap(),
            Sha256::new([1; 32]),
            Sha256::digest(BODY),
            BODY.len() as u64,
            AssetContentType::new("image/png").unwrap(),
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
        )
    }

    fn exact() -> AssetTargetFacts {
        let asset = asset();
        AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Verified(asset.published_sha256()),
        )
    }

    #[test]
    fn a_verified_exact_object_is_the_only_ready_verdict() {
        assert_eq!(
            asset().judge(&AssetTargetState::Present(exact())),
            AssetVerification::Ready
        );
        assert_eq!(
            asset().judge(&AssetTargetState::Missing),
            AssetVerification::Missing
        );
    }

    #[test]
    fn an_attestation_is_never_enough_to_authorize_a_publication() {
        let asset = asset();
        let attested = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Attested(asset.published_sha256()),
        );
        let unavailable = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Unavailable,
        );

        assert_eq!(
            asset.judge(&AssetTargetState::Present(attested)),
            AssetVerification::Unverifiable
        );
        assert_eq!(
            asset.judge(&AssetTargetState::Present(unavailable)),
            AssetVerification::Unverifiable
        );
    }

    #[test]
    fn every_frozen_fact_is_compared() {
        let asset = asset();
        // A different published blob, because the object key is content-addressed:
        // two assets with identical bytes necessarily share one key.
        let other_key = PublishedAsset::from_parts_for_test(
            ContentPath::new("img/b.png").unwrap(),
            Sha256::new([1; 32]),
            Sha256::digest(b"other bytes"),
            11,
            AssetContentType::new("image/png").unwrap(),
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
        );
        let wrong_key = AssetTargetFacts::new(
            other_key.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Verified(asset.published_sha256()),
        );
        let wrong_size = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size() + 1,
            asset.published_content_type().clone(),
            AssetByteIdentity::Verified(asset.published_sha256()),
        );
        let wrong_type = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            AssetContentType::new("image/jpeg").unwrap(),
            AssetByteIdentity::Verified(asset.published_sha256()),
        );
        let wrong_bytes = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Verified(Sha256::new([9; 32])),
        );

        assert!(matches!(
            asset.judge(&AssetTargetState::Present(wrong_key)),
            AssetVerification::Conflict(AssetTargetConflict::ObjectKey { .. })
        ));
        assert!(matches!(
            asset.judge(&AssetTargetState::Present(wrong_size)),
            AssetVerification::Conflict(AssetTargetConflict::Size { .. })
        ));
        assert!(matches!(
            asset.judge(&AssetTargetState::Present(wrong_type)),
            AssetVerification::Conflict(AssetTargetConflict::ContentType { .. })
        ));
        assert!(matches!(
            asset.judge(&AssetTargetState::Present(wrong_bytes)),
            AssetVerification::Conflict(AssetTargetConflict::Bytes { .. })
        ));
    }

    #[test]
    fn an_attestation_that_disagrees_is_a_conflict() {
        let asset = asset();
        let facts = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Attested(Sha256::new([9; 32])),
        );

        assert!(matches!(
            asset.judge(&AssetTargetState::Present(facts)),
            AssetVerification::Conflict(AssetTargetConflict::Bytes { .. })
        ));
    }

    #[test]
    fn verified_bytes_must_be_the_frozen_representation() {
        let asset = asset();

        let content = asset.verify_bytes(BODY).unwrap();
        assert_eq!(content.asset().object_key(), asset.object_key());
        assert_eq!(content.bytes(), BODY);
        assert_eq!(content.len(), BODY.len());

        assert_eq!(
            asset.verify_bytes(b"short").unwrap_err(),
            AssetContentError::SizeMismatch {
                logical_path: ContentPath::new("img/a.png").unwrap(),
                expected: BODY.len() as u64,
                actual: 5,
            }
        );
        let same_length = b"different bytes";
        assert_eq!(same_length.len(), BODY.len());
        assert!(matches!(
            asset.verify_bytes(same_length).unwrap_err(),
            AssetContentError::IdentityMismatch { .. }
        ));
    }
}
