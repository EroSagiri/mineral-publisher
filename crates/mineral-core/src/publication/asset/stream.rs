use sha2::Digest;

use crate::{domain::Sha256, ports::ContentStoreError, workflow::PublishedAsset};

use super::AssetContentError;

/// A bounded, streaming view of one immutable published blob.
///
/// This is the runtime boundary for large media: the engine never asks for the
/// whole object, and an implementation may not hand it over. The contract is
/// deliberately narrower than `std::io::Read` so the bound is part of the type:
///
/// * [`read_chunk`](ImmutableBlobSource::read_chunk) fills at most the caller's
///   buffer and returns how many bytes it produced; `Ok(0)` is the end.
/// * Returning the whole object in one call because the buffer happened to be
///   large is a contract violation; a runtime that cannot stream must say so
///   rather than pretend.
///
/// The engine still verifies the bytes it reads — see
/// [`IncrementalBlobVerifier`] — so a source is a transport, never an authority
/// on what the frozen representation is.
pub trait ImmutableBlobSource {
    /// The immutable blob identity this source reads from.
    ///
    /// The engine compares it with the frozen asset identity before anything is
    /// published, so a source cannot quietly stream a different object.
    fn identity(&self) -> Sha256;

    /// Reads at most `buffer.len()` bytes at the current position.
    ///
    /// `Ok(0)` means the end of the blob. A source may produce fewer bytes than
    /// the buffer holds at any time.
    fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, ContentStoreError>;
}

/// An already-buffered blob, yielded in bounded chunks.
///
/// This is the fallback a [`crate::ports::BlobStore`] gets when it cannot stream:
/// the bytes were read completely, and the source hands them out in pieces so the
/// rest of the pipeline keeps its bounded shape.
pub struct BufferedBlobSource {
    identity: Sha256,
    bytes: Vec<u8>,
    position: usize,
}

impl BufferedBlobSource {
    pub fn new(identity: Sha256, bytes: Vec<u8>) -> Self {
        Self {
            identity,
            bytes,
            position: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl ImmutableBlobSource for BufferedBlobSource {
    fn identity(&self) -> Sha256 {
        self.identity
    }

    fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, ContentStoreError> {
        let remaining = self.bytes.len().saturating_sub(self.position);
        let take = remaining.min(buffer.len());
        buffer[..take].copy_from_slice(&self.bytes[self.position..self.position + take]);
        self.position += take;
        Ok(take)
    }
}

/// The frozen-identity rule, evaluated over a stream instead of one buffer.
///
/// Every chunk the runtime reads is folded in as it arrives, so an arbitrarily
/// large published blob can be proven to be exactly the frozen representation
/// without ever being held in memory. [`PublishedAsset::verify_bytes`] is this
/// same rule applied to bytes that are already in hand, so there is one
/// definition of "these bytes are the published representation" in the engine.
#[derive(Clone, Debug)]
pub struct IncrementalBlobVerifier {
    hasher: sha2::Sha256,
    size: u64,
}

impl Default for IncrementalBlobVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalBlobVerifier {
    pub fn new() -> Self {
        Self {
            hasher: <sha2::Sha256 as sha2::Digest>::new(),
            size: 0,
        }
    }

    /// Folds one chunk of the stream into the running identity.
    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.size = self.size.saturating_add(chunk.len() as u64);
    }

    /// The number of bytes folded in so far.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Judges the bytes folded in so far against one asset's frozen facts.
    pub fn verify(&self, asset: &PublishedAsset) -> Result<(), AssetContentError> {
        if self.size != asset.published_size() {
            return Err(AssetContentError::SizeMismatch {
                logical_path: asset.logical_path().clone(),
                expected: asset.published_size(),
                actual: self.size,
            });
        }
        let actual = Sha256::new(self.hasher.clone().finalize().into());
        if actual != asset.published_sha256() {
            return Err(AssetContentError::IdentityMismatch {
                logical_path: asset.logical_path().clone(),
                expected: asset.published_sha256(),
                actual,
            });
        }
        Ok(())
    }
}

/// A source over already-verified in-memory bytes.
///
/// [`crate::workflow::PublishedAsset::verify_bytes`] proves the bytes up front
/// and hands back a value whose existence is that proof; this turns it into the
/// same bounded shape every other source has, so a target implementation has one
/// input contract and the in-memory correctness seam keeps working.
pub struct VerifiedBytesSource<'a> {
    content: &'a [u8],
    asset: &'a PublishedAsset,
    position: usize,
}

impl<'a> VerifiedBytesSource<'a> {
    /// Wraps verified content. The bytes must be the ones
    /// `PublishedAsset::verify_bytes` accepted.
    pub fn new(content: &super::VerifiedAssetContent<'a>) -> Self {
        Self {
            content: content.bytes(),
            asset: content.asset(),
            position: 0,
        }
    }
}

impl ImmutableBlobSource for VerifiedBytesSource<'_> {
    fn identity(&self) -> Sha256 {
        self.asset.published_sha256()
    }

    fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, ContentStoreError> {
        let remaining = self.content.len().saturating_sub(self.position);
        let take = remaining.min(buffer.len());
        buffer[..take].copy_from_slice(&self.content[self.position..self.position + take]);
        self.position += take;
        Ok(take)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{ContentPath, Sha256},
        workflow::{AssetContentType, AssetDeliveryConfig, PublishedAsset},
    };

    use super::*;

    const BODY: &[u8] = b"the published representation of one asset";

    fn asset(body: &[u8]) -> PublishedAsset {
        PublishedAsset::from_parts_for_test(
            ContentPath::new("img/a.png").unwrap(),
            Sha256::new([1; 32]),
            Sha256::digest(body),
            body.len() as u64,
            AssetContentType::new("image/png").unwrap(),
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
        )
    }

    fn drain(source: &mut dyn ImmutableBlobSource, chunk: usize) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut buffer = vec![0_u8; chunk];
        loop {
            let read = source.read_chunk(&mut buffer).unwrap();
            if read == 0 {
                return collected;
            }
            collected.extend_from_slice(&buffer[..read]);
        }
    }

    #[test]
    fn a_buffered_source_yields_bounded_chunks_and_the_exact_bytes() {
        let asset = asset(BODY);
        let mut source = BufferedBlobSource::new(asset.published_sha256(), BODY.to_vec());

        let mut buffer = vec![0_u8; 4];
        let mut sizes = Vec::new();
        let mut collected = Vec::new();
        loop {
            let read = source.read_chunk(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            assert!(read <= buffer.len());
            sizes.push(read);
            collected.extend_from_slice(&buffer[..read]);
        }

        assert_eq!(collected, BODY);
        assert!(sizes.len() > 1, "a 4-byte buffer must need several reads");
        assert_eq!(source.identity(), asset.published_sha256());
    }

    #[test]
    fn the_incremental_rule_is_the_same_rule_as_the_in_memory_one() {
        let asset = asset(BODY);

        let mut verifier = IncrementalBlobVerifier::new();
        for chunk in BODY.chunks(3) {
            verifier.update(chunk);
        }
        assert_eq!(verifier.size(), BODY.len() as u64);
        assert_eq!(verifier.verify(&asset), Ok(()));

        // And it is exactly what verifying the same bytes in one piece says.
        assert!(asset.verify_bytes(BODY).is_ok());

        let mut short = IncrementalBlobVerifier::new();
        short.update(&BODY[..BODY.len() - 1]);
        assert!(matches!(
            short.verify(&asset),
            Err(AssetContentError::SizeMismatch { .. })
        ));

        let mut changed = BODY.to_vec();
        changed[0] ^= 0x01;
        let mut different = IncrementalBlobVerifier::new();
        different.update(&changed);
        assert_eq!(different.size(), BODY.len() as u64);
        assert!(matches!(
            different.verify(&asset),
            Err(AssetContentError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn verified_bytes_are_one_source_among_others() {
        let asset = asset(BODY);
        let content = asset.verify_bytes(BODY).unwrap();
        let mut source = VerifiedBytesSource::new(&content);

        assert_eq!(source.identity(), asset.published_sha256());
        assert_eq!(drain(&mut source, 7), BODY);
    }
}
