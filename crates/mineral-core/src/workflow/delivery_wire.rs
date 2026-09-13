use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::domain::{ContentPath, Sha256, SnapshotId};

use super::{
    AssetContentType, AssetDeliveryConfig, AssetObjectKey, AssetProjection, AssetPublicBaseUrl,
    AssetPublicUrl, DeliveryProjection, ManagedRoot, ProjectionTargetPath, PublishedAsset,
    TextProjection, TextProjectionFile,
};

/// Versioned durable encoding of one immutable [`DeliveryProjection`].
///
/// A stored projection is the only thing a later execution may rematerialize a
/// reviewed tree from, so the payload has to carry its own version: when the
/// delivery model changes shape, an old durable row must fail loudly instead of
/// being silently bound to today's field set. Version 1 is:
///
/// ```json
/// {
///   "version": 1,
///   "delivery_sha256": "…",
///   "source_projection_sha256": "…",
///   "snapshot_id": 1,
///   "managed_root": "content",
///   "text": {
///     "source_projection_sha256": "…",
///     "projection_sha256": "…",
///     "snapshot_id": 1,
///     "managed_root": "content",
///     "files": [
///       { "target_path": "…", "blob_sha256": "…", "source_path": "…", "source_sha256": "…" }
///     ]
///   },
///   "assets": [
///     { "logical_path": "…", "source_sha256": "…", "published_sha256": "…",
///       "published_size": 0, "published_content_type": "image/png",
///       "object_key": "…", "public_url": "…" }
///   ]
/// }
/// ```
///
/// Only portable facts appear here: no runtime handle, no path, no credential,
/// and no base URL that is not already implied by each asset's final URL.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeliveryProjectionWire;

impl DeliveryProjectionWire {
    /// The only durable version this engine writes and reads.
    pub const VERSION: u32 = 1;

    pub fn encode(projection: &DeliveryProjection) -> String {
        let wire = WireDeliveryProjection {
            version: Self::VERSION,
            delivery_sha256: projection.delivery_sha256().to_string(),
            source_projection_sha256: projection.source_projection_sha256().to_string(),
            snapshot_id: projection.snapshot_id().get(),
            managed_root: projection.managed_root().as_str().to_owned(),
            text: WireTextProjection {
                source_projection_sha256: projection.text().source_projection_sha256().to_string(),
                projection_sha256: projection.text().projection_sha256().to_string(),
                snapshot_id: projection.text().snapshot_id().get(),
                managed_root: projection.text().managed_root().as_str().to_owned(),
                files: projection
                    .text()
                    .files()
                    .iter()
                    .map(|file| WireTextFile {
                        target_path: file.target_path().as_str().to_owned(),
                        blob_sha256: file.blob_sha256().to_string(),
                        source_path: file.source_path().as_str().to_owned(),
                        source_sha256: file.source_sha256().to_string(),
                    })
                    .collect(),
            },
            assets: projection
                .assets()
                .assets()
                .iter()
                .map(|asset| WireAsset {
                    logical_path: asset.logical_path().as_str().to_owned(),
                    source_sha256: asset.source_sha256().to_string(),
                    published_sha256: asset.published_sha256().to_string(),
                    published_size: asset.published_size(),
                    published_content_type: asset.published_content_type().as_str().to_owned(),
                    object_key: asset.object_key().as_str().to_owned(),
                    public_url: asset.public_url().as_str().to_owned(),
                })
                .collect(),
        };
        serde_json::to_string(&wire)
            .expect("a delivery projection is always representable as a JSON object")
    }

    /// Decodes a durable projection, rejecting anything that is not exactly the
    /// immutable delivery intent it claims to be.
    ///
    /// Every fact is rebuilt through its domain constructor and both canonical
    /// identities are recomputed and compared with the recorded ones, so a row
    /// whose key says one thing and whose payload says another cannot be loaded.
    pub fn decode(value: &str) -> Result<DeliveryProjection, DeliveryProjectionWireError> {
        // The version is read first so an unknown durable format is reported as
        // such instead of as the shape error its different fields would cause.
        let probe: WireVersion =
            serde_json::from_str(value).map_err(|_| DeliveryProjectionWireError::Malformed)?;
        if probe.version != Self::VERSION {
            return Err(DeliveryProjectionWireError::UnsupportedVersion(
                probe.version,
            ));
        }
        let wire: WireDeliveryProjection =
            serde_json::from_str(value).map_err(|_| DeliveryProjectionWireError::Malformed)?;

        let source_projection_sha256 = sha256(&wire.source_projection_sha256)?;
        let snapshot_id = snapshot_id(wire.snapshot_id)?;
        let managed_root = managed_root(&wire.managed_root)?;
        if wire.text.source_projection_sha256 != wire.source_projection_sha256 {
            return Err(field_mismatch("text.source_projection_sha256"));
        }
        if wire.text.snapshot_id != wire.snapshot_id {
            return Err(field_mismatch("text.snapshot_id"));
        }
        if wire.text.managed_root != wire.managed_root {
            return Err(field_mismatch("text.managed_root"));
        }

        let mut files = Vec::with_capacity(wire.text.files.len());
        let mut previous: Option<ProjectionTargetPath> = None;
        for file in &wire.text.files {
            let target_path = ProjectionTargetPath::new(file.target_path.clone())
                .map_err(|_| DeliveryProjectionWireError::Malformed)?;
            if previous.as_ref().is_some_and(|path| path >= &target_path) {
                return Err(DeliveryProjectionWireError::NonCanonicalOrder {
                    collection: "text.files",
                });
            }
            previous = Some(target_path.clone());
            files.push(TextProjectionFile::from_parts(
                target_path,
                sha256(&file.blob_sha256)?,
                content_path(&file.source_path)?,
                sha256(&file.source_sha256)?,
            ));
        }
        let text = TextProjection::from_parts(
            snapshot_id,
            managed_root.clone(),
            source_projection_sha256,
            files,
        );
        let recorded_text_sha256 = sha256(&wire.text.projection_sha256)?;
        if text.projection_sha256() != recorded_text_sha256 {
            return Err(DeliveryProjectionWireError::TextProjectionHashMismatch {
                recorded: recorded_text_sha256,
                recomputed: text.projection_sha256(),
            });
        }

        let mut assets = Vec::with_capacity(wire.assets.len());
        let mut previous: Option<ContentPath> = None;
        for asset in &wire.assets {
            let logical_path = content_path(&asset.logical_path)?;
            if previous.as_ref().is_some_and(|path| path >= &logical_path) {
                return Err(DeliveryProjectionWireError::NonCanonicalOrder {
                    collection: "assets",
                });
            }
            previous = Some(logical_path.clone());
            let published_sha256 = sha256(&asset.published_sha256)?;
            let object_key = AssetObjectKey::for_published_sha256(&published_sha256);
            if object_key.as_str() != asset.object_key {
                return Err(DeliveryProjectionWireError::ObjectKeyMismatch {
                    logical_path: asset.logical_path.clone(),
                });
            }
            let public_url = public_url(&asset.public_url, &object_key, &asset.logical_path)?;
            assets.push(PublishedAsset::from_parts(
                logical_path,
                sha256(&asset.source_sha256)?,
                published_sha256,
                asset.published_size,
                AssetContentType::new(asset.published_content_type.clone())
                    .map_err(|_| DeliveryProjectionWireError::Malformed)?,
                object_key,
                public_url,
            ));
        }
        let assets = AssetProjection::from_assets(assets);

        let projection = DeliveryProjection::from_parts(
            source_projection_sha256,
            snapshot_id,
            managed_root,
            text,
            assets,
        );
        let recorded_delivery_sha256 = sha256(&wire.delivery_sha256)?;
        if projection.delivery_sha256() != recorded_delivery_sha256 {
            return Err(
                DeliveryProjectionWireError::DeliveryProjectionHashMismatch {
                    recorded: recorded_delivery_sha256,
                    recomputed: projection.delivery_sha256(),
                },
            );
        }
        Ok(projection)
    }
}

/// Rebuilds one asset URL from the object key and validates the base it implies.
///
/// A stored URL is not trusted: stripping the canonical `/<object_key>` suffix must
/// leave a base URL that itself passes validation, and rebuilding from that base
/// must reproduce the stored URL byte for byte.
fn public_url(
    value: &str,
    object_key: &AssetObjectKey,
    logical_path: &str,
) -> Result<AssetPublicUrl, DeliveryProjectionWireError> {
    let suffix = format!("/{object_key}");
    let Some(base) = value.strip_suffix(&suffix) else {
        return Err(DeliveryProjectionWireError::PublicUrlMismatch {
            logical_path: logical_path.to_owned(),
        });
    };
    let base = AssetPublicBaseUrl::new(base).map_err(|_| {
        DeliveryProjectionWireError::PublicUrlMismatch {
            logical_path: logical_path.to_owned(),
        }
    })?;
    let config = AssetDeliveryConfig::new(base.as_str()).map_err(|_| {
        DeliveryProjectionWireError::PublicUrlMismatch {
            logical_path: logical_path.to_owned(),
        }
    })?;
    let rebuilt = config.public_url(object_key);
    if rebuilt.as_str() != value {
        return Err(DeliveryProjectionWireError::PublicUrlMismatch {
            logical_path: logical_path.to_owned(),
        });
    }
    Ok(rebuilt)
}

fn sha256(value: &str) -> Result<Sha256, DeliveryProjectionWireError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DeliveryProjectionWireError::Malformed);
    }
    let mut bytes = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(chunk).expect("hexadecimal bytes are ASCII");
        bytes[index] = u8::from_str_radix(text, 16).expect("validated hexadecimal");
    }
    Ok(Sha256::new(bytes))
}

fn content_path(value: &str) -> Result<ContentPath, DeliveryProjectionWireError> {
    ContentPath::new(value).map_err(|_| DeliveryProjectionWireError::Malformed)
}

fn managed_root(value: &str) -> Result<ManagedRoot, DeliveryProjectionWireError> {
    ManagedRoot::new(value).map_err(|_| DeliveryProjectionWireError::Malformed)
}

fn snapshot_id(value: u64) -> Result<SnapshotId, DeliveryProjectionWireError> {
    SnapshotId::new(value).map_err(|_| DeliveryProjectionWireError::Malformed)
}

fn field_mismatch(field: &'static str) -> DeliveryProjectionWireError {
    DeliveryProjectionWireError::CrossFieldMismatch { field }
}

#[derive(Deserialize)]
struct WireVersion {
    version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDeliveryProjection {
    version: u32,
    delivery_sha256: String,
    source_projection_sha256: String,
    snapshot_id: u64,
    managed_root: String,
    text: WireTextProjection,
    assets: Vec<WireAsset>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTextProjection {
    source_projection_sha256: String,
    projection_sha256: String,
    snapshot_id: u64,
    managed_root: String,
    files: Vec<WireTextFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTextFile {
    target_path: String,
    blob_sha256: String,
    source_path: String,
    source_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAsset {
    logical_path: String,
    source_sha256: String,
    published_sha256: String,
    published_size: u64,
    published_content_type: String,
    object_key: String,
    public_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryProjectionWireError {
    /// The payload is not a JSON object with the versioned shape, or carries a
    /// fact that no domain constructor accepts.
    Malformed,
    /// The payload declares a durable version this engine does not understand.
    UnsupportedVersion(u32),
    /// A collection is not in the single canonical order the identity is defined
    /// over (strictly ascending, no duplicates).
    NonCanonicalOrder { collection: &'static str },
    /// The stored object key is not the content-addressed key of the stored bytes.
    ObjectKeyMismatch { logical_path: String },
    /// The stored URL is not the deterministic URL of the stored object key.
    PublicUrlMismatch { logical_path: String },
    /// Two copies of the same fact inside one payload disagree.
    CrossFieldMismatch { field: &'static str },
    /// The stored text identity does not describe the stored text files.
    TextProjectionHashMismatch {
        recorded: Sha256,
        recomputed: Sha256,
    },
    /// The stored delivery identity does not describe the stored delivery facts.
    DeliveryProjectionHashMismatch {
        recorded: Sha256,
        recomputed: Sha256,
    },
}

impl fmt::Display for DeliveryProjectionWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("encoded delivery projection is malformed"),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "unsupported delivery projection version: {version}"
            ),
            Self::NonCanonicalOrder { collection } => write!(
                formatter,
                "encoded delivery projection has a non-canonical {collection} order"
            ),
            Self::ObjectKeyMismatch { logical_path } => write!(
                formatter,
                "encoded delivery projection has a non-canonical object key for {logical_path}"
            ),
            Self::PublicUrlMismatch { logical_path } => write!(
                formatter,
                "encoded delivery projection has a non-deterministic public URL for {logical_path}"
            ),
            Self::CrossFieldMismatch { field } => write!(
                formatter,
                "encoded delivery projection disagrees with itself about {field}"
            ),
            Self::TextProjectionHashMismatch { .. } => formatter
                .write_str("encoded delivery projection text identity does not describe its files"),
            Self::DeliveryProjectionHashMismatch { .. } => formatter
                .write_str("encoded delivery projection identity does not describe its facts"),
        }
    }
}

impl Error for DeliveryProjectionWireError {}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        domain::{Snapshot, SnapshotFile, SourceId},
        ports::{BlobStore, ContentStoreError},
        workflow::{
            AssetContentType, AssetDeliveryConfig, AssetReviewRunId, DeliveryProjectionBuilder,
            FinalPublicationSet, ManagedRoot, PublicProjection, SanitizationTransformation,
            SanitizedAsset,
        },
    };

    use super::*;

    const DOCUMENT: &str = "# A\n\n![[img/a.png]]\n![[files/a.pdf]]\n";

    #[derive(Default)]
    struct MemoryStore(std::collections::HashMap<Sha256, Vec<u8>>);

    impl MemoryStore {
        fn insert(&mut self, bytes: &[u8]) -> Sha256 {
            let identity = Sha256::digest(bytes);
            self.0.insert(identity, bytes.to_vec());
            identity
        }
    }

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            Ok(Sha256::digest(content))
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn digest(value: u8) -> Sha256 {
        Sha256::new([value; 32])
    }

    fn asset(
        name: &str,
        source: Sha256,
        published: Sha256,
        size: u64,
        media_type: &str,
    ) -> SanitizedAsset {
        SanitizedAsset::from_parts(
            path(name),
            AssetReviewRunId::new(1).unwrap(),
            source,
            published,
            size,
            AssetContentType::new(media_type).unwrap(),
            vec![SanitizationTransformation::StripMetadata],
        )
    }

    /// A real, non-trivial projection: two rewritten documents, two assets, one
    /// of which is a shared physical object.
    fn projection() -> (DeliveryProjection, MemoryStore) {
        let mut store = MemoryStore::default();
        let mut files = vec![
            SnapshotFile::new(
                path("a.md"),
                DOCUMENT.len() as u64,
                store.insert(DOCUMENT.as_bytes()),
                None,
            ),
            SnapshotFile::new(
                path("notes/b.md"),
                DOCUMENT.len() as u64,
                store.insert(DOCUMENT.as_bytes()),
                None,
            ),
            SnapshotFile::new(path("img/a.png"), 100, digest(1), None),
            SnapshotFile::new(path("files/a.pdf"), 200, digest(3), None),
        ];
        files.sort_by(|left, right| left.path().cmp(right.path()));
        let snapshot = Snapshot::new(
            SnapshotId::new(7).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files,
        )
        .unwrap();
        let set = FinalPublicationSet::from_parts_for_test(
            snapshot.id(),
            vec![path("a.md"), path("notes/b.md")],
            vec![
                asset("img/a.png", digest(1), digest(2), 4242, "image/jpeg"),
                asset("files/a.pdf", digest(3), digest(3), 200, "application/pdf"),
            ],
        );
        let public =
            PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap();
        let projection = DeliveryProjectionBuilder::build(
            &public,
            &snapshot,
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
            &store,
        )
        .unwrap();
        (projection, store)
    }

    #[test]
    fn version_one_pins_the_field_names_and_round_trips() {
        let (projection, _) = projection();

        let encoded = DeliveryProjectionWire::encode(&projection);
        let decoded = DeliveryProjectionWire::decode(&encoded).unwrap();

        assert_eq!(decoded, projection);
        assert!(encoded.contains("\"version\":1"));
        assert!(encoded.contains("\"delivery_sha256\":"));
        assert!(encoded.contains("\"source_projection_sha256\":"));
        assert!(encoded.contains("\"published_content_type\":\"image/jpeg\""));
        assert!(encoded.contains("\"published_size\":4242"));
        assert!(encoded.contains("\"object_key\":\"assets/sha256/"));
        // Only portable facts: no runtime handle, no filesystem path.
        assert!(!encoded.contains("PathBuf"));
        assert!(!encoded.contains("/srv/"));
        // Canonical collection order is part of the encoding.
        let first = encoded.find("content/a.md").unwrap();
        let second = encoded.find("content/notes/b.md").unwrap();
        assert!(first < second, "text files are ordered by target path");
    }

    #[test]
    fn an_unknown_durable_version_is_rejected() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);

        assert_eq!(
            DeliveryProjectionWire::decode(&encoded.replace("\"version\":1", "\"version\":2")),
            Err(DeliveryProjectionWireError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn a_payload_that_is_not_the_versioned_shape_is_rejected() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);

        for value in [
            "".to_owned(),
            "null".to_owned(),
            "[]".to_owned(),
            "not json".to_owned(),
            "{}".to_owned(),
            "{\"version\":1}".to_owned(),
            "{\"version\":\"1\"}".to_owned(),
            encoded.replace("\"assets\":", "\"unexpected\":1,\"assets\":"),
            encoded.replace("\"published_size\":4242", "\"published_size\":-1"),
            encoded.replace("\"published_size\":4242", "\"published_size\":1.5"),
            encoded.replace("\"snapshot_id\":7", "\"snapshot_id\":0"),
            encoded.replace(
                "\"published_content_type\":\"image/jpeg\"",
                "\"published_content_type\":\"image/jpeg; charset=binary\"",
            ),
            encoded.replace(
                "\"target_path\":\"content/a.md\"",
                "\"target_path\":\"../escape.md\"",
            ),
        ] {
            assert_eq!(
                DeliveryProjectionWire::decode(&value),
                Err(DeliveryProjectionWireError::Malformed),
                "accepted {value:?}"
            );
        }
    }

    /// The stored key is a *claim*: the payload has to prove it.
    #[test]
    fn an_identity_that_does_not_describe_the_payload_is_rejected() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);

        // Rewrite the recorded delivery identity while every other byte stays the
        // same: this is exactly the `key = HASH_A, payload = HASH_B` corruption.
        let tampered = encoded.replace(
            &projection.delivery_sha256().to_string(),
            &digest(0xaa).to_string(),
        );
        assert!(matches!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::DeliveryProjectionHashMismatch { .. })
        ));

        let tampered = encoded.replace(
            &projection.text().projection_sha256().to_string(),
            &digest(0xbb).to_string(),
        );
        assert!(matches!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::TextProjectionHashMismatch { .. })
        ));

        // A recorded identity that is not even a hash is malformed, not missing.
        let tampered = encoded.replace(&projection.delivery_sha256().to_string(), &"z".repeat(64));
        assert_eq!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::Malformed)
        );
    }

    #[test]
    fn a_tampered_published_fact_is_rejected_even_when_the_hashes_are_rewritten() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);

        // Rewriting a published size while keeping the recorded identity must not
        // decode: the identity covers the publication facts.
        let tampered = encoded.replace("\"published_size\":4242", "\"published_size\":9");
        assert!(matches!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::DeliveryProjectionHashMismatch { .. })
        ));
    }

    #[test]
    fn a_non_canonical_object_key_or_url_is_rejected() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);
        let key = projection.assets().assets()[0]
            .object_key()
            .as_str()
            .to_owned();
        let url = projection.assets().assets()[0]
            .public_url()
            .as_str()
            .to_owned();

        let tampered = encoded.replace(&key, &format!("logical/{key}"));
        assert!(matches!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::ObjectKeyMismatch { .. })
        ));

        let tampered = encoded.replace(&url, "https://elsewhere.example.com/a.png");
        assert!(matches!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::PublicUrlMismatch { .. })
        ));

        // A URL that still ends with the key but with a non-canonical base.
        let tampered = encoded.replace(&url, &format!("https://assets.example.com//{key}"));
        assert!(matches!(
            DeliveryProjectionWire::decode(&tampered),
            Err(DeliveryProjectionWireError::PublicUrlMismatch { .. })
        ));
    }

    #[test]
    fn a_duplicate_or_unsorted_collection_is_rejected() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);

        let mut value: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        // Duplicate a text file entry: duplicates cannot be canonical.
        let mut duplicated = value.clone();
        let files = duplicated["text"]["files"].as_array_mut().unwrap();
        let first = files[0].clone();
        files.push(first);
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&duplicated).unwrap()),
            Err(DeliveryProjectionWireError::NonCanonicalOrder {
                collection: "text.files"
            })
        );

        // Same content, wrong order.
        let files = value["text"]["files"].as_array_mut().unwrap();
        files.reverse();
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&value).unwrap()),
            Err(DeliveryProjectionWireError::NonCanonicalOrder {
                collection: "text.files"
            })
        );

        // Assets are held to the same rule.
        let mut duplicated = serde_json::from_str::<serde_json::Value>(&encoded).unwrap();
        let assets = duplicated["assets"].as_array_mut().unwrap();
        let first = assets[0].clone();
        assets.push(first);
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&duplicated).unwrap()),
            Err(DeliveryProjectionWireError::NonCanonicalOrder {
                collection: "assets"
            })
        );
    }

    #[test]
    fn an_internally_inconsistent_payload_is_rejected() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);

        for (from, to, field) in [
            (
                "\"snapshot_id\":7,\"managed_root\":\"content\",\"text\":{\"source_projection_sha256\":\"",
                "\"snapshot_id\":8,\"managed_root\":\"content\",\"text\":{\"source_projection_sha256\":\"",
                "text.snapshot_id",
            ),
            (
                "\"managed_root\":\"content\",\"text\":{\"source_projection_sha256\"",
                "\"managed_root\":\"public\",\"text\":{\"source_projection_sha256\"",
                "text.managed_root",
            ),
        ] {
            let tampered = encoded.replacen(from, to, 1);
            assert_eq!(
                DeliveryProjectionWire::decode(&tampered),
                Err(DeliveryProjectionWireError::CrossFieldMismatch { field }),
                "accepted {field} mismatch"
            );
        }
    }

    #[test]
    fn an_empty_projection_round_trips_with_an_empty_asset_set() {
        let store = MemoryStore::default();
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![],
        )
        .unwrap();
        let public = PublicProjection::build(
            &FinalPublicationSet::from_parts_for_test(snapshot.id(), vec![], vec![]),
            &snapshot,
            ManagedRoot::repository_root(),
        )
        .unwrap();
        let projection = DeliveryProjectionBuilder::build(
            &public,
            &snapshot,
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
            &store,
        )
        .unwrap();

        let encoded = DeliveryProjectionWire::encode(&projection);
        let decoded = DeliveryProjectionWire::decode(&encoded).unwrap();

        assert_eq!(decoded, projection);
        assert!(decoded.assets().is_empty());
        assert!(encoded.contains("\"assets\":[]"));
    }
}
