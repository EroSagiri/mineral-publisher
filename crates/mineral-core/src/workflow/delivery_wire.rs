use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::domain::{ContentPath, Sha256, SnapshotId};

use super::{
    AssetContentType, AssetDeliveryConfig, AssetObjectKey, AssetProjection, AssetPublicBaseUrl,
    AssetPublicFilename, AssetPublicUrl, DeliveryIdentityVersion, DeliveryProjection, ManagedRoot,
    ProjectionTargetPath, PublishedAsset, TextProjection, TextProjectionFile,
};

/// Versioned durable encoding of one immutable [`DeliveryProjection`].
///
/// A stored projection is the only thing a later execution may rematerialize a
/// reviewed tree from, so the payload has to carry its own version: when the
/// delivery model changes shape, an old durable row must fail loudly instead of
/// being silently bound to today's field set.
///
/// Version 3 is what this engine writes. Its fields are version 2's; what changed
/// is the delivery identity those fields are hashed into. Versions 1 and 2 hashed
/// the delivered text tree and the published assets but not the snapshot provenance
/// the intent stores, so two different payloads could share one durable key — and a
/// store that keys on that identity then had to refuse the second one. Version 3
/// hashes every immutable fact, and older rows keep the identity they recorded.
///
/// Version 2 is version 1 plus one frozen presentation fact per asset:
///
/// ```json
/// {
///   "version": 2,
///   "delivery_sha256": "…",
///   "source_projection_sha256": "…",
///   "snapshot_id": 1,
///   "managed_root": "content",
///   "text": { "…": "…", "files": [ … ] },
///   "assets": [
///     { "logical_path": "…", "source_sha256": "…", "published_sha256": "…",
///       "published_size": 0, "published_content_type": "image/png",
///       "public_filename": "photo.png",
///       "object_key": "assets/sha256/ab/<hash>/photo.png", "public_url": "…" }
///   ]
/// }
/// ```
///
/// Version 1 is still decoded, and only for recovery. It froze keys of the shape
/// `assets/sha256/<2 hex>/<64 hex>` and no filename at all. Those payloads are
/// immutable: this decoder rebuilds the exact legacy object key they recorded,
/// and [`DeliveryProjectionWire::encode`] reproduces a decoded V1 payload as V1
/// rather than upgrading it, so a historical intent can never be silently
/// re-pointed at objects it never published.
///
/// Only portable facts appear here: no runtime handle, no path, no credential,
/// and no base URL that is not already implied by each asset's final URL.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeliveryProjectionWire;

impl DeliveryProjectionWire {
    /// The durable version this engine writes.
    pub const VERSION: u32 = 3;

    /// The filename-bearing version, whose identity left the snapshot provenance
    /// out. Read-only.
    pub const VERSION_2: u32 = 2;

    /// The filename-less version. Read-only, and only in order to recover an intent
    /// published before S6.4.1.
    pub const LEGACY_VERSION: u32 = 1;

    /// Encodes one projection in the version its own facts belong to.
    ///
    /// A projection built now carries a frozen filename for every asset and is
    /// written as V2. A projection decoded from a V1 row carries none and is
    /// written back as V1, byte for byte the same shape: recovery reads history, it
    /// does not rewrite it.
    pub fn encode(projection: &DeliveryProjection) -> String {
        let assets = projection.assets().assets();
        let legacy =
            !assets.is_empty() && assets.iter().all(|asset| asset.public_filename().is_none());
        if legacy {
            let wire = WireDeliveryProjectionV1 {
                version: Self::LEGACY_VERSION,
                delivery_sha256: projection.delivery_sha256().to_string(),
                source_projection_sha256: projection.source_projection_sha256().to_string(),
                snapshot_id: projection.snapshot_id().get(),
                managed_root: projection.managed_root().as_str().to_owned(),
                text: wire_text(projection),
                assets: assets
                    .iter()
                    .map(|asset| WireAssetV1 {
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
            return serde_json::to_string(&wire)
                .expect("a delivery projection is always representable as a JSON object");
        }
        // A payload is written in the version whose identity function produced its
        // key, so re-encoding a decoded row reproduces that row instead of silently
        // re-identifying history.
        let version = match projection.identity_version() {
            DeliveryIdentityVersion::V1 => Self::VERSION_2,
            DeliveryIdentityVersion::V2 => Self::VERSION,
        };
        let wire = WireDeliveryProjectionV2 {
            version,
            delivery_sha256: projection.delivery_sha256().to_string(),
            source_projection_sha256: projection.source_projection_sha256().to_string(),
            snapshot_id: projection.snapshot_id().get(),
            managed_root: projection.managed_root().as_str().to_owned(),
            text: wire_text(projection),
            assets: assets
                .iter()
                .map(|asset| WireAssetV2 {
                    logical_path: asset.logical_path().as_str().to_owned(),
                    source_sha256: asset.source_sha256().to_string(),
                    published_sha256: asset.published_sha256().to_string(),
                    published_size: asset.published_size(),
                    published_content_type: asset.published_content_type().as_str().to_owned(),
                    public_filename: asset
                        .public_filename()
                        .expect("a current delivery projection freezes one filename per asset")
                        .clone(),
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
        match probe.version {
            Self::LEGACY_VERSION => Self::decode_v1(value),
            Self::VERSION_2 => Self::decode_filenames(value, DeliveryIdentityVersion::V1),
            Self::VERSION => Self::decode_filenames(value, DeliveryIdentityVersion::V2),
            other => Err(DeliveryProjectionWireError::UnsupportedVersion(other)),
        }
    }

    /// Rebuilds a version 1 payload, whose assets carry no presentation filename.
    fn decode_v1(value: &str) -> Result<DeliveryProjection, DeliveryProjectionWireError> {
        let wire: WireDeliveryProjectionV1 =
            serde_json::from_str(value).map_err(|_| DeliveryProjectionWireError::Malformed)?;
        let (source_projection_sha256, snapshot_id, managed_root) = shared_facts(
            &wire.source_projection_sha256,
            wire.snapshot_id,
            &wire.managed_root,
        )?;
        let text = decode_text(
            &wire.text,
            source_projection_sha256,
            snapshot_id,
            &managed_root,
        )?;

        let mut assets = Vec::with_capacity(wire.assets.len());
        let mut previous: Option<ContentPath> = None;
        for asset in &wire.assets {
            let logical_path = ordered_asset_path(&mut previous, &asset.logical_path)?;
            let published_sha256 = sha256(&asset.published_sha256)?;
            let object_key = AssetObjectKey::legacy_for_published_sha256(&published_sha256);
            if object_key.as_str() != asset.object_key {
                return Err(DeliveryProjectionWireError::ObjectKeyMismatch {
                    logical_path: asset.logical_path.clone(),
                });
            }
            let public_url = public_url(&asset.public_url, &object_key, &asset.logical_path)?;
            assets.push(PublishedAsset::from_legacy_parts(
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
        finish(
            source_projection_sha256,
            snapshot_id,
            managed_root,
            text,
            assets,
            DeliveryIdentityVersion::V1,
            &wire.delivery_sha256,
        )
    }

    /// Rebuilds a filename-bearing payload, cross-checking every presentation fact.
    ///
    /// The filename is not trusted because it is stored: it must be exactly the
    /// filename the frozen logical path and the frozen media type derive, and the
    /// stored key must be exactly the key that filename and the published digest
    /// produce. A payload that names the right digest under the wrong filename is
    /// a different delivery intent, not a recoverable one.
    ///
    /// Versions 2 and 3 share these fields; the caller states which identity
    /// function the recorded key was computed with, because reproducing an old key
    /// is the only way an old intent stays recoverable.
    fn decode_filenames(
        value: &str,
        identity: DeliveryIdentityVersion,
    ) -> Result<DeliveryProjection, DeliveryProjectionWireError> {
        let wire: WireDeliveryProjectionV2 =
            serde_json::from_str(value).map_err(|_| DeliveryProjectionWireError::Malformed)?;
        let (source_projection_sha256, snapshot_id, managed_root) = shared_facts(
            &wire.source_projection_sha256,
            wire.snapshot_id,
            &wire.managed_root,
        )?;
        let text = decode_text(
            &wire.text,
            source_projection_sha256,
            snapshot_id,
            &managed_root,
        )?;

        let mut assets = Vec::with_capacity(wire.assets.len());
        let mut previous: Option<ContentPath> = None;
        for asset in &wire.assets {
            let logical_path = ordered_asset_path(&mut previous, &asset.logical_path)?;
            let published_sha256 = sha256(&asset.published_sha256)?;
            let content_type = AssetContentType::new(asset.published_content_type.clone())
                .map_err(|_| DeliveryProjectionWireError::Malformed)?;
            let derived = AssetPublicFilename::from_logical_path(&logical_path, &content_type)
                .map_err(|_| DeliveryProjectionWireError::PublicFilenameMismatch {
                    logical_path: asset.logical_path.clone(),
                })?;
            if derived != asset.public_filename {
                return Err(DeliveryProjectionWireError::PublicFilenameMismatch {
                    logical_path: asset.logical_path.clone(),
                });
            }
            let object_key =
                AssetObjectKey::for_published_asset(&published_sha256, &asset.public_filename);
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
                content_type,
                asset.public_filename.clone(),
                public_url,
            ));
        }
        finish(
            source_projection_sha256,
            snapshot_id,
            managed_root,
            text,
            assets,
            identity,
            &wire.delivery_sha256,
        )
    }
}

fn wire_text(projection: &DeliveryProjection) -> WireTextProjection {
    WireTextProjection {
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
    }
}

/// The three facts both versions state once and the text side repeats.
fn shared_facts(
    source_projection_sha256: &str,
    snapshot_id: u64,
    managed_root: &str,
) -> Result<(Sha256, SnapshotId, ManagedRoot), DeliveryProjectionWireError> {
    Ok((
        sha256(source_projection_sha256)?,
        self::snapshot_id(snapshot_id)?,
        self::managed_root(managed_root)?,
    ))
}

fn ordered_asset_path(
    previous: &mut Option<ContentPath>,
    logical_path: &str,
) -> Result<ContentPath, DeliveryProjectionWireError> {
    let logical_path = content_path(logical_path)?;
    if previous.as_ref().is_some_and(|path| path >= &logical_path) {
        return Err(DeliveryProjectionWireError::NonCanonicalOrder {
            collection: "assets",
        });
    }
    *previous = Some(logical_path.clone());
    Ok(logical_path)
}

fn decode_text(
    wire: &WireTextProjection,
    source_projection_sha256: Sha256,
    snapshot_id: SnapshotId,
    managed_root: &ManagedRoot,
) -> Result<TextProjection, DeliveryProjectionWireError> {
    // The text side restates the snapshot-level facts; a payload whose two copies
    // disagree describes no single intent.
    if wire.source_projection_sha256 != source_projection_sha256.to_string() {
        return Err(field_mismatch("text.source_projection_sha256"));
    }
    if wire.snapshot_id != snapshot_id.get() {
        return Err(field_mismatch("text.snapshot_id"));
    }
    if wire.managed_root != managed_root.as_str() {
        return Err(field_mismatch("text.managed_root"));
    }
    let mut files = Vec::with_capacity(wire.files.len());
    let mut previous: Option<ProjectionTargetPath> = None;
    for file in &wire.files {
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
    let recorded_text_sha256 = sha256(&wire.projection_sha256)?;
    if text.projection_sha256() != recorded_text_sha256 {
        return Err(DeliveryProjectionWireError::TextProjectionHashMismatch {
            recorded: recorded_text_sha256,
            recomputed: text.projection_sha256(),
        });
    }
    Ok(text)
}

#[allow(clippy::too_many_arguments)]
fn finish(
    source_projection_sha256: Sha256,
    snapshot_id: SnapshotId,
    managed_root: ManagedRoot,
    text: TextProjection,
    assets: Vec<PublishedAsset>,
    identity: DeliveryIdentityVersion,
    recorded_delivery_sha256: &str,
) -> Result<DeliveryProjection, DeliveryProjectionWireError> {
    let assets = AssetProjection::from_assets(assets);
    let projection = match identity {
        DeliveryIdentityVersion::V1 => DeliveryProjection::from_parts_with_legacy_identity(
            source_projection_sha256,
            snapshot_id,
            managed_root,
            text,
            assets,
        ),
        DeliveryIdentityVersion::V2 => DeliveryProjection::from_parts(
            source_projection_sha256,
            snapshot_id,
            managed_root,
            text,
            assets,
        ),
    };
    let recorded_delivery_sha256 = sha256(recorded_delivery_sha256)?;
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
    let suffix = format!("/{}", object_key.as_url_path());
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
struct WireDeliveryProjectionV1 {
    version: u32,
    delivery_sha256: String,
    source_projection_sha256: String,
    snapshot_id: u64,
    managed_root: String,
    text: WireTextProjection,
    assets: Vec<WireAssetV1>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDeliveryProjectionV2 {
    version: u32,
    delivery_sha256: String,
    source_projection_sha256: String,
    snapshot_id: u64,
    managed_root: String,
    text: WireTextProjection,
    assets: Vec<WireAssetV2>,
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
struct WireAssetV1 {
    logical_path: String,
    source_sha256: String,
    published_sha256: String,
    published_size: u64,
    published_content_type: String,
    object_key: String,
    public_url: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAssetV2 {
    logical_path: String,
    source_sha256: String,
    published_sha256: String,
    published_size: u64,
    published_content_type: String,
    public_filename: AssetPublicFilename,
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
    /// The stored object key is not the content-addressed, filename-bearing key of
    /// the stored bytes and filename.
    ObjectKeyMismatch { logical_path: String },
    /// The stored presentation filename is not the one the stored logical path and
    /// published media type derive.
    PublicFilenameMismatch { logical_path: String },
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
            Self::PublicFilenameMismatch { logical_path } => write!(
                formatter,
                "encoded delivery projection names a presentation filename that does not belong to {logical_path}"
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
    fn version_three_pins_the_field_names_and_round_trips() {
        let (projection, _) = projection();

        let encoded = DeliveryProjectionWire::encode(&projection);
        let decoded = DeliveryProjectionWire::decode(&encoded).unwrap();

        assert_eq!(decoded, projection);
        assert!(encoded.contains("\"version\":3"));
        assert!(encoded.contains("\"delivery_sha256\":"));
        assert!(encoded.contains("\"source_projection_sha256\":"));
        assert!(encoded.contains("\"published_content_type\":\"image/jpeg\""));
        assert!(encoded.contains("\"published_size\":4242"));
        // The presentation filename is frozen next to the facts it was derived
        // from, so recovery never has to re-derive it.
        assert!(encoded.contains("\"public_filename\":\"a.jpg\""));
        assert!(encoded.contains("\"public_filename\":\"a.pdf\""));
        assert!(
            encoded.contains("\"object_key\":\"assets/sha256/02/") && encoded.contains("/a.jpg\""),
            "the key carries the filename segment: {encoded}"
        );
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
            DeliveryProjectionWire::decode(&encoded.replace("\"version\":3", "\"version\":4")),
            Err(DeliveryProjectionWireError::UnsupportedVersion(4))
        );
    }

    /// The exact bytes this engine wrote once keys carried a filename, before the
    /// delivery identity covered the snapshot provenance the payload stores.
    ///
    /// It is kept verbatim because a row already in someone's database has to keep
    /// loading: its recorded identity was computed without the snapshot id, and the
    /// decoder must reproduce that identity rather than today's.
    const FROZEN_VERSION_TWO_PAYLOAD: &str = concat!(
        "{\"version\":2,",
        "\"delivery_sha256\":\"ab4637adf0fa30c1d637f75201d8f345f3bf976fd82d00c8e11e50727c35731b\",",
        "\"source_projection_sha256\":\"e6a6a5a6a237f81d871a85086c3b4a445505ccd9429dce5fe0ecf26de5e6ebcd\",",
        "\"snapshot_id\":7,\"managed_root\":\"content\",",
        "\"text\":{",
        "\"source_projection_sha256\":\"e6a6a5a6a237f81d871a85086c3b4a445505ccd9429dce5fe0ecf26de5e6ebcd\",",
        "\"projection_sha256\":\"13a9eb9b718d839b4f028d61be87d9449d515ffeda3cc72ad4f58aa6dc7652ec\",",
        "\"snapshot_id\":7,\"managed_root\":\"content\",",
        "\"files\":[",
        "{\"target_path\":\"content/a.md\",\"blob_sha256\":\"3f2f9417578b39902aa45f417c47fa52df8b2e3c5bc0cbffe95c2e8c2e22244e\",",
        "\"source_path\":\"a.md\",\"source_sha256\":\"3a660235b7d958fb2f3b9d50fba11ba5e1c2b9c0063d58503f315174d0a33f7f\"},",
        "{\"target_path\":\"content/notes/b.md\",\"blob_sha256\":\"3f2f9417578b39902aa45f417c47fa52df8b2e3c5bc0cbffe95c2e8c2e22244e\",",
        "\"source_path\":\"notes/b.md\",\"source_sha256\":\"3a660235b7d958fb2f3b9d50fba11ba5e1c2b9c0063d58503f315174d0a33f7f\"}]},",
        "\"assets\":[",
        "{\"logical_path\":\"files/a.pdf\",",
        "\"source_sha256\":\"0303030303030303030303030303030303030303030303030303030303030303\",",
        "\"published_sha256\":\"0303030303030303030303030303030303030303030303030303030303030303\",",
        "\"published_size\":200,\"published_content_type\":\"application/pdf\",\"public_filename\":\"a.pdf\",",
        "\"object_key\":\"assets/sha256/03/0303030303030303030303030303030303030303030303030303030303030303/a.pdf\",",
        "\"public_url\":\"https://assets.example.com/assets/sha256/03/0303030303030303030303030303030303030303030303030303030303030303/a.pdf\"},",
        "{\"logical_path\":\"img/a.png\",",
        "\"source_sha256\":\"0101010101010101010101010101010101010101010101010101010101010101\",",
        "\"published_sha256\":\"0202020202020202020202020202020202020202020202020202020202020202\",",
        "\"published_size\":4242,\"published_content_type\":\"image/jpeg\",\"public_filename\":\"a.jpg\",",
        "\"object_key\":\"assets/sha256/02/0202020202020202020202020202020202020202020202020202020202020202/a.jpg\",",
        "\"public_url\":\"https://assets.example.com/assets/sha256/02/0202020202020202020202020202020202020202020202020202020202020202/a.jpg\"}]}",
    );

    /// A version 2 row still decodes to the identity it recorded, and re-encodes as
    /// version 2 rather than being silently re-identified under today's function.
    #[test]
    fn a_frozen_version_two_payload_keeps_its_recorded_identity() {
        let decoded = DeliveryProjectionWire::decode(FROZEN_VERSION_TWO_PAYLOAD).unwrap();

        assert_eq!(
            decoded.delivery_sha256().to_string(),
            "ab4637adf0fa30c1d637f75201d8f345f3bf976fd82d00c8e11e50727c35731b"
        );
        assert_eq!(
            DeliveryProjectionWire::encode(&decoded),
            FROZEN_VERSION_TWO_PAYLOAD
        );
    }

    /// The identity is the durable key of an immutable intent, so it covers every
    /// fact the intent stores.
    ///
    /// This is the failure a real workspace hit: publishing the same delivered
    /// content from a new snapshot (an unrelated file had been added to the vault)
    /// recomputed the same key for a different payload, and the store — which
    /// promises one key, one payload — refused to persist it.
    #[test]
    fn the_identity_covers_the_snapshot_provenance() {
        let (projection, _) = projection();
        let other_snapshot = SnapshotId::new(8).unwrap();
        let text = TextProjection::from_parts(
            other_snapshot,
            projection.managed_root().clone(),
            projection.source_projection_sha256(),
            projection.text().files().to_vec(),
        );
        let reidentified = DeliveryProjection::from_parts(
            projection.source_projection_sha256(),
            other_snapshot,
            projection.managed_root().clone(),
            text,
            projection.assets().clone(),
        );

        assert_ne!(
            reidentified.delivery_sha256(),
            projection.delivery_sha256(),
            "the same delivered bytes from another snapshot are another intent"
        );
        // Both payloads are valid on their own terms.
        assert_eq!(
            DeliveryProjectionWire::decode(&DeliveryProjectionWire::encode(&projection)).unwrap(),
            projection
        );
        assert_eq!(
            DeliveryProjectionWire::decode(&DeliveryProjectionWire::encode(&reidentified)).unwrap(),
            reidentified
        );
    }

    /// A document whose *delivered* bytes are unchanged but whose authored bytes are
    /// not is a different intent as well.
    #[test]
    fn the_identity_covers_each_documents_authored_provenance() {
        let (projection, _) = projection();
        let mut files = projection.text().files().to_vec();
        let first = &files[0];
        files[0] = TextProjectionFile::from_parts(
            first.target_path().clone(),
            first.blob_sha256(),
            first.source_path().clone(),
            Sha256::new([0xee; 32]),
        );
        let reauthored = DeliveryProjection::from_parts(
            projection.source_projection_sha256(),
            projection.snapshot_id(),
            projection.managed_root().clone(),
            TextProjection::from_parts(
                projection.snapshot_id(),
                projection.managed_root().clone(),
                projection.source_projection_sha256(),
                files,
            ),
            projection.assets().clone(),
        );

        assert_eq!(
            reauthored.text().projection_sha256(),
            projection.text().projection_sha256(),
            "the delivered tree is identical"
        );
        assert_ne!(
            reauthored.delivery_sha256(),
            projection.delivery_sha256(),
            "the reviewed inputs are not"
        );
    }

    /// The exact bytes the engine wrote before filename-bearing keys existed.
    ///
    /// This payload was produced by the version 1 encoder for [`projection`]. It
    /// is kept verbatim because it is the only real evidence that a durable row
    /// already in someone's database still loads: the keys it recorded have no
    /// filename segment, and recovery must reach those very objects.
    const FROZEN_VERSION_ONE_PAYLOAD: &str = concat!(
        "{\"version\":1,",
        "\"delivery_sha256\":\"9b111e21d1d7c1656440b27b2c23ffe883d0bc14d24970901de131ffe383be61\",",
        "\"source_projection_sha256\":\"e6a6a5a6a237f81d871a85086c3b4a445505ccd9429dce5fe0ecf26de5e6ebcd\",",
        "\"snapshot_id\":7,\"managed_root\":\"content\",",
        "\"text\":{",
        "\"source_projection_sha256\":\"e6a6a5a6a237f81d871a85086c3b4a445505ccd9429dce5fe0ecf26de5e6ebcd\",",
        "\"projection_sha256\":\"930770590736035e19b8c3152c097ad821d85c398a4267456be84a2166cee982\",",
        "\"snapshot_id\":7,\"managed_root\":\"content\",",
        "\"files\":[",
        "{\"target_path\":\"content/a.md\",\"blob_sha256\":\"9eb37c01176c0bfda0ee6ec0f0eba34324392d87e97d145b88a87a3fd38ac21c\",",
        "\"source_path\":\"a.md\",\"source_sha256\":\"3a660235b7d958fb2f3b9d50fba11ba5e1c2b9c0063d58503f315174d0a33f7f\"},",
        "{\"target_path\":\"content/notes/b.md\",\"blob_sha256\":\"9eb37c01176c0bfda0ee6ec0f0eba34324392d87e97d145b88a87a3fd38ac21c\",",
        "\"source_path\":\"notes/b.md\",\"source_sha256\":\"3a660235b7d958fb2f3b9d50fba11ba5e1c2b9c0063d58503f315174d0a33f7f\"}]},",
        "\"assets\":[",
        "{\"logical_path\":\"files/a.pdf\",",
        "\"source_sha256\":\"0303030303030303030303030303030303030303030303030303030303030303\",",
        "\"published_sha256\":\"0303030303030303030303030303030303030303030303030303030303030303\",",
        "\"published_size\":200,\"published_content_type\":\"application/pdf\",",
        "\"object_key\":\"assets/sha256/03/0303030303030303030303030303030303030303030303030303030303030303\",",
        "\"public_url\":\"https://assets.example.com/assets/sha256/03/0303030303030303030303030303030303030303030303030303030303030303\"},",
        "{\"logical_path\":\"img/a.png\",",
        "\"source_sha256\":\"0101010101010101010101010101010101010101010101010101010101010101\",",
        "\"published_sha256\":\"0202020202020202020202020202020202020202020202020202020202020202\",",
        "\"published_size\":4242,\"published_content_type\":\"image/jpeg\",",
        "\"object_key\":\"assets/sha256/02/0202020202020202020202020202020202020202020202020202020202020202\",",
        "\"public_url\":\"https://assets.example.com/assets/sha256/02/0202020202020202020202020202020202020202020202020202020202020202\"}]}",
    );

    /// §18.11/§18.13: a real V1 row still decodes, keeps the legacy objects it
    /// already published, and is never silently upgraded to the current scheme.
    #[test]
    fn a_frozen_version_one_payload_still_decodes_to_its_exact_legacy_intent() {
        let decoded = DeliveryProjectionWire::decode(FROZEN_VERSION_ONE_PAYLOAD).unwrap();

        assert_eq!(
            decoded.delivery_sha256().to_string(),
            "9b111e21d1d7c1656440b27b2c23ffe883d0bc14d24970901de131ffe383be61"
        );
        for asset in decoded.assets().assets() {
            assert!(
                asset.public_filename().is_none(),
                "a V1 asset froze no filename"
            );
            assert!(asset.object_key().is_legacy());
            let digest = asset.published_sha256().to_string();
            assert_eq!(
                asset.object_key().as_str(),
                format!("assets/sha256/{}/{digest}", &digest[..2])
            );
            assert!(
                asset
                    .public_url()
                    .as_str()
                    .ends_with(asset.object_key().as_str())
            );
        }

        // Re-encoding reproduces the identical payload: recovery reads history, it
        // does not rewrite it into today's scheme.
        assert_eq!(
            DeliveryProjectionWire::encode(&decoded),
            FROZEN_VERSION_ONE_PAYLOAD
        );
    }

    /// §18.14: a version 2 payload has to prove every presentation fact it states.
    #[test]
    fn a_version_two_payload_must_name_the_filename_its_own_facts_derive() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);
        let mut value: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        // The image asset is `img/a.png` re-encoded to `image/jpeg`, so its frozen
        // filename is `a.jpg`. Naming it `a.png` must not decode: that is a
        // different delivery intent, not a recoverable one.
        let mut renamed = value.clone();
        renamed["assets"][1]["public_filename"] = serde_json::json!("a.png");
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&renamed).unwrap()),
            Err(DeliveryProjectionWireError::PublicFilenameMismatch {
                logical_path: "img/a.png".to_owned()
            })
        );

        // The right filename under a key that does not carry it.
        let mut wrong_key = value.clone();
        wrong_key["assets"][1]["object_key"] = serde_json::json!(
            "assets/sha256/02/0202020202020202020202020202020202020202020202020202020202020202"
        );
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&wrong_key).unwrap()),
            Err(DeliveryProjectionWireError::ObjectKeyMismatch {
                logical_path: "img/a.png".to_owned()
            })
        );

        // The right hash under the wrong fan-out prefix.
        let mut wrong_prefix = value.clone();
        wrong_prefix["assets"][1]["object_key"] = serde_json::json!(
            "assets/sha256/aa/0202020202020202020202020202020202020202020202020202020202020202/a.jpg"
        );
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&wrong_prefix).unwrap()),
            Err(DeliveryProjectionWireError::ObjectKeyMismatch {
                logical_path: "img/a.png".to_owned()
            })
        );

        // An extra segment is not a filename.
        let mut extra = value.clone();
        extra["assets"][1]["object_key"] = serde_json::json!(
            "assets/sha256/02/0202020202020202020202020202020202020202020202020202020202020202/a.jpg/extra"
        );
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&extra).unwrap()),
            Err(DeliveryProjectionWireError::ObjectKeyMismatch {
                logical_path: "img/a.png".to_owned()
            })
        );

        // A URL whose filename disagrees with the key.
        let mut wrong_url = value.clone();
        let url = projection.assets().assets()[1]
            .public_url()
            .as_str()
            .to_owned();
        let key = projection.assets().assets()[1]
            .object_key()
            .as_str()
            .to_owned();
        let tampered = url.replace(&key, &key.replace("/a.jpg", "/b.jpg"));
        wrong_url["assets"][1]["public_url"] = serde_json::json!(tampered);
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&wrong_url).unwrap()),
            Err(DeliveryProjectionWireError::PublicUrlMismatch {
                logical_path: "img/a.png".to_owned()
            })
        );

        // A missing filename is not a version 2 payload at all.
        value["assets"][1]
            .as_object_mut()
            .unwrap()
            .remove("public_filename");
        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&value).unwrap()),
            Err(DeliveryProjectionWireError::Malformed)
        );
    }

    /// §18.14: a payload labelled version 1 may not carry version 2 facts.
    #[test]
    fn a_version_one_label_can_not_carry_a_filename_bearing_key() {
        let (projection, _) = projection();
        let encoded = DeliveryProjectionWire::encode(&projection);
        let mut value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        value["version"] = serde_json::json!(1);

        assert_eq!(
            DeliveryProjectionWire::decode(&serde_json::to_string(&value).unwrap()),
            Err(DeliveryProjectionWireError::Malformed)
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
