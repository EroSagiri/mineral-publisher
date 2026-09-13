use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    asset::{
        AssetByteIdentity, AssetTargetFacts, AssetTargetState, ObjectStoreTransport, ObjectWriter,
    },
    domain::Sha256,
    workflow::{AssetContentType, AssetObjectKey, PublishedAsset},
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

/// A filesystem-backed object store.
///
/// This is the native runtime's object storage: objects live under one root at
/// their frozen content-addressed key, exactly as they would in a bucket. It
/// hashes the bytes it serves on every inspection and never overwrites an object
/// holding different content.
///
/// A media type cannot be recovered from an object's bytes without guessing from
/// its name, so it is stored beside the object as target metadata, exactly as an
/// object store keeps it as an HTTP header. Size and byte identity are always
/// observed from the object itself.
#[derive(Clone, Debug)]
pub struct FilesystemObjectStore {
    root: PathBuf,
}

impl FilesystemObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(
        &self,
        object_key: &AssetObjectKey,
    ) -> Result<PathBuf, FilesystemObjectStoreError> {
        let key = object_key.as_str();
        // The key is content-addressed and can only be built by the engine, so this
        // cannot fire today; it exists so a runtime never joins an unvalidated
        // string into a filesystem path.
        if key.is_empty()
            || key.starts_with('/')
            || key.contains(['\\', '\0'])
            || key
                .split('/')
                .any(|segment| matches!(segment, "" | "." | ".."))
        {
            return Err(FilesystemObjectStoreError::UnsafeObjectKey {
                object_key: key.to_owned(),
            });
        }
        Ok(self
            .root
            .join(key.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }

    fn metadata_path(
        &self,
        object_key: &AssetObjectKey,
    ) -> Result<PathBuf, FilesystemObjectStoreError> {
        let object = self.object_path(object_key)?;
        let mut name = object
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_default();
        name.push(".mineral-metadata");
        Ok(object.with_file_name(name))
    }

    /// Checks that an object already present under a frozen key is exactly the
    /// frozen representation, so it can be reused instead of rewritten.
    fn verify_existing(
        &self,
        asset: &PublishedAsset,
        object_path: &Path,
        metadata_path: &Path,
    ) -> Result<(), FilesystemObjectStoreError> {
        let existing = fs::read(object_path).map_err(|source| {
            FilesystemObjectStoreError::io("read existing asset object", object_path, source)
        })?;
        let existing_sha256 = Sha256::digest(&existing);
        if existing_sha256 != asset.published_sha256()
            || existing.len() as u64 != asset.published_size()
        {
            return Err(FilesystemObjectStoreError::ConflictingObject {
                object_key: asset.object_key().clone(),
                expected_sha256: asset.published_sha256(),
                expected_size: asset.published_size(),
                observed_sha256: existing_sha256,
                observed_size: existing.len() as u64,
            });
        }
        if metadata_path.is_file() {
            let metadata = read_metadata(metadata_path)?;
            if metadata.content_type != *asset.published_content_type() {
                return Err(FilesystemObjectStoreError::ConflictingObjectMetadata {
                    object_key: asset.object_key().clone(),
                    expected: asset.published_content_type().to_string(),
                    observed: metadata.content_type.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl ObjectStoreTransport for FilesystemObjectStore {
    type Error = FilesystemObjectStoreError;

    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error> {
        let object_path = self.object_path(object_key)?;
        let metadata_path = self.metadata_path(object_key)?;
        let object_exists = object_path.is_file();
        let metadata_exists = metadata_path.is_file();

        match (object_exists, metadata_exists) {
            (false, false) => Ok(AssetTargetState::Missing),
            // Half an object is not something to guess about: the namespace is
            // immutable, so a missing half is target corruption.
            (true, false) => Err(FilesystemObjectStoreError::MetadataMissing { object_path }),
            (false, true) => {
                Err(FilesystemObjectStoreError::MetadataWithoutObject { metadata_path })
            }
            (true, true) => {
                let bytes = fs::read(&object_path).map_err(|source| {
                    FilesystemObjectStoreError::io("read asset object", &object_path, source)
                })?;
                let metadata = read_metadata(&metadata_path)?;
                Ok(AssetTargetState::Present(AssetTargetFacts::new(
                    object_key.clone(),
                    bytes.len() as u64,
                    metadata.content_type,
                    // The bytes this store serves were read and hashed here; the
                    // value is never an ETag or any other store-supplied digest.
                    AssetByteIdentity::Verified(Sha256::digest(&bytes)),
                )))
            }
        }
    }

    fn open_writer(
        &self,
        asset: &PublishedAsset,
    ) -> Result<Box<dyn ObjectWriter<Error = Self::Error> + '_>, Self::Error> {
        let object_key = asset.object_key();
        let object_path = self.object_path(object_key)?;
        let metadata_path = self.metadata_path(object_key)?;

        match (object_path.is_file(), metadata_path.is_file()) {
            // Anything already under the frozen key must be exactly the frozen
            // representation; the writer then only completes missing metadata.
            (true, metadata_exists) => {
                self.verify_existing(asset, &object_path, &metadata_path)?;
                Ok(Box::new(ReuseWriter {
                    metadata_path: (!metadata_exists).then_some(metadata_path),
                    content_type: asset.published_content_type().clone(),
                }))
            }
            (false, true) => {
                Err(FilesystemObjectStoreError::MetadataWithoutObject { metadata_path })
            }
            (false, false) => Ok(Box::new(CreateWriter {
                root: self.root.clone(),
                object_path,
                metadata_path,
                content_type: asset.published_content_type().clone(),
                temporary_path: None,
            })),
        }
    }
}

/// A writer that leaves an already-correct object alone.
///
/// The bytes the driver sends are discarded on purpose: the object under this
/// content-addressed key has been checked to be exactly the frozen
/// representation, and a content-addressed key is never rewritten. Only the
/// publisher's own metadata, if an earlier attempt failed to record it, is
/// completed — that is not overwriting content.
struct ReuseWriter {
    metadata_path: Option<PathBuf>,
    content_type: AssetContentType,
}

impl ObjectWriter for ReuseWriter {
    type Error = FilesystemObjectStoreError;

    fn write(&mut self, _: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), Self::Error> {
        match self.metadata_path {
            None => Ok(()),
            Some(path) => write_metadata(&path, &self.content_type),
        }
    }
}

/// A writer that streams a new object into a temporary file and links it into
/// place without ever replacing an existing object.
struct CreateWriter {
    root: PathBuf,
    object_path: PathBuf,
    metadata_path: PathBuf,
    content_type: AssetContentType,
    temporary_path: Option<PathBuf>,
}

impl CreateWriter {
    fn temporary_path(&mut self) -> Result<PathBuf, FilesystemObjectStoreError> {
        if self.temporary_path.is_none() {
            if let Some(parent) = self.object_path.parent() {
                fs::create_dir_all(parent).map_err(|source| {
                    FilesystemObjectStoreError::io("create asset directory", parent, source)
                })?;
            }
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            self.temporary_path = Some(self.root.join(format!(
                ".mineral-asset-{}-{sequence}.tmp",
                std::process::id()
            )));
        }
        Ok(self
            .temporary_path
            .clone()
            .expect("the temporary path was just set"))
    }
}

impl ObjectWriter for CreateWriter {
    type Error = FilesystemObjectStoreError;

    fn write(&mut self, chunk: &[u8]) -> Result<(), Self::Error> {
        let path = self.temporary_path()?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| {
                FilesystemObjectStoreError::io("open temporary asset", &path, source)
            })?;
        io::Write::write_all(&mut file, chunk).map_err(|source| {
            FilesystemObjectStoreError::io("write temporary asset", &path, source)
        })?;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), Self::Error> {
        let mut writer = self;
        let temporary = writer.temporary_path()?;
        // Create-new: an existing object is never replaced, even by a writer that
        // raced this one.
        match fs::hard_link(&temporary, &writer.object_path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temporary);
                return Err(FilesystemObjectStoreError::ConcurrentObject {
                    object_path: writer.object_path.clone(),
                });
            }
            Err(source) => {
                let _ = fs::remove_file(&temporary);
                return Err(FilesystemObjectStoreError::io(
                    "publish asset object",
                    &writer.object_path,
                    source,
                ));
            }
        }
        let _ = fs::remove_file(&temporary);
        write_metadata(&writer.metadata_path, &writer.content_type)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectMetadata {
    content_type: AssetContentType,
}

fn read_metadata(path: &Path) -> Result<ObjectMetadata, FilesystemObjectStoreError> {
    let bytes = fs::read(path)
        .map_err(|source| FilesystemObjectStoreError::io("read asset metadata", path, source))?;
    serde_json::from_slice(&bytes).map_err(|error| FilesystemObjectStoreError::UnreadableMetadata {
        metadata_path: path.to_path_buf(),
        error: error.to_string(),
    })
}

fn write_metadata(
    path: &Path,
    content_type: &AssetContentType,
) -> Result<(), FilesystemObjectStoreError> {
    let encoded = serde_json::to_vec(&ObjectMetadata {
        content_type: content_type.clone(),
    })
    .expect("asset metadata is always representable as JSON");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            FilesystemObjectStoreError::io("create asset directory", parent, source)
        })?;
    }
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("meta-{}-{sequence}.tmp", std::process::id()));
    fs::write(&temporary, &encoded).map_err(|source| {
        FilesystemObjectStoreError::io("write asset metadata", &temporary, source)
    })?;
    fs::rename(&temporary, path)
        .map_err(|source| FilesystemObjectStoreError::io("publish asset metadata", path, source))
}

#[derive(Debug)]
pub enum FilesystemObjectStoreError {
    UnsafeObjectKey {
        object_key: String,
    },
    MetadataMissing {
        object_path: PathBuf,
    },
    MetadataWithoutObject {
        metadata_path: PathBuf,
    },
    UnreadableMetadata {
        metadata_path: PathBuf,
        error: String,
    },
    /// The frozen key already holds different bytes.
    ConflictingObject {
        object_key: AssetObjectKey,
        expected_sha256: Sha256,
        expected_size: u64,
        observed_sha256: Sha256,
        observed_size: u64,
    },
    /// The frozen key holds the right bytes but advertises another media type.
    ConflictingObjectMetadata {
        object_key: AssetObjectKey,
        expected: String,
        observed: String,
    },
    /// Another writer created the object first; nothing was overwritten.
    ConcurrentObject {
        object_path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl FilesystemObjectStoreError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for FilesystemObjectStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeObjectKey { object_key } => {
                write!(
                    formatter,
                    "asset object key is not a safe relative path: {object_key}"
                )
            }
            Self::MetadataMissing { object_path } => write!(
                formatter,
                "asset object exists without its content-type metadata: {}",
                object_path.display()
            ),
            Self::MetadataWithoutObject { metadata_path } => write!(
                formatter,
                "asset metadata exists without its object: {}",
                metadata_path.display()
            ),
            Self::UnreadableMetadata { metadata_path, .. } => write!(
                formatter,
                "asset object metadata is unreadable: {}",
                metadata_path.display()
            ),
            Self::ConflictingObject { object_key, .. } => write!(
                formatter,
                "asset target already holds different bytes under the frozen key: {object_key}"
            ),
            Self::ConflictingObjectMetadata { object_key, .. } => write!(
                formatter,
                "asset target already advertises another media type for: {object_key}"
            ),
            Self::ConcurrentObject { object_path } => write!(
                formatter,
                "another writer created this object first: {}",
                object_path.display()
            ),
            Self::Io {
                operation, path, ..
            } => write!(formatter, "could not {operation}: {}", path.display()),
        }
    }
}

impl Error for FilesystemObjectStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl AsRef<Path> for FilesystemObjectStore {
    fn as_ref(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        asset::{AssetVerification, ImmutableBlobSource, VerifiedBytesSource},
        domain::{ContentPath, Sha256},
        workflow::{AssetContentType, AssetDeliveryConfig, PublishedAsset},
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestStore {
        root: PathBuf,
        store: FilesystemObjectStore,
    }

    impl TestStore {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-object-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self {
                store: FilesystemObjectStore::new(&root),
                root,
            }
        }

        fn object_path(&self, asset: &PublishedAsset) -> PathBuf {
            self.root.join(
                asset
                    .object_key()
                    .as_str()
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            )
        }

        fn metadata_path(&self, asset: &PublishedAsset) -> PathBuf {
            let object = self.object_path(asset);
            let mut name = object.file_name().unwrap().to_os_string();
            name.push(".mineral-metadata");
            object.with_file_name(name)
        }

        /// A writer that hashes nothing: it exists to show what the *transport*
        /// does with bytes it is handed.
        fn write(&self, asset: &PublishedAsset, bytes: &[u8]) {
            let mut writer = self.store.open_writer(asset).unwrap();
            for chunk in bytes.chunks(3) {
                writer.write(chunk).unwrap();
            }
            writer.finish().unwrap();
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn asset(logical_path: &str, body: &[u8], media_type: &str) -> PublishedAsset {
        PublishedAsset::from_parts_for_test(
            ContentPath::new(logical_path).unwrap(),
            Sha256::new([1; 32]),
            Sha256::digest(body),
            body.len() as u64,
            AssetContentType::new(media_type).unwrap(),
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
        )
    }

    #[test]
    fn an_object_is_written_and_then_verified_from_its_own_bytes() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        assert_eq!(
            fixture.store.inspect(asset.object_key()).unwrap(),
            AssetTargetState::Missing
        );

        fixture.write(&asset, b"published bytes");

        let AssetTargetState::Present(facts) = fixture.store.inspect(asset.object_key()).unwrap()
        else {
            panic!("the written object must be present");
        };
        assert_eq!(facts.object_key(), asset.object_key());
        assert_eq!(facts.size(), asset.published_size());
        assert_eq!(facts.content_type(), asset.published_content_type());
        assert_eq!(
            facts.bytes(),
            AssetByteIdentity::Verified(asset.published_sha256())
        );
        assert_eq!(
            asset.judge(&AssetTargetState::Present(facts)),
            AssetVerification::Ready
        );
        assert_eq!(
            fs::read(fixture.object_path(&asset)).unwrap(),
            b"published bytes"
        );
    }

    /// The bytes the driver sends must be the verified ones; what the object store
    /// does with a *second* write under the same key is to ignore it, because the
    /// object already there is exactly the frozen representation.
    #[test]
    fn an_exact_object_is_reused_and_never_rewritten() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        fixture.write(&asset, b"published bytes");

        let mut writer = fixture.store.open_writer(&asset).unwrap();
        writer.write(b"not the frozen bytes").unwrap();
        writer.finish().unwrap();

        assert_eq!(
            fs::read(fixture.object_path(&asset)).unwrap(),
            b"published bytes"
        );
        let state = fixture.store.inspect(asset.object_key()).unwrap();
        assert_eq!(asset.judge(&state), AssetVerification::Ready);
    }

    #[test]
    fn a_conflicting_object_is_never_overwritten() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        let path = fixture.object_path(&asset);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"a different object").unwrap();
        fs::write(
            fixture.metadata_path(&asset),
            b"{\"content_type\":\"image/png\"}",
        )
        .unwrap();

        assert!(matches!(
            fixture.store.open_writer(&asset),
            Err(FilesystemObjectStoreError::ConflictingObject { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"a different object");
    }

    #[test]
    fn a_wrong_content_type_under_the_frozen_key_is_a_conflict() {
        let fixture = TestStore::new();
        let frozen = asset("img/photo.png", b"published bytes", "image/jpeg");
        fixture.write(&frozen, b"published bytes");
        let other = asset("img/photo.png", b"published bytes", "image/png");

        assert!(matches!(
            fixture.store.open_writer(&other),
            Err(FilesystemObjectStoreError::ConflictingObjectMetadata { .. })
        ));
    }

    #[test]
    fn an_object_without_its_metadata_fails_inspection_but_can_be_completed() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        fixture.write(&asset, b"published bytes");
        fs::remove_file(fixture.metadata_path(&asset)).unwrap();

        assert!(matches!(
            fixture.store.inspect(asset.object_key()),
            Err(FilesystemObjectStoreError::MetadataMissing { .. })
        ));

        // The bytes are exactly the frozen representation, so completing the
        // publisher's own metadata is not overwriting content.
        let mut writer = fixture.store.open_writer(&asset).unwrap();
        writer.write(b"ignored").unwrap();
        writer.finish().unwrap();

        let state = fixture.store.inspect(asset.object_key()).unwrap();
        assert_eq!(asset.judge(&state), AssetVerification::Ready);
        assert_eq!(
            fs::read(fixture.object_path(&asset)).unwrap(),
            b"published bytes"
        );
    }

    #[test]
    fn metadata_without_an_object_fails_closed() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        let path = fixture.metadata_path(&asset);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"content_type\":\"image/png\"}").unwrap();

        assert!(matches!(
            fixture.store.inspect(asset.object_key()),
            Err(FilesystemObjectStoreError::MetadataWithoutObject { .. })
        ));
        assert!(matches!(
            fixture.store.open_writer(&asset),
            Err(FilesystemObjectStoreError::MetadataWithoutObject { .. })
        ));
    }

    #[test]
    fn unreadable_metadata_fails_closed() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        fixture.write(&asset, b"published bytes");
        fs::write(fixture.metadata_path(&asset), b"not json").unwrap();

        assert!(matches!(
            fixture.store.inspect(asset.object_key()),
            Err(FilesystemObjectStoreError::UnreadableMetadata { .. })
        ));
    }

    #[test]
    fn the_content_type_is_the_stored_metadata_not_the_file_name() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"jpeg bytes", "image/jpeg");
        fixture.write(&asset, b"jpeg bytes");

        let AssetTargetState::Present(facts) = fixture.store.inspect(asset.object_key()).unwrap()
        else {
            panic!("the written object must be present");
        };
        assert_eq!(facts.content_type().as_str(), "image/jpeg");
    }

    /// Committing is an explicit step: a writer that is dropped without finishing
    /// leaves nothing behind, which is what makes it safe for the driver to abort
    /// a write whose bytes it could not verify.
    #[test]
    fn a_writer_that_is_dropped_without_finishing_creates_no_object() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");

        {
            let mut writer = fixture.store.open_writer(&asset).unwrap();
            writer.write(b"published").unwrap();
        }

        assert_eq!(
            fixture.store.inspect(asset.object_key()).unwrap(),
            AssetTargetState::Missing
        );
    }

    /// The in-memory seam from S6.3 still drives the same transport.
    #[test]
    fn verified_bytes_can_be_streamed_into_the_store() {
        let fixture = TestStore::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        let content = asset.verify_bytes(b"published bytes").unwrap();
        let mut source = VerifiedBytesSource::new(&content);

        let mut writer = fixture.store.open_writer(&asset).unwrap();
        let mut buffer = [0_u8; 4];
        loop {
            let read = source.read_chunk(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            writer.write(&buffer[..read]).unwrap();
        }
        writer.finish().unwrap();

        let state = fixture.store.inspect(asset.object_key()).unwrap();
        assert_eq!(asset.judge(&state), AssetVerification::Ready);
    }
}
