use std::error::Error;

use crate::{
    asset::{
        AssetContentError, AssetTarget, AssetTargetState, ImmutableBlobSource,
        IncrementalBlobVerifier, ObjectStoreTransport,
    },
    ports::ContentStoreError,
    workflow::{AssetObjectKey, PublishedAsset},
};

/// How much of a published blob the driver reads and sends at a time.
///
/// This is the driver's working set: one buffer of this size, whatever the size
/// of the object. It is intentionally a runtime knob, not a domain fact.
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;

/// The runtime asset driver: it streams one published blob into an object store
/// in bounded chunks and commits it only after the engine's verification rule has
/// accepted every byte it sent.
///
/// The split of responsibility is deliberate:
///
/// * the **engine** decides whether a target's facts satisfy the frozen asset
///   (`PublishedAsset::judge`) and whether a stream is the frozen representation
///   ([`IncrementalBlobVerifier`]) — one rule, defined in `mineral-core`;
/// * the **driver** owns I/O: reading bounded chunks, feeding the verifier, and
///   never committing a write the verifier has not accepted.
///
/// Committing after verification is what makes a content-addressed key safe to
/// write directly: if the source changed between the engine's own check and this
/// stream, or the store mutated it, the driver aborts the write instead of
/// publishing bytes that do not hash to the frozen identity.
#[derive(Clone, Debug)]
pub struct StreamingAssetTarget<T: ObjectStoreTransport> {
    transport: T,
    chunk_size: usize,
}

impl<T: ObjectStoreTransport> StreamingAssetTarget<T> {
    pub fn with_transport(transport: T) -> Self {
        Self {
            transport,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Sets the driver's working set, for runtimes and tests that want a smaller
    /// bound than the default.
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size.max(1);
        self
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: ObjectStoreTransport> AssetTarget for StreamingAssetTarget<T> {
    type Error = StreamingAssetTargetError<T::Error>;

    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error> {
        self.transport
            .inspect(object_key)
            .map_err(StreamingAssetTargetError::Inspect)
    }

    fn publish(
        &self,
        asset: &PublishedAsset,
        source: &mut dyn ImmutableBlobSource,
    ) -> Result<(), Self::Error> {
        // The engine opened this source for one exact identity; a source that
        // reads another blob is refused before any writer exists.
        if source.identity() != asset.published_sha256() {
            return Err(StreamingAssetTargetError::SourceIdentityMismatch {
                expected: asset.published_sha256(),
                actual: source.identity(),
            });
        }

        let mut writer = self
            .transport
            .open_writer(asset)
            .map_err(StreamingAssetTargetError::Publish)?;
        let mut verifier = IncrementalBlobVerifier::new();
        let mut buffer = vec![0_u8; self.chunk_size];
        loop {
            let read = source
                .read_chunk(&mut buffer)
                .map_err(StreamingAssetTargetError::Source)?;
            if read == 0 {
                break;
            }
            verifier.update(&buffer[..read]);
            writer
                .write(&buffer[..read])
                .map_err(StreamingAssetTargetError::Publish)?;
        }

        // The engine's own rule, applied to exactly the bytes that were sent.
        // Nothing is committed before this passes, so a source that changed under
        // us cannot reach the frozen key.
        verifier
            .verify(asset)
            .map_err(StreamingAssetTargetError::Content)?;
        writer.finish().map_err(StreamingAssetTargetError::Publish)
    }
}

#[derive(Debug)]
pub enum StreamingAssetTargetError<T: Error> {
    /// The store could not report what it holds.
    Inspect(T),
    /// The store could not place the object.
    Publish(T),
    /// The bounded source failed while streaming.
    Source(ContentStoreError),
    /// The source reads another blob than the frozen asset.
    SourceIdentityMismatch {
        expected: crate::domain::Sha256,
        actual: crate::domain::Sha256,
    },
    /// The streamed bytes are not the frozen representation, so nothing was
    /// committed.
    Content(AssetContentError),
}

impl<T: Error> std::fmt::Display for StreamingAssetTargetError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inspect(error) => write!(formatter, "could not inspect asset target: {error}"),
            Self::Publish(error) => write!(formatter, "could not publish asset: {error}"),
            Self::Source(error) => {
                write!(formatter, "could not stream the published blob: {error}")
            }
            Self::SourceIdentityMismatch { .. } => formatter.write_str(
                "the published blob source reads another identity than the frozen asset",
            ),
            Self::Content(error) => write!(
                formatter,
                "the streamed bytes are not the frozen representation: {error}"
            ),
        }
    }
}

impl<T: Error + 'static> Error for StreamingAssetTargetError<T> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect(error) | Self::Publish(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::Content(error) => Some(error),
            Self::SourceIdentityMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, error::Error, fmt};

    use crate::{
        asset::{
            AssetByteIdentity, AssetTargetFacts, BufferedBlobSource, FilesystemAssetTarget,
            ObjectStoreTransport, ObjectWriter,
        },
        domain::{ContentPath, Sha256},
        ports::ContentStoreError,
        workflow::{AssetContentType, AssetDeliveryConfig, PublishedAsset},
    };

    use super::*;

    const BODY: &[u8] = b"one published representation, larger than a single chunk";

    fn asset(body: &[u8], media_type: &str) -> PublishedAsset {
        PublishedAsset::from_parts_for_test(
            ContentPath::new("img/a.bin").unwrap(),
            Sha256::new([1; 32]),
            Sha256::digest(body),
            body.len() as u64,
            AssetContentType::new(media_type).unwrap(),
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
        )
    }

    #[derive(Debug)]
    struct TransportFailure(&'static str);

    impl fmt::Display for TransportFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TransportFailure {}

    type Result<T> = std::result::Result<T, TransportFailure>;

    /// The largest single write the transport was asked to accept.
    #[derive(Default)]
    struct Written {
        chunks: Vec<Vec<u8>>,
        committed: Vec<Vec<u8>>,
    }

    /// A transport that records every chunk a writer hands it and only commits on
    /// `finish`, so a test can show exactly what was sent and what was committed.
    struct FakeTransport {
        writer_calls: Cell<u32>,
        written: std::rc::Rc<RefCell<Written>>,
        existing: RefCell<Option<AssetTargetState>>,
    }

    impl Default for FakeTransport {
        fn default() -> Self {
            Self {
                writer_calls: Cell::new(0),
                written: std::rc::Rc::new(RefCell::new(Written::default())),
                existing: RefCell::new(None),
            }
        }
    }

    impl FakeTransport {
        fn sent(&self) -> Vec<Vec<u8>> {
            self.written.borrow().chunks.clone()
        }

        fn committed(&self) -> Option<Vec<u8>> {
            self.written.borrow().committed.last().cloned()
        }

        fn largest_chunk(&self) -> usize {
            self.written
                .borrow()
                .chunks
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(0)
        }

        fn writer_calls(&self) -> u32 {
            self.writer_calls.get()
        }
    }

    struct FakeWriter {
        written: std::rc::Rc<RefCell<Written>>,
        buffer: Vec<u8>,
    }

    impl ObjectWriter for FakeWriter {
        type Error = TransportFailure;

        fn write(&mut self, chunk: &[u8]) -> Result<()> {
            self.written.borrow_mut().chunks.push(chunk.to_vec());
            self.buffer.extend_from_slice(chunk);
            Ok(())
        }

        fn finish(self: Box<Self>) -> Result<()> {
            self.written.borrow_mut().committed.push(self.buffer);
            Ok(())
        }
    }

    impl ObjectStoreTransport for FakeTransport {
        type Error = TransportFailure;

        fn inspect(&self, _object_key: &AssetObjectKey) -> Result<AssetTargetState> {
            Ok(self
                .existing
                .borrow()
                .clone()
                .unwrap_or(AssetTargetState::Missing))
        }

        fn open_writer(
            &self,
            _: &PublishedAsset,
        ) -> Result<Box<dyn ObjectWriter<Error = Self::Error> + '_>> {
            self.writer_calls.set(self.writer_calls.get() + 1);
            Ok(Box::new(FakeWriter {
                written: std::rc::Rc::clone(&self.written),
                buffer: Vec::new(),
            }))
        }
    }

    /// A source that records the buffers it was handed and refuses to fill
    /// anything larger than the bound it was built with.
    ///
    /// This is the proof that the driver does not ask for the whole object: a
    /// full-buffer implementation would immediately trip the bound.
    struct BoundedSource {
        identity: Sha256,
        body: Vec<u8>,
        position: usize,
        bound: usize,
        largest_request: Cell<usize>,
        reads: Cell<u32>,
    }

    impl BoundedSource {
        fn new(identity: Sha256, body: &[u8], bound: usize) -> Self {
            Self {
                identity,
                body: body.to_vec(),
                position: 0,
                bound,
                largest_request: Cell::new(0),
                reads: Cell::new(0),
            }
        }

        fn largest_request(&self) -> usize {
            self.largest_request.get()
        }

        fn reads(&self) -> u32 {
            self.reads.get()
        }
    }

    impl ImmutableBlobSource for BoundedSource {
        fn identity(&self) -> Sha256 {
            self.identity
        }

        fn read_chunk(
            &mut self,
            buffer: &mut [u8],
        ) -> std::result::Result<usize, ContentStoreError> {
            self.reads.set(self.reads.get() + 1);
            self.largest_request
                .set(self.largest_request.get().max(buffer.len()));
            assert!(
                buffer.len() <= self.bound,
                "the driver asked for {} bytes with a {} byte bound",
                buffer.len(),
                self.bound
            );
            let remaining = self.body.len().saturating_sub(self.position);
            let take = remaining.min(buffer.len());
            buffer[..take].copy_from_slice(&self.body[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    /// A source that fails once it has produced `after` chunks.
    struct FailingSource {
        identity: Sha256,
        body: Vec<u8>,
        position: usize,
        produced: u32,
        fail_after: u32,
    }

    impl ImmutableBlobSource for FailingSource {
        fn identity(&self) -> Sha256 {
            self.identity
        }

        fn read_chunk(
            &mut self,
            buffer: &mut [u8],
        ) -> std::result::Result<usize, ContentStoreError> {
            if self.produced >= self.fail_after {
                return Err(ContentStoreError::Missing(self.identity));
            }
            self.produced += 1;
            let remaining = self.body.len().saturating_sub(self.position);
            let take = remaining.min(buffer.len());
            buffer[..take].copy_from_slice(&self.body[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    #[test]
    fn a_multi_chunk_object_is_streamed_in_bounded_pieces() {
        let asset = asset(BODY, "application/octet-stream");
        let transport = FakeTransport::default();
        let target = StreamingAssetTarget::with_transport(&transport).with_chunk_size(8);
        let mut source = BoundedSource::new(asset.published_sha256(), BODY, 8);

        target.publish(&asset, &mut source).unwrap();

        assert_eq!(transport.writer_calls(), 1);
        assert!(source.reads() > 1, "a large object needs several reads");
        assert_eq!(source.largest_request(), 8);
        assert!(transport.sent().len() > 1, "the store sees several chunks");
        assert!(transport.largest_chunk() <= 8);
        assert_eq!(
            transport.sent().into_iter().flatten().collect::<Vec<u8>>(),
            BODY,
            "the store received exactly the frozen object"
        );
        assert_eq!(transport.committed().unwrap(), BODY);
    }

    #[test]
    fn a_source_that_reads_another_blob_is_refused_before_any_writer_exists() {
        let asset = asset(BODY, "application/octet-stream");
        let transport = FakeTransport::default();
        let target = StreamingAssetTarget::with_transport(&transport);
        let mut source = BoundedSource::new(Sha256::new([9; 32]), BODY, 64);

        let error = target.publish(&asset, &mut source).unwrap_err();

        assert!(matches!(
            error,
            StreamingAssetTargetError::SourceIdentityMismatch { .. }
        ));
        assert_eq!(transport.writer_calls(), 0);
        assert!(transport.committed().is_none());
    }

    #[test]
    fn a_stream_that_is_not_the_frozen_representation_is_never_committed() {
        let asset = asset(BODY, "application/octet-stream");
        let mut changed = BODY.to_vec();
        changed[0] ^= 0x01;

        for (body, expected) in [
            (changed, "identity"),
            (BODY[..BODY.len() - 1].to_vec(), "size"),
        ] {
            let transport = FakeTransport::default();
            let target = StreamingAssetTarget::with_transport(&transport).with_chunk_size(8);
            let mut source = BoundedSource::new(asset.published_sha256(), &body, 8);

            let error = target.publish(&asset, &mut source).unwrap_err();

            match (expected, error) {
                (
                    "identity",
                    StreamingAssetTargetError::Content(AssetContentError::IdentityMismatch {
                        ..
                    }),
                ) => {}
                (
                    "size",
                    StreamingAssetTargetError::Content(AssetContentError::SizeMismatch { .. }),
                ) => {}
                (expected, error) => panic!("expected a {expected} refusal, got {error:?}"),
            }
            assert!(
                transport.committed().is_none(),
                "nothing may be committed for a {expected} mismatch"
            );
        }
    }

    /// The case the engine's own look at the blob cannot close: the source is the
    /// frozen blob for almost its whole length and diverges only at the end, so the
    /// difference is only knowable after every byte has been sent.
    #[test]
    fn a_stream_that_diverges_at_its_last_byte_is_never_committed() {
        let body = vec![0x11_u8; 4096];
        let asset = asset(&body, "application/octet-stream");
        let mut changed = body.clone();
        *changed.last_mut().unwrap() = 0x12;
        assert_eq!(changed.len(), body.len());

        let transport = FakeTransport::default();
        let target = StreamingAssetTarget::with_transport(&transport).with_chunk_size(64);
        let mut source = BoundedSource::new(asset.published_sha256(), &changed, 64);

        let error = target.publish(&asset, &mut source).unwrap_err();

        assert!(matches!(
            error,
            StreamingAssetTargetError::Content(AssetContentError::IdentityMismatch { .. })
        ));
        // Every chunk but the last was handed to the store before the difference
        // was visible, and not one byte of it was committed.
        assert!(transport.sent().len() > 1);
        assert_eq!(
            transport.sent().into_iter().flatten().collect::<Vec<u8>>(),
            changed
        );
        assert!(transport.committed().is_none());
    }

    #[test]
    fn a_source_that_fails_mid_stream_is_never_committed() {
        let asset = asset(BODY, "application/octet-stream");
        let transport = FakeTransport::default();
        let target = StreamingAssetTarget::with_transport(&transport).with_chunk_size(8);
        let mut source = FailingSource {
            identity: asset.published_sha256(),
            body: BODY.to_vec(),
            position: 0,
            produced: 0,
            fail_after: 1,
        };

        let error = target.publish(&asset, &mut source).unwrap_err();

        assert!(matches!(error, StreamingAssetTargetError::Source(_)));
        assert!(transport.committed().is_none());
    }

    #[test]
    fn the_driver_uses_its_own_chunk_size_for_any_object_size() {
        // A "large" object, by test standards: many times the driver's bound.
        let body = vec![0x5a_u8; 4096];
        let asset = asset(&body, "application/octet-stream");
        let transport = FakeTransport::default();
        let target = StreamingAssetTarget::with_transport(&transport).with_chunk_size(64);
        let mut source = BoundedSource::new(asset.published_sha256(), &body, 64);

        target.publish(&asset, &mut source).unwrap();

        assert_eq!(source.largest_request(), 64);
        assert_eq!(transport.largest_chunk(), 64);
        assert_eq!(transport.sent().len(), 4096 / 64);
        assert_eq!(transport.committed().unwrap().len(), 4096);
    }

    #[test]
    fn a_buffered_source_still_streams_through_the_same_driver() {
        let asset = asset(BODY, "application/octet-stream");
        let transport = FakeTransport::default();
        let target = StreamingAssetTarget::with_transport(&transport).with_chunk_size(16);
        let mut source = BufferedBlobSource::new(asset.published_sha256(), BODY.to_vec());

        target.publish(&asset, &mut source).unwrap();

        assert!(transport.sent().len() > 1);
        assert_eq!(transport.committed().unwrap(), BODY);
    }

    /// The native driver over the real filesystem store, end to end.
    #[test]
    fn the_native_target_streams_into_a_real_object_store() {
        let root = std::env::temp_dir().join(format!(
            "mineral-publisher-streaming-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let target = FilesystemAssetTarget::new(&root).with_chunk_size(16);
        let asset = asset(BODY, "application/octet-stream");

        let mut source = BoundedSource::new(asset.published_sha256(), BODY, 16);
        target.publish(&asset, &mut source).unwrap();

        let facts = match target.inspect(asset.object_key()).unwrap() {
            AssetTargetState::Present(facts) => facts,
            other => panic!("expected a present object, got {other:?}"),
        };
        assert_eq!(facts.size(), asset.published_size());
        assert_eq!(
            facts.bytes(),
            AssetByteIdentity::Verified(asset.published_sha256())
        );
        assert_eq!(
            std::fs::read(
                root.join(
                    asset
                        .object_key()
                        .as_str()
                        .replace('/', std::path::MAIN_SEPARATOR_STR)
                )
            )
            .unwrap(),
            BODY
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The S6.3 in-memory seam and the streaming seam agree about the facts.
    #[test]
    fn an_in_memory_exact_object_is_ready_for_the_same_asset() {
        let asset = asset(BODY, "application/octet-stream");
        let facts = AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Verified(asset.published_sha256()),
        );

        assert_eq!(
            asset.judge(&AssetTargetState::Present(facts)),
            crate::asset::AssetVerification::Ready
        );
    }
}
