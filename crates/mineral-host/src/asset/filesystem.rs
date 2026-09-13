use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    asset::{
        AssetByteIdentity, AssetTarget, AssetTargetFacts, AssetTargetState, VerifiedAssetContent,
    },
    domain::Sha256,
    workflow::{AssetContentType, AssetObjectKey},
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

/// A filesystem-backed object target.
///
/// This is the native runtime's stand-in for object storage: objects live under
/// one root at their frozen content-addressed key, exactly as they would live in a
/// bucket. It is a real adapter rather than a test double — it hashes the bytes it
/// serves on every inspection, refuses to overwrite an object holding different
/// content, and derives nothing: the key, the content type, and the bytes all come
/// from the frozen value the engine passes in.
///
/// A media type cannot be recovered from a file's bytes without guessing from its
/// name, so it is stored beside the object as target metadata, exactly as an
/// object store stores it as an HTTP header. Everything else — size and byte
/// identity — is observed from the object itself.
#[derive(Clone, Debug)]
pub struct FilesystemAssetTarget {
    root: PathBuf,
}

impl FilesystemAssetTarget {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(
        &self,
        object_key: &AssetObjectKey,
    ) -> Result<PathBuf, FilesystemAssetTargetError> {
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
            return Err(FilesystemAssetTargetError::UnsafeObjectKey {
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
    ) -> Result<PathBuf, FilesystemAssetTargetError> {
        let object = self.object_path(object_key)?;
        let mut name = object
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_default();
        name.push(".mineral-metadata");
        Ok(object.with_file_name(name))
    }
}

impl AssetTarget for FilesystemAssetTarget {
    type Error = FilesystemAssetTargetError;

    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error> {
        let object_path = self.object_path(object_key)?;
        let metadata_path = self.metadata_path(object_key)?;
        let object_exists = object_path.is_file();
        let metadata_exists = metadata_path.is_file();

        match (object_exists, metadata_exists) {
            (false, false) => Ok(AssetTargetState::Missing),
            // Half an object is not something to guess about: the namespace is
            // immutable, so a missing half is target corruption.
            (true, false) => Err(FilesystemAssetTargetError::MetadataMissing { object_path }),
            (false, true) => {
                Err(FilesystemAssetTargetError::MetadataWithoutObject { metadata_path })
            }
            (true, true) => {
                let bytes = fs::read(&object_path).map_err(|source| {
                    FilesystemAssetTargetError::io("read asset object", &object_path, source)
                })?;
                let metadata = read_metadata(&metadata_path)?;
                Ok(AssetTargetState::Present(AssetTargetFacts::new(
                    object_key.clone(),
                    bytes.len() as u64,
                    metadata.content_type,
                    // The bytes this target serves were read and hashed here; the
                    // value is never an ETag or any other store-supplied digest.
                    AssetByteIdentity::Verified(Sha256::digest(&bytes)),
                )))
            }
        }
    }

    fn publish(&self, content: VerifiedAssetContent<'_>) -> Result<(), Self::Error> {
        let asset = content.asset();
        let object_key = asset.object_key();
        let object_path = self.object_path(object_key)?;
        let metadata_path = self.metadata_path(object_key)?;

        if object_path.is_file() {
            // A content-addressed key can only ever mean one thing. Anything else
            // under it is corruption, and this adapter never resolves that by
            // overwriting.
            let existing = fs::read(&object_path).map_err(|source| {
                FilesystemAssetTargetError::io("read existing asset object", &object_path, source)
            })?;
            let existing_sha256 = Sha256::digest(&existing);
            if existing_sha256 != asset.published_sha256()
                || existing.len() as u64 != asset.published_size()
            {
                return Err(FilesystemAssetTargetError::ConflictingObject {
                    object_key: object_key.clone(),
                    expected_sha256: asset.published_sha256(),
                    expected_size: asset.published_size(),
                    observed_sha256: existing_sha256,
                    observed_size: existing.len() as u64,
                });
            }
            if metadata_path.is_file() {
                let metadata = read_metadata(&metadata_path)?;
                if metadata.content_type != *asset.published_content_type() {
                    return Err(FilesystemAssetTargetError::ConflictingObjectMetadata {
                        object_key: object_key.clone(),
                        expected: asset.published_content_type().to_string(),
                        observed: metadata.content_type.to_string(),
                    });
                }
                return Ok(());
            }
            // Identical bytes, but this publisher's own metadata never landed.
            // Completing it is not overwriting content.
            return write_metadata(&metadata_path, asset.published_content_type());
        }

        // Bytes first, metadata second, both through a temporary file and a rename,
        // so a reader never sees a partially written object.
        write_atomically(&object_path, content.bytes())?;
        write_metadata(&metadata_path, asset.published_content_type())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectMetadata {
    content_type: AssetContentType,
}

fn read_metadata(path: &Path) -> Result<ObjectMetadata, FilesystemAssetTargetError> {
    let bytes = fs::read(path)
        .map_err(|source| FilesystemAssetTargetError::io("read asset metadata", path, source))?;
    serde_json::from_slice(&bytes).map_err(|error| FilesystemAssetTargetError::UnreadableMetadata {
        metadata_path: path.to_path_buf(),
        error: error.to_string(),
    })
}

fn write_metadata(
    path: &Path,
    content_type: &AssetContentType,
) -> Result<(), FilesystemAssetTargetError> {
    let encoded = serde_json::to_vec(&ObjectMetadata {
        content_type: content_type.clone(),
    })
    .expect("asset metadata is always representable as JSON");
    write_atomically(path, &encoded)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), FilesystemAssetTargetError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            FilesystemAssetTargetError::io("create asset directory", parent, source)
        })?;
    }
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    fs::write(&temporary, bytes).map_err(|source| {
        FilesystemAssetTargetError::io("write asset object", &temporary, source)
    })?;
    fs::rename(&temporary, path)
        .map_err(|source| FilesystemAssetTargetError::io("publish asset object", path, source))
}

#[derive(Debug)]
pub enum FilesystemAssetTargetError {
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
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl FilesystemAssetTargetError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for FilesystemAssetTargetError {
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
            Self::Io {
                operation, path, ..
            } => write!(formatter, "could not {operation}: {}", path.display()),
        }
    }
}

impl Error for FilesystemAssetTargetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl AsRef<Path> for FilesystemAssetTarget {
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
        asset::{AssetTargetState, AssetVerification},
        domain::{ContentPath, Sha256},
        workflow::{AssetContentType, AssetDeliveryConfig, PublishedAsset},
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestTarget {
        root: PathBuf,
        target: FilesystemAssetTarget,
    }

    impl TestTarget {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mineral-publisher-asset-target-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self {
                target: FilesystemAssetTarget::new(&root),
                root,
            }
        }

        /// The raw path the target stores one object at, so a test can corrupt the
        /// namespace from outside.
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
    }

    impl Drop for TestTarget {
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
    fn an_object_is_published_and_then_verified_from_its_own_bytes() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");

        assert_eq!(
            fixture.target.inspect(asset.object_key()).unwrap(),
            AssetTargetState::Missing
        );

        let content = asset.verify_bytes(b"published bytes").unwrap();
        fixture.target.publish(content).unwrap();

        let state = fixture.target.inspect(asset.object_key()).unwrap();
        let AssetTargetState::Present(facts) = state else {
            panic!("the published object must be present");
        };
        assert_eq!(facts.object_key(), asset.object_key());
        assert_eq!(facts.size(), asset.published_size());
        assert_eq!(facts.content_type(), asset.published_content_type());
        assert_eq!(
            facts.bytes(),
            AssetByteIdentity::Verified(asset.published_sha256()),
            "the target hashed the object it serves"
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

    #[test]
    fn publishing_the_same_representation_repeatedly_is_idempotent() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");

        for _ in 0..3 {
            fixture
                .target
                .publish(asset.verify_bytes(b"published bytes").unwrap())
                .unwrap();
        }

        let state = fixture.target.inspect(asset.object_key()).unwrap();
        assert_eq!(asset.judge(&state), AssetVerification::Ready);
    }

    #[test]
    fn the_content_type_is_the_stored_metadata_not_the_file_name() {
        let fixture = TestTarget::new();
        // A `.png` path whose published bytes are a JPEG.
        let asset = asset("img/photo.png", b"jpeg bytes", "image/jpeg");

        fixture
            .target
            .publish(asset.verify_bytes(b"jpeg bytes").unwrap())
            .unwrap();

        let AssetTargetState::Present(facts) = fixture.target.inspect(asset.object_key()).unwrap()
        else {
            panic!("the published object must be present");
        };
        assert_eq!(facts.content_type().as_str(), "image/jpeg");
    }

    #[test]
    fn a_conflicting_object_is_never_overwritten() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        // Something else already sits at the frozen content-addressed key.
        let path = fixture.object_path(&asset);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"a different object").unwrap();

        let error = fixture
            .target
            .publish(asset.verify_bytes(b"published bytes").unwrap())
            .unwrap_err();

        assert!(matches!(
            error,
            FilesystemAssetTargetError::ConflictingObject { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), b"a different object");
    }

    #[test]
    fn an_object_without_its_metadata_fails_closed() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        let path = fixture.object_path(&asset);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"published bytes").unwrap();

        assert!(matches!(
            fixture.target.inspect(asset.object_key()),
            Err(FilesystemAssetTargetError::MetadataMissing { .. })
        ));
    }

    #[test]
    fn metadata_without_an_object_fails_closed() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        let path = fixture.metadata_path(&asset);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"content_type\":\"image/png\"}").unwrap();

        assert!(matches!(
            fixture.target.inspect(asset.object_key()),
            Err(FilesystemAssetTargetError::MetadataWithoutObject { .. })
        ));
    }

    #[test]
    fn unreadable_metadata_fails_closed() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        fixture
            .target
            .publish(asset.verify_bytes(b"published bytes").unwrap())
            .unwrap();
        fs::write(fixture.metadata_path(&asset), b"not json").unwrap();

        assert!(matches!(
            fixture.target.inspect(asset.object_key()),
            Err(FilesystemAssetTargetError::UnreadableMetadata { .. })
        ));
    }

    /// Identical bytes with missing publisher metadata are completed rather than
    /// treated as a conflict: nothing is being overwritten.
    #[test]
    fn missing_metadata_for_correct_bytes_is_completed_on_republish() {
        let fixture = TestTarget::new();
        let asset = asset("img/photo.png", b"published bytes", "image/png");
        fixture
            .target
            .publish(asset.verify_bytes(b"published bytes").unwrap())
            .unwrap();
        fs::remove_file(fixture.metadata_path(&asset)).unwrap();

        fixture
            .target
            .publish(asset.verify_bytes(b"published bytes").unwrap())
            .unwrap();

        let state = fixture.target.inspect(asset.object_key()).unwrap();
        assert_eq!(asset.judge(&state), AssetVerification::Ready);
        assert_eq!(
            fs::read(fixture.object_path(&asset)).unwrap(),
            b"published bytes"
        );
    }
}
