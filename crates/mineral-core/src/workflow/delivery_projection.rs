use std::{collections::BTreeSet, error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::{
    content::{
        AnalyzedMarkdown, AssetDependencyGraph, DependencyProblem, Reference, ReferenceKind,
        Resolution, is_navigation_warning,
    },
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId},
    ports::{BlobStore, ContentStoreError},
};

use super::{
    AssetContentType, AssetDeliveryConfig, AssetObjectKey, AssetPublicUrl, ManagedRoot,
    ProjectionEntryKind, ProjectionTargetPath, PublicProjection, PublicationFileMode,
};

/// One text publication file, carrying the bytes that must reach the target.
///
/// `blob_sha256` is the identity of the exact bytes Git must commit; it differs
/// from `source_sha256` exactly when a binary asset reference inside the document
/// was rewritten to its final delivery URL. The immutable Snapshot identity is
/// retained so the rewrite stays auditable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextProjectionFile {
    target_path: ProjectionTargetPath,
    blob_sha256: Sha256,
    source_path: ContentPath,
    source_sha256: Sha256,
}

impl TextProjectionFile {
    pub fn target_path(&self) -> &ProjectionTargetPath {
        &self.target_path
    }

    /// The exact bytes identity the Publisher must materialize.
    pub fn blob_sha256(&self) -> Sha256 {
        self.blob_sha256
    }

    pub fn file_mode(&self) -> PublicationFileMode {
        PublicationFileMode::Regular
    }

    /// The immutable Snapshot identity this file was derived from.
    pub fn source_path(&self) -> &ContentPath {
        &self.source_path
    }

    pub fn source_sha256(&self) -> Sha256 {
        self.source_sha256
    }

    /// Whether delivery rewrote this document at all.
    pub fn is_rewritten(&self) -> bool {
        self.blob_sha256 != self.source_sha256
    }

    pub(crate) fn from_parts(
        target_path: ProjectionTargetPath,
        blob_sha256: Sha256,
        source_path: ContentPath,
        source_sha256: Sha256,
    ) -> Self {
        Self {
            target_path,
            blob_sha256,
            source_path,
            source_sha256,
        }
    }

    #[doc(hidden)]
    pub fn from_parts_for_test(
        target_path: ProjectionTargetPath,
        blob_sha256: Sha256,
        source_path: ContentPath,
        source_sha256: Sha256,
    ) -> Self {
        Self::from_parts(target_path, blob_sha256, source_path, source_sha256)
    }
}

/// The delivery text side: the complete set of documents Git may commit.
///
/// Binary assets are structurally absent from this type, so a Git adapter cannot
/// stage one even by mistake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextProjection {
    snapshot_id: SnapshotId,
    managed_root: ManagedRoot,
    source_projection_sha256: Sha256,
    files: Vec<TextProjectionFile>,
    projection_sha256: Sha256,
}

impl TextProjection {
    /// Rebuilds a text projection from already-final files.
    ///
    /// This is the single definition of the text identity: the delivery builder
    /// and the durable codec both go through it, so a text identity always
    /// describes exactly the file set it was computed from.
    pub(crate) fn from_parts(
        snapshot_id: SnapshotId,
        managed_root: ManagedRoot,
        source_projection_sha256: Sha256,
        mut files: Vec<TextProjectionFile>,
    ) -> Self {
        files.sort_by(|left, right| left.target_path.cmp(&right.target_path));
        let projection_sha256 = text_projection_identity(&files);
        Self {
            snapshot_id,
            managed_root,
            source_projection_sha256,
            files,
            projection_sha256,
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }

    /// The logical public projection this text side was derived from.
    pub fn source_projection_sha256(&self) -> Sha256 {
        self.source_projection_sha256
    }

    pub fn files(&self) -> &[TextProjectionFile] {
        &self.files
    }

    /// Stable identity of the exact text tree, in canonical target-path order.
    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }

    /// Builds a text projection directly from already-final blob identities.
    ///
    /// Runtime-side/testing constructor: the delivery builder is the production
    /// path, and it is the only thing that decides which bytes are final.
    #[doc(hidden)]
    pub fn from_parts_for_test(
        snapshot_id: SnapshotId,
        managed_root: ManagedRoot,
        source_projection_sha256: Sha256,
        files: Vec<(ContentPath, Sha256)>,
    ) -> Self {
        let files = files
            .into_iter()
            .map(|(source_path, blob_sha256)| {
                let target_path = managed_root
                    .target_for(&source_path)
                    .expect("test text paths are canonical");
                TextProjectionFile {
                    target_path,
                    blob_sha256,
                    source_path: source_path.clone(),
                    source_sha256: blob_sha256,
                }
            })
            .collect::<Vec<_>>();
        Self::from_parts(snapshot_id, managed_root, source_projection_sha256, files)
    }
}

/// One logical asset that must exist in object storage under a deterministic
/// identity before the text side may become public.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedAsset {
    logical_path: ContentPath,
    source_sha256: Sha256,
    published_sha256: Sha256,
    published_size: u64,
    published_content_type: AssetContentType,
    object_key: AssetObjectKey,
    public_url: AssetPublicUrl,
}

impl PublishedAsset {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        logical_path: ContentPath,
        source_sha256: Sha256,
        published_sha256: Sha256,
        published_size: u64,
        published_content_type: AssetContentType,
        object_key: AssetObjectKey,
        public_url: AssetPublicUrl,
    ) -> Self {
        Self {
            logical_path,
            source_sha256,
            published_sha256,
            published_size,
            published_content_type,
            object_key,
            public_url,
        }
    }

    pub fn logical_path(&self) -> &ContentPath {
        &self.logical_path
    }

    /// The immutable Snapshot identity this asset was sanitized from.
    pub fn source_sha256(&self) -> Sha256 {
        self.source_sha256
    }

    /// Identity of the bytes that will actually be served, never the source bytes.
    pub fn published_sha256(&self) -> Sha256 {
        self.published_sha256
    }

    pub fn published_size(&self) -> u64 {
        self.published_size
    }

    /// Media type of the bytes that will actually be served.
    pub fn published_content_type(&self) -> &AssetContentType {
        &self.published_content_type
    }

    pub fn object_key(&self) -> &AssetObjectKey {
        &self.object_key
    }

    pub fn public_url(&self) -> &AssetPublicUrl {
        &self.public_url
    }

    #[doc(hidden)]
    pub fn from_parts_for_test(
        logical_path: ContentPath,
        source_sha256: Sha256,
        published_sha256: Sha256,
        published_size: u64,
        published_content_type: AssetContentType,
        config: &AssetDeliveryConfig,
    ) -> Self {
        let object_key = AssetObjectKey::for_published_sha256(&published_sha256);
        let public_url = config.public_url(&object_key);
        Self::from_parts(
            logical_path,
            source_sha256,
            published_sha256,
            published_size,
            published_content_type,
            object_key,
            public_url,
        )
    }
}

/// The delivery binary side: exactly the assets a public document references.
///
/// Every entry here is reachable from the text side, so the logical
/// `No document, no asset` invariant survives the delivery split.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetProjection {
    assets: Vec<PublishedAsset>,
}

impl AssetProjection {
    pub(crate) fn from_assets(mut assets: Vec<PublishedAsset>) -> Self {
        assets.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        Self { assets }
    }

    pub fn assets(&self) -> &[PublishedAsset] {
        &self.assets
    }

    pub fn len(&self) -> usize {
        self.assets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    pub fn get(&self, logical_path: &ContentPath) -> Option<&PublishedAsset> {
        self.assets
            .binary_search_by(|asset| asset.logical_path.cmp(logical_path))
            .ok()
            .map(|index| &self.assets[index])
    }
}

/// The complete delivery decision for one public projection.
///
/// It says both what may become public (the source projection), where every
/// referenced asset will physically live, and which exact document bytes Git may
/// commit once those assets exist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryProjection {
    source_projection_sha256: Sha256,
    snapshot_id: SnapshotId,
    managed_root: ManagedRoot,
    text: TextProjection,
    assets: AssetProjection,
    delivery_sha256: Sha256,
}

impl DeliveryProjection {
    /// Rebuilds a delivery projection from its immutable parts.
    ///
    /// The canonical delivery identity is computed here, so a decoded projection
    /// can never carry an identity that does not describe its own contents.
    pub(crate) fn from_parts(
        source_projection_sha256: Sha256,
        snapshot_id: SnapshotId,
        managed_root: ManagedRoot,
        text: TextProjection,
        assets: AssetProjection,
    ) -> Self {
        let delivery_sha256 = delivery_projection_identity(
            source_projection_sha256,
            text.projection_sha256(),
            &assets,
        );
        Self {
            source_projection_sha256,
            snapshot_id,
            managed_root,
            text,
            assets,
            delivery_sha256,
        }
    }

    pub fn source_projection_sha256(&self) -> Sha256 {
        self.source_projection_sha256
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }

    pub fn text(&self) -> &TextProjection {
        &self.text
    }

    pub fn assets(&self) -> &AssetProjection {
        &self.assets
    }

    /// Stable identity of the complete delivery intent.
    ///
    /// Later stages persist this value; it is derived only from canonical,
    /// ordered content and the frozen delivery configuration, so it is
    /// reproducible across processes, machines and runs.
    pub fn delivery_sha256(&self) -> Sha256 {
        self.delivery_sha256
    }
}

/// Immutable, content-addressed persistence for delivery projections.
///
/// The durable key *is* the projection's canonical identity, so a store can never
/// be asked to replace one delivery intent with another: the same content may be
/// saved repeatedly, while different content under an existing key is a conflict
/// the store must report rather than resolve.
///
/// A stored projection is the only thing a later execution is allowed to
/// rematerialize a reviewed tree from, so `get` must return the exact projection
/// whose canonical identity is `id` — or `None` when that identity was never
/// captured. Returning a different (or partially readable) projection would break
/// the `reviewed tree == committed tree` invariant, so it must fail closed.
pub trait DeliveryProjectionStore {
    type Error: Error + 'static;

    fn save(&self, projection: &DeliveryProjection) -> Result<(), Self::Error>;

    fn get(&self, id: Sha256) -> Result<Option<DeliveryProjection>, Self::Error>;
}

/// Pure builder for the delivery stage.
///
/// It reads document bytes through the [`BlobStore`] port and writes rewritten
/// documents back through the same content-addressed port. It has no network
/// client, no clock, and no configuration source of its own: the delivery
/// location is a frozen input, and the reference semantics it uses to decide what
/// to rewrite are exactly the ones the dependency analysis already used.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeliveryProjectionBuilder;

impl DeliveryProjectionBuilder {
    pub fn build<B: BlobStore>(
        projection: &PublicProjection,
        snapshot: &Snapshot,
        config: &AssetDeliveryConfig,
        blobs: &B,
    ) -> Result<DeliveryProjection, DeliveryProjectionError> {
        if projection.snapshot_id() != snapshot.id() {
            return Err(DeliveryProjectionError::SnapshotMismatch {
                projection_snapshot_id: projection.snapshot_id(),
                snapshot_id: snapshot.id(),
            });
        }

        let mut text_sources = BTreeSet::new();
        let mut text_entries = Vec::new();
        let mut asset_entries = Vec::new();
        let mut asset_sources = BTreeSet::new();
        for entry in projection.entries() {
            match entry.kind() {
                ProjectionEntryKind::Markdown => {
                    if !text_sources.insert(entry.source_path().clone()) {
                        return Err(DeliveryProjectionError::DuplicateTextSource(
                            entry.source_path().clone(),
                        ));
                    }
                    text_entries.push(entry);
                }
                ProjectionEntryKind::Asset => {
                    if !asset_sources.insert(entry.source_path().clone()) {
                        return Err(DeliveryProjectionError::DuplicateAssetSource(
                            entry.source_path().clone(),
                        ));
                    }
                    asset_entries.push(entry);
                }
            }
        }

        // Analyze with the very same parser and resolver the dependency analysis
        // uses, so the rewriter and the dependency scan can never disagree about
        // what a reference points at.
        let mut originals = Vec::with_capacity(text_entries.len());
        let mut documents = Vec::with_capacity(text_entries.len());
        for entry in &text_entries {
            let path = entry.source_path();
            let file = snapshot_file(snapshot, path)?;
            if file.sha256() != entry.blob_sha256() {
                return Err(DeliveryProjectionError::TextSourceIdentityMismatch {
                    path: path.clone(),
                    snapshot_sha256: file.sha256(),
                    projection_sha256: entry.blob_sha256(),
                });
            }
            let bytes =
                blobs
                    .read(file.sha256())
                    .map_err(|source| DeliveryProjectionError::BlobRead {
                        path: path.clone(),
                        source,
                    })?;
            let actual = Sha256::digest(&bytes);
            if actual != file.sha256() {
                return Err(DeliveryProjectionError::BlobIdentityMismatch {
                    path: path.clone(),
                    expected: file.sha256(),
                    actual,
                });
            }
            let markdown = String::from_utf8(bytes)
                .map_err(|_| DeliveryProjectionError::NotUtf8 { path: path.clone() })?;
            documents.push(AnalyzedMarkdown::analyze(file.clone(), &markdown, snapshot));
            originals.push(markdown);
        }

        let graph = AssetDependencyGraph::build(projection.snapshot_id(), &documents);
        if let Some(problem) = graph
            .problems()
            .iter()
            .find(|problem| !is_navigation_warning(problem))
        {
            return Err(DeliveryProjectionError::UnresolvedDependency(
                problem.clone(),
            ));
        }

        // The delivery asset set is recomputed from the final documents. An asset
        // entry the text side does not reference never enters the delivery.
        let mut referenced = BTreeSet::new();
        for dependency in graph.dependencies() {
            if !asset_sources.contains(dependency.asset_path()) {
                return Err(DeliveryProjectionError::ReferencedAssetNotInProjection {
                    document_path: dependency.document_path().clone(),
                    asset_path: dependency.asset_path().clone(),
                });
            }
            referenced.insert(dependency.asset_path().clone());
        }

        let published = asset_entries
            .iter()
            .filter(|entry| referenced.contains(entry.source_path()))
            .map(|entry| {
                // `PublicProjection` already recorded the sanitized publication
                // identity of an asset in `blob_sha256`; the source identity is
                // retained separately and is never used as delivery identity.
                let published_sha256 = entry.blob_sha256();
                let Some(facts) = entry.asset_publication() else {
                    return Err(DeliveryProjectionError::AssetPublicationFactsMissing {
                        logical_path: entry.source_path().clone(),
                    });
                };
                let object_key = AssetObjectKey::for_published_sha256(&published_sha256);
                let public_url = config.public_url(&object_key);
                Ok(PublishedAsset::from_parts(
                    entry.source_path().clone(),
                    entry.source_sha256(),
                    published_sha256,
                    facts.published_size(),
                    facts.published_content_type().clone(),
                    object_key,
                    public_url,
                ))
            })
            .collect::<Result<Vec<_>, DeliveryProjectionError>>()?;
        let assets = AssetProjection::from_assets(published);

        let mut files = Vec::with_capacity(text_entries.len());
        for ((entry, original), document) in text_entries
            .iter()
            .zip(originals.iter())
            .zip(documents.iter())
        {
            let rewritten = rewrite_document(document, original, &assets)?;
            let blob_sha256 = if rewritten == *original {
                // Nothing referenced a binary asset, so the reviewed bytes are
                // already the delivery bytes and no write is needed.
                entry.blob_sha256()
            } else {
                let expected = Sha256::digest(rewritten.as_bytes());
                let stored = blobs.store(rewritten.as_bytes()).map_err(|source| {
                    DeliveryProjectionError::BlobWrite {
                        path: entry.source_path().clone(),
                        source,
                    }
                })?;
                if stored != expected {
                    return Err(DeliveryProjectionError::StoredBlobIdentityMismatch {
                        path: entry.source_path().clone(),
                        expected,
                        actual: stored,
                    });
                }
                stored
            };
            files.push(TextProjectionFile::from_parts(
                entry.target_path().clone(),
                blob_sha256,
                entry.source_path().clone(),
                entry.source_sha256(),
            ));
        }

        let text = TextProjection::from_parts(
            projection.snapshot_id(),
            projection.managed_root().clone(),
            projection.projection_sha256(),
            files,
        );
        Ok(DeliveryProjection::from_parts(
            projection.projection_sha256(),
            projection.snapshot_id(),
            projection.managed_root().clone(),
            text,
            assets,
        ))
    }
}

fn snapshot_file<'a>(
    snapshot: &'a Snapshot,
    path: &ContentPath,
) -> Result<&'a SnapshotFile, DeliveryProjectionError> {
    snapshot
        .files()
        .binary_search_by(|file| file.path().cmp(path))
        .ok()
        .map(|index| &snapshot.files()[index])
        .ok_or_else(|| DeliveryProjectionError::TextSourceMissingFromSnapshot(path.clone()))
}

fn rewrite_document(
    document: &AnalyzedMarkdown,
    original: &str,
    assets: &AssetProjection,
) -> Result<String, DeliveryProjectionError> {
    let mut rewrites = Vec::new();
    for resolved in document.references() {
        let Resolution::ResolvedAsset { path } = resolved.resolution() else {
            continue;
        };
        let reference = resolved.reference();
        let Some(asset) = assets.get(path) else {
            return Err(DeliveryProjectionError::ReferencedAssetNotInProjection {
                document_path: document.path().clone(),
                asset_path: path.clone(),
            });
        };
        rewrites.push((
            reference.span(),
            rewrite_reference(
                document.path(),
                reference,
                original,
                asset.public_url().as_str(),
            )?,
        ));
    }

    if rewrites.is_empty() {
        return Ok(original.to_owned());
    }

    // The parser yields references in source order with disjoint spans, so the
    // document is rebuilt by copying the untouched bytes and substituting only
    // the exact reference syntax. No textual search-and-replace is involved.
    let mut rewritten = String::with_capacity(original.len());
    let mut cursor = 0;
    for (span, replacement) in rewrites {
        rewritten.push_str(&original[cursor..span.start]);
        rewritten.push_str(&replacement);
        cursor = span.end;
    }
    rewritten.push_str(&original[cursor..]);
    Ok(rewritten)
}

fn rewrite_reference(
    document_path: &ContentPath,
    reference: &Reference,
    original: &str,
    url: &str,
) -> Result<String, DeliveryProjectionError> {
    let text = &original[reference.span()];
    match reference.kind() {
        // Obsidian's embed syntax has no Markdown equivalent. Its `|` suffix is
        // ambiguous between a display size and alt text, so it is dropped rather
        // than guessed at; the final URL is what has to survive.
        ReferenceKind::WikiEmbed => Ok(format!("![]({url})")),
        ReferenceKind::MarkdownImage | ReferenceKind::MarkdownLink => {
            // The parser found the reference's first `](`; keeping everything up
            // to and including it preserves the authored label/alt text verbatim.
            let Some(separator) = text.find("](") else {
                return Err(DeliveryProjectionError::MalformedReferenceSpan {
                    document_path: document_path.clone(),
                    target: reference.target().to_owned(),
                });
            };
            Ok(format!("{}{url})", &text[..separator + 2]))
        }
        // The resolver never produces these combinations, and rewriting them
        // would either invent a delivery rule or grant publication permission a
        // document link never carried.
        ReferenceKind::WikiLink | ReferenceKind::ExternalUrl => {
            Err(DeliveryProjectionError::AssetReferenceKindNotRewritable {
                document_path: document_path.clone(),
                kind: reference.kind(),
                target: reference.target().to_owned(),
            })
        }
    }
}

fn text_projection_identity(files: &[TextProjectionFile]) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(b"mineral-publisher-text-projection-v1\0");
    for file in files {
        let path = file.target_path.as_str().as_bytes();
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path);
        hasher.update(file.blob_sha256.as_bytes());
    }
    Sha256::new(hasher.finalize().into())
}

fn delivery_projection_identity(
    source_projection_sha256: Sha256,
    text_projection_sha256: Sha256,
    assets: &AssetProjection,
) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(b"mineral-publisher-delivery-projection-v1\0");
    hasher.update(source_projection_sha256.as_bytes());
    hasher.update(text_projection_sha256.as_bytes());
    for asset in &assets.assets {
        let path = asset.logical_path.as_str().as_bytes();
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path);
        hasher.update(asset.source_sha256.as_bytes());
        hasher.update(asset.published_sha256.as_bytes());
        hasher.update(asset.published_size.to_be_bytes());
        let content_type = asset.published_content_type.as_str().as_bytes();
        hasher.update((content_type.len() as u64).to_be_bytes());
        hasher.update(content_type);
        let key = asset.object_key.as_str().as_bytes();
        hasher.update((key.len() as u64).to_be_bytes());
        hasher.update(key);
        let url = asset.public_url.as_str().as_bytes();
        hasher.update((url.len() as u64).to_be_bytes());
        hasher.update(url);
    }
    Sha256::new(hasher.finalize().into())
}

#[derive(Debug)]
pub enum DeliveryProjectionError {
    SnapshotMismatch {
        projection_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    DuplicateTextSource(ContentPath),
    DuplicateAssetSource(ContentPath),
    TextSourceMissingFromSnapshot(ContentPath),
    TextSourceIdentityMismatch {
        path: ContentPath,
        snapshot_sha256: Sha256,
        projection_sha256: Sha256,
    },
    BlobRead {
        path: ContentPath,
        source: ContentStoreError,
    },
    BlobIdentityMismatch {
        path: ContentPath,
        expected: Sha256,
        actual: Sha256,
    },
    NotUtf8 {
        path: ContentPath,
    },
    UnresolvedDependency(DependencyProblem),
    ReferencedAssetNotInProjection {
        document_path: ContentPath,
        asset_path: ContentPath,
    },
    /// The public projection carried an asset entry without the publication facts
    /// the sanitizer must have recorded for it.
    AssetPublicationFactsMissing {
        logical_path: ContentPath,
    },
    AssetReferenceKindNotRewritable {
        document_path: ContentPath,
        kind: ReferenceKind,
        target: String,
    },
    MalformedReferenceSpan {
        document_path: ContentPath,
        target: String,
    },
    BlobWrite {
        path: ContentPath,
        source: ContentStoreError,
    },
    StoredBlobIdentityMismatch {
        path: ContentPath,
        expected: Sha256,
        actual: Sha256,
    },
}

impl fmt::Display for DeliveryProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch { .. } => formatter
                .write_str("public projection and snapshot must share one snapshot identity"),
            Self::DuplicateTextSource(path) => {
                write!(formatter, "duplicate text projection source: {path}")
            }
            Self::DuplicateAssetSource(path) => {
                write!(formatter, "duplicate asset projection source: {path}")
            }
            Self::TextSourceMissingFromSnapshot(path) => {
                write!(
                    formatter,
                    "text source is missing from the snapshot: {path}"
                )
            }
            Self::TextSourceIdentityMismatch { path, .. } => write!(
                formatter,
                "text source identity does not match the snapshot blob: {path}"
            ),
            Self::BlobRead { path, .. } => {
                write!(formatter, "could not read document bytes: {path}")
            }
            Self::BlobIdentityMismatch { path, .. } => write!(
                formatter,
                "content store returned different bytes than the requested identity: {path}"
            ),
            Self::NotUtf8 { path } => {
                write!(formatter, "public document is not valid UTF-8: {path}")
            }
            Self::UnresolvedDependency(problem) => write!(
                formatter,
                "public document {} has an unresolved local reference: {}",
                problem.document_path(),
                problem.origin().target()
            ),
            Self::ReferencedAssetNotInProjection {
                document_path,
                asset_path,
            } => write!(
                formatter,
                "document {document_path} references {asset_path}, which the public projection does not contain"
            ),
            Self::AssetPublicationFactsMissing { logical_path } => write!(
                formatter,
                "asset entry carries no publication facts: {logical_path}"
            ),
            Self::AssetReferenceKindNotRewritable {
                document_path,
                kind,
                target,
            } => write!(
                formatter,
                "document {document_path} resolves {kind:?} {target} to an asset with no delivery rewrite rule"
            ),
            Self::MalformedReferenceSpan {
                document_path,
                target,
            } => write!(
                formatter,
                "document {document_path} contains a reference with an unusable source span: {target}"
            ),
            Self::BlobWrite { path, .. } => {
                write!(
                    formatter,
                    "could not store rewritten document bytes: {path}"
                )
            }
            Self::StoredBlobIdentityMismatch { path, .. } => write!(
                formatter,
                "content store returned a wrong identity for rewritten document bytes: {path}"
            ),
        }
    }
}

impl Error for DeliveryProjectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BlobRead { source, .. } | Self::BlobWrite { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, time::SystemTime};

    use crate::{
        domain::{SnapshotFile, SourceId, TimestampMillis},
        ports::ContentStoreError,
        publication::git::{
            GitCommitOid, GitCommitSpec, GitCurrentTarget, GitPublicationPrepareError,
            GitPublicationPrepareRequest, GitPublicationPreparer, GitRefTarget, GitRepository,
            LocalCommitState, ReviewedGitTree,
        },
        publish::{PublishRunId, PublishTargetId, RepositoryLocator},
        workflow::{
            AssetReviewRunId, CurrentTargetEntry, CurrentTargetState, FinalPublicationSet,
            SanitizationTransformation, SanitizedAsset,
        },
    };

    use super::*;

    const BASE: char = 'a';
    const TREE: char = '9';

    fn oid(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn digest(value: u8) -> Sha256 {
        Sha256::new([value; 32])
    }

    fn config() -> AssetDeliveryConfig {
        AssetDeliveryConfig::new("https://assets.example.com").unwrap()
    }

    #[derive(Default)]
    struct MemoryStore {
        blobs: RefCell<HashMap<Sha256, Vec<u8>>>,
    }

    impl MemoryStore {
        fn insert(&self, bytes: &[u8]) -> Sha256 {
            let identity = Sha256::digest(bytes);
            self.blobs.borrow_mut().insert(identity, bytes.to_vec());
            identity
        }
    }

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.blobs
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            Ok(self.insert(content))
        }
    }

    /// A store that answers every read with bytes that do not match the identity
    /// it was asked for.
    struct WrongReadStore;

    impl BlobStore for WrongReadStore {
        fn read(&self, _: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            Ok(b"unexpected".to_vec())
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            Ok(Sha256::digest(content))
        }
    }

    /// A store that reads correctly but reports a wrong identity for a write.
    struct WrongWriteStore<'a>(&'a MemoryStore);

    impl BlobStore for WrongWriteStore<'_> {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0.read(identity)
        }

        fn store(&self, _: &[u8]) -> Result<Sha256, ContentStoreError> {
            Ok(digest(0))
        }
    }

    fn asset(name: &str, source: Sha256, published: Sha256) -> SanitizedAsset {
        asset_with(name, source, published, 1, "image/png")
    }

    fn asset_with(
        name: &str,
        source: Sha256,
        published: Sha256,
        published_size: u64,
        media_type: &str,
    ) -> SanitizedAsset {
        SanitizedAsset::from_parts(
            path(name),
            AssetReviewRunId::new(1).unwrap(),
            source,
            published,
            published_size,
            AssetContentType::new(media_type).unwrap(),
            if source == published {
                vec![SanitizationTransformation::Identity]
            } else {
                vec![SanitizationTransformation::StripMetadata]
            },
        )
    }

    /// A Snapshot, its final logical publication set, and the content store that
    /// holds the immutable document bytes.
    struct Fixture {
        store: MemoryStore,
        snapshot: Snapshot,
        set: FinalPublicationSet,
        root: ManagedRoot,
    }

    impl Fixture {
        fn new(documents: &[(&str, &str)], assets: &[(&str, Sha256, Sha256)]) -> Self {
            Self::with_publication(
                documents,
                &assets
                    .iter()
                    .map(|(name, source, published)| (*name, *source, *published, 1, "image/png"))
                    .collect::<Vec<_>>(),
            )
        }

        /// A fixture whose assets carry explicit published size and media type.
        fn with_publication(
            documents: &[(&str, &str)],
            assets: &[(&str, Sha256, Sha256, u64, &str)],
        ) -> Self {
            let store = MemoryStore::default();
            let mut files = Vec::new();
            for (name, markdown) in documents {
                let identity = store.insert(markdown.as_bytes());
                files.push(SnapshotFile::new(
                    path(name),
                    markdown.len() as u64,
                    identity,
                    None,
                ));
            }
            for (name, source, _, size, _) in assets {
                files.push(SnapshotFile::new(path(name), *size, *source, None));
            }
            let snapshot = Snapshot::new(
                SnapshotId::new(1).unwrap(),
                SystemTime::UNIX_EPOCH,
                SourceId::new("test").unwrap(),
                files,
            )
            .unwrap();
            let set = FinalPublicationSet::from_parts_for_test(
                snapshot.id(),
                documents.iter().map(|(name, _)| path(name)).collect(),
                assets
                    .iter()
                    .map(|(name, source, published, size, media_type)| {
                        asset_with(name, *source, *published, *size, media_type)
                    })
                    .collect(),
            );
            Self {
                store,
                snapshot,
                set,
                root: ManagedRoot::repository_root(),
            }
        }

        fn with_root(mut self, root: ManagedRoot) -> Self {
            self.root = root;
            self
        }

        fn public(&self) -> PublicProjection {
            PublicProjection::build(&self.set, &self.snapshot, self.root.clone()).unwrap()
        }

        fn build(&self) -> DeliveryProjection {
            self.try_build().unwrap()
        }

        fn try_build(&self) -> Result<DeliveryProjection, DeliveryProjectionError> {
            self.build_with(&config())
        }

        fn build_with(
            &self,
            config: &AssetDeliveryConfig,
        ) -> Result<DeliveryProjection, DeliveryProjectionError> {
            DeliveryProjectionBuilder::build(&self.public(), &self.snapshot, config, &self.store)
        }

        fn text(&self, name: &str) -> String {
            let file = self
                .snapshot
                .files()
                .iter()
                .find(|file| file.path() == &path(name))
                .expect("fixture document exists");
            String::from_utf8(self.store.read(file.sha256()).unwrap()).unwrap()
        }

        /// What one delivery text file actually materializes to.
        fn delivered(&self, delivery: &DeliveryProjection, name: &str) -> String {
            let file = delivery
                .text()
                .files()
                .iter()
                .find(|file| file.source_path() == &path(name))
                .expect("delivered document exists");
            String::from_utf8(self.store.read(file.blob_sha256()).unwrap()).unwrap()
        }
    }

    fn expected_url(published: Sha256) -> String {
        config()
            .public_url(&AssetObjectKey::for_published_sha256(&published))
            .as_str()
            .to_owned()
    }

    fn mixed() -> Fixture {
        Fixture::new(
            &[
                ("a.md", "# A\n\n![[img/a.png]]\n"),
                ("notes/b.md", "# B\n\n[manual](../files/a.pdf)\n"),
            ],
            &[
                ("img/a.png", digest(1), digest(2)),
                ("files/a.pdf", digest(3), digest(4)),
            ],
        )
    }

    // 1. mixed projection
    #[test]
    fn mixed_projection_splits_text_from_assets_and_keeps_binaries_out_of_git() {
        let delivery = mixed().build();

        let text = delivery
            .text()
            .files()
            .iter()
            .map(|file| file.target_path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(text, ["a.md", "notes/b.md"]);
        assert!(text.iter().all(|path| !path.ends_with(".png")));
        assert!(text.iter().all(|path| !path.ends_with(".pdf")));

        let assets = delivery
            .assets()
            .assets()
            .iter()
            .map(|asset| asset.logical_path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(assets, ["files/a.pdf", "img/a.png"]);
    }

    // 2. wikilink image rewrite
    #[test]
    fn wiki_embed_of_an_asset_becomes_its_final_https_url() {
        let fixture = mixed();
        let delivery = fixture.build();

        let rewritten = fixture.delivered(&delivery, "a.md");

        assert_eq!(
            rewritten,
            format!("# A\n\n![]({})\n", expected_url(digest(2)))
        );
        assert!(!rewritten.contains("![["));
        assert!(rewritten.starts_with("# A\n"));
        assert_eq!(
            delivery.text().files()[0].blob_sha256(),
            Sha256::digest(rewritten.as_bytes())
        );
    }

    // 3. Markdown image rewrite through the existing resolver
    #[test]
    fn markdown_image_and_wiki_embed_resolve_to_the_same_asset_url() {
        let fixture = Fixture::new(
            &[
                ("notes/b.md", "# B\n\n![](../img/a.png)\n"),
                ("a.md", "![[img/a.png]]\n"),
            ],
            &[("img/a.png", digest(1), digest(2))],
        );
        let delivery = fixture.build();

        let url = expected_url(digest(2));
        assert_eq!(
            fixture.delivered(&delivery, "notes/b.md"),
            format!("# B\n\n![]({url})\n")
        );
        assert_eq!(
            fixture.delivered(&delivery, "a.md"),
            format!("![]({url})\n")
        );
        assert_eq!(delivery.assets().len(), 1);
    }

    // 4. binary download link
    #[test]
    fn markdown_download_link_keeps_its_label_and_gets_the_asset_url() {
        let fixture = mixed();
        let delivery = fixture.build();

        assert_eq!(
            fixture.delivered(&delivery, "notes/b.md"),
            format!("# B\n\n[manual]({})\n", expected_url(digest(4)))
        );
    }

    // 5. document link untouched
    #[test]
    fn document_links_are_never_rewritten_to_asset_urls() {
        let fixture = Fixture::new(&[("a.md", "see [[b]] and [b](./b.md) and ![[b]]\n")], &[]);
        // `b.md` exists in the Snapshot but is not published: a note link must not
        // become an asset URL, and it must not grant publication permission.
        let mut files = fixture.snapshot.files().to_vec();
        files.push(SnapshotFile::new(path("b.md"), 1, digest(5), None));
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files,
        )
        .unwrap();
        let projection =
            PublicProjection::build(&fixture.set, &snapshot, ManagedRoot::repository_root())
                .unwrap();

        let delivery =
            DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &fixture.store)
                .unwrap();

        assert_eq!(
            fixture.delivered(&delivery, "a.md"),
            "see [[b]] and [b](./b.md) and ![[b]]\n"
        );
        assert!(delivery.assets().is_empty());
        let file = &delivery.text().files()[0];
        assert!(!file.is_rewritten());
        assert_eq!(file.blob_sha256(), file.source_sha256());
    }

    // 6. unreferenced asset
    #[test]
    fn an_asset_no_public_document_references_never_enters_the_asset_projection() {
        let fixture = Fixture::new(
            &[("a.md", "![[img/a.png]]\n")],
            &[
                ("img/a.png", digest(1), digest(2)),
                ("orphan.png", digest(6), digest(7)),
            ],
        );

        let delivery = fixture.build();

        assert_eq!(delivery.assets().len(), 1);
        assert_eq!(
            delivery.assets().assets()[0].logical_path().as_str(),
            "img/a.png"
        );
        assert!(delivery.assets().get(&path("orphan.png")).is_none());
    }

    // 12. existing No-document-no-asset invariant
    #[test]
    fn every_delivered_asset_is_reachable_from_a_delivered_document() {
        let fixture = Fixture::new(
            &[("a.md", "![[img/a.png]]\n"), ("b.md", "no assets here\n")],
            &[
                ("img/a.png", digest(1), digest(2)),
                ("files/a.pdf", digest(3), digest(4)),
            ],
        );
        let delivery = fixture.build();

        for asset in delivery.assets().assets() {
            let referenced = [
                fixture.delivered(&delivery, "a.md"),
                fixture.delivered(&delivery, "b.md"),
            ]
            .iter()
            .any(|document| document.contains(asset.public_url().as_str()));
            assert!(
                referenced,
                "delivered asset {} is referenced by no delivered document",
                asset.logical_path()
            );
        }
    }

    #[test]
    fn an_asset_only_a_private_document_references_is_excluded() {
        let public = "nothing here\n";
        let store = MemoryStore::default();
        let public_blob = store.insert(public.as_bytes());
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![
                SnapshotFile::new(path("public.md"), public.len() as u64, public_blob, None),
                SnapshotFile::new(path("secret.png"), 1, digest(1), None),
            ],
        )
        .unwrap();
        // The private document never enters the public projection, but its sanitized
        // asset exists and must still not be delivered.
        let projection = PublicProjection::build(
            &FinalPublicationSet::from_parts_for_test(
                SnapshotId::new(1).unwrap(),
                vec![path("public.md")],
                vec![asset("secret.png", digest(1), digest(2))],
            ),
            &snapshot,
            ManagedRoot::repository_root(),
        )
        .unwrap();

        let delivery =
            DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &store).unwrap();

        assert!(delivery.assets().is_empty());
    }

    // 7. deterministic object identity
    #[test]
    fn identical_bytes_and_config_produce_identical_identity_regardless_of_input_order() {
        let first = Fixture::new(
            &[
                ("a.md", "![[img/a.png]]\n![[files/a.pdf]]\n"),
                ("z.md", "plain\n"),
            ],
            &[
                ("img/a.png", digest(1), digest(2)),
                ("files/a.pdf", digest(3), digest(4)),
            ],
        );
        let second = Fixture::new(
            &[
                ("z.md", "plain\n"),
                ("a.md", "![[img/a.png]]\n![[files/a.pdf]]\n"),
            ],
            &[
                ("files/a.pdf", digest(3), digest(4)),
                ("img/a.png", digest(1), digest(2)),
            ],
        );

        let first = first.build();
        let second = second.build();

        assert_eq!(first, second);
        assert_eq!(first.delivery_sha256(), second.delivery_sha256());
        assert_eq!(
            first.text().projection_sha256(),
            second.text().projection_sha256()
        );
        for (left, right) in first.assets().assets().iter().zip(second.assets().assets()) {
            assert_eq!(left.object_key(), right.object_key());
            assert_eq!(left.public_url(), right.public_url());
        }
    }

    #[test]
    fn one_physical_object_serves_every_logical_asset_with_identical_published_bytes() {
        let fixture = Fixture::new(
            &[("a.md", "![[one.png]]\n![[two.png]]\n")],
            &[
                ("one.png", digest(1), digest(9)),
                ("two.png", digest(2), digest(9)),
            ],
        );

        let delivery = fixture.build();

        assert_eq!(delivery.assets().len(), 2);
        assert_eq!(
            delivery.assets().assets()[0].object_key(),
            delivery.assets().assets()[1].object_key()
        );
        assert_eq!(
            delivery.assets().assets()[0].public_url(),
            delivery.assets().assets()[1].public_url()
        );
    }

    #[test]
    fn changing_the_delivery_config_changes_identity_but_not_the_object_key() {
        let fixture = mixed();
        let first = fixture.build_with(&config()).unwrap();
        let second = fixture
            .build_with(&AssetDeliveryConfig::new("https://cdn.example.com/mineral").unwrap())
            .unwrap();

        assert_eq!(
            first.assets().assets()[0].object_key(),
            second.assets().assets()[0].object_key()
        );
        assert_ne!(
            first.assets().assets()[0].public_url(),
            second.assets().assets()[0].public_url()
        );
        assert_ne!(first.delivery_sha256(), second.delivery_sha256());
        // The rewritten document carries the URL, so the text identity changes with
        // the delivery location as well.
        assert_ne!(
            first.text().projection_sha256(),
            second.text().projection_sha256()
        );
    }

    // 2. the publication facts the delivery intent must freeze
    #[test]
    fn delivery_freezes_the_published_size_and_content_type() {
        let fixture = Fixture::with_publication(
            &[("a.md", "![[img/photo.png]]\n")],
            &[("img/photo.png", digest(7), digest(8), 4321, "image/jpeg")],
        );

        let delivery = fixture.build();
        let asset = &delivery.assets().assets()[0];

        assert_eq!(asset.logical_path().as_str(), "img/photo.png");
        assert_eq!(asset.source_sha256(), digest(7));
        assert_eq!(asset.published_sha256(), digest(8));
        assert_eq!(asset.published_size(), 4321);
        assert_eq!(asset.published_content_type().as_str(), "image/jpeg");
    }

    #[test]
    fn the_delivery_identity_covers_the_publication_facts() {
        let documents = [("a.md", "![[img/a.png]]\n")];
        let base = Fixture::with_publication(
            &documents,
            &[("img/a.png", digest(1), digest(2), 10, "image/png")],
        )
        .build();
        let other_size = Fixture::with_publication(
            &documents,
            &[("img/a.png", digest(1), digest(2), 11, "image/png")],
        )
        .build();
        let other_type = Fixture::with_publication(
            &documents,
            &[("img/a.png", digest(1), digest(2), 10, "image/jpeg")],
        )
        .build();

        assert_ne!(base.delivery_sha256(), other_size.delivery_sha256());
        assert_ne!(base.delivery_sha256(), other_type.delivery_sha256());
        // The document bytes only depend on the published blob identity and the
        // URL, so all three agree on the text side.
        assert_eq!(
            base.text().projection_sha256(),
            other_type.text().projection_sha256()
        );
    }

    // 8. source hash vs published hash
    #[test]
    fn delivery_identity_uses_the_published_bytes_and_never_the_source_bytes() {
        let fixture = Fixture::new(
            &[("a.md", "![[img/a.png]]\n")],
            &[("img/a.png", digest(7), digest(8))],
        );

        let delivery = fixture.build();
        let asset = &delivery.assets().assets()[0];

        assert_ne!(digest(7), digest(8), "fixture must change the bytes");
        assert_eq!(asset.published_sha256(), digest(8));
        assert_eq!(
            asset.object_key(),
            &AssetObjectKey::for_published_sha256(&digest(8))
        );
        assert!(asset.public_url().as_str().contains(&digest(8).to_string()));
        assert!(!asset.public_url().as_str().contains(&digest(7).to_string()));
    }

    #[test]
    fn rewrites_are_visible_as_a_distinct_text_identity() {
        let fixture = mixed();
        let delivery = fixture.build();

        assert_ne!(
            delivery.source_projection_sha256(),
            delivery.text().projection_sha256()
        );
        let a = &delivery.text().files()[0];
        assert_eq!(a.source_path().as_str(), "a.md");
        assert!(a.is_rewritten());
        assert_ne!(a.blob_sha256(), a.source_sha256());
        // The immutable Snapshot identity is never touched by delivery.
        assert_eq!(
            fixture.text("a.md"),
            "# A\n\n![[img/a.png]]\n",
            "the source snapshot content stays auditable"
        );
    }

    // 9. multiple references
    #[test]
    fn two_documents_sharing_one_asset_get_one_identity_and_the_same_url() {
        let fixture = Fixture::new(
            &[
                ("a.md", "![[img/a.png]]\n"),
                ("notes/b.md", "![x](../img/a.png)\n"),
            ],
            &[("img/a.png", digest(1), digest(2))],
        );

        let delivery = fixture.build();
        let url = expected_url(digest(2));

        assert_eq!(delivery.assets().len(), 1);
        assert_eq!(
            fixture.delivered(&delivery, "a.md"),
            format!("![]({url})\n")
        );
        assert_eq!(
            fixture.delivered(&delivery, "notes/b.md"),
            format!("![x]({url})\n")
        );
        assert_eq!(delivery.assets().len(), 1, "one physical object, one entry");
    }

    #[test]
    fn repeated_references_in_one_document_are_all_rewritten() {
        let fixture = Fixture::new(
            &[("a.md", "![[img/a.png]] then ![alt](img/a.png)\n")],
            &[("img/a.png", digest(1), digest(2))],
        );

        let delivery = fixture.build();

        assert_eq!(
            fixture.delivered(&delivery, "a.md"),
            format!(
                "![]({url}) then ![alt]({url})\n",
                url = expected_url(digest(2))
            )
        );
    }

    // 10. false positive protection
    #[test]
    fn bare_path_text_in_prose_and_code_is_never_rewritten() {
        let markdown = "prose mentions img/a.png but is not a reference\n\n```text\n![[img/a.png]]\n```\n\ninline `![[img/a.png]]` too\n";
        let fixture = Fixture::new(
            &[("a.md", markdown)],
            &[("img/a.png", digest(1), digest(2))],
        );

        let delivery = fixture.build();

        assert_eq!(fixture.delivered(&delivery, "a.md"), markdown);
        assert!(!delivery.text().files()[0].is_rewritten());
        assert!(delivery.assets().is_empty());
    }

    #[test]
    fn a_managed_root_prefixes_target_paths_without_moving_asset_identity() {
        let fixture = Fixture::new(
            &[("a.md", "![[img/a.png]]\n")],
            &[("img/a.png", digest(1), digest(2))],
        )
        .with_root(ManagedRoot::new("public").unwrap());

        let delivery = fixture.build();

        assert_eq!(
            delivery.text().files()[0].target_path().as_str(),
            "public/a.md"
        );
        assert_eq!(delivery.text().files()[0].source_path().as_str(), "a.md");
        assert_eq!(
            delivery.assets().assets()[0].logical_path().as_str(),
            "img/a.png"
        );
        assert_eq!(
            delivery.assets().assets()[0].object_key(),
            &AssetObjectKey::for_published_sha256(&digest(2))
        );
    }

    // fail-closed paths
    #[test]
    fn snapshot_mismatch_fails_closed() {
        let fixture = mixed();
        let other = Snapshot::new(
            SnapshotId::new(2).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            fixture.snapshot.files().to_vec(),
        )
        .unwrap();

        assert!(matches!(
            DeliveryProjectionBuilder::build(
                &fixture.public(),
                &other,
                &config(),
                &fixture.store
            ),
            Err(DeliveryProjectionError::SnapshotMismatch {
                projection_snapshot_id,
                snapshot_id,
            }) if projection_snapshot_id == SnapshotId::new(1).unwrap()
                && snapshot_id == SnapshotId::new(2).unwrap()
        ));
    }

    #[test]
    fn a_referenced_asset_the_projection_does_not_contain_fails_closed() {
        // The asset exists in the Snapshot and resolves, but the public projection
        // never admitted it: publishing the document without it would break the
        // required-resource invariant, so delivery refuses.
        let markdown = "![[img/not-approved.png]]\n";
        let store = MemoryStore::default();
        let document = store.insert(markdown.as_bytes());
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![
                SnapshotFile::new(path("a.md"), markdown.len() as u64, document, None),
                SnapshotFile::new(path("img/not-approved.png"), 1, digest(1), None),
            ],
        )
        .unwrap();
        let projection = PublicProjection::build(
            &FinalPublicationSet::from_parts_for_test(
                SnapshotId::new(1).unwrap(),
                vec![path("a.md")],
                vec![],
            ),
            &snapshot,
            ManagedRoot::repository_root(),
        )
        .unwrap();

        assert!(matches!(
            DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &store),
            Err(DeliveryProjectionError::ReferencedAssetNotInProjection {
                asset_path,
                ..
            }) if asset_path == path("img/not-approved.png")
        ));
    }

    #[test]
    fn an_unresolved_local_reference_fails_closed() {
        let fixture = Fixture::new(&[("a.md", "![[missing.png]]\n")], &[]);

        assert!(matches!(
            fixture.try_build(),
            Err(DeliveryProjectionError::UnresolvedDependency(problem))
                if problem.document_path() == &path("a.md")
        ));
    }

    #[test]
    fn a_navigation_warning_does_not_block_delivery() {
        let fixture = Fixture::new(&[("a.md", "see [[not-created-yet]]\n")], &[]);

        let delivery = fixture.build();

        assert_eq!(
            fixture.delivered(&delivery, "a.md"),
            "see [[not-created-yet]]\n"
        );
    }

    #[test]
    fn a_non_utf8_public_document_fails_closed() {
        let bytes = [0xff_u8, 0xfe, 0x00];
        let store = MemoryStore::default();
        let identity = store.insert(&bytes);
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![SnapshotFile::new(path("a.md"), 3, identity, None)],
        )
        .unwrap();
        let projection = PublicProjection::build(
            &FinalPublicationSet::from_parts_for_test(
                SnapshotId::new(1).unwrap(),
                vec![path("a.md")],
                vec![],
            ),
            &snapshot,
            ManagedRoot::repository_root(),
        )
        .unwrap();

        assert!(matches!(
            DeliveryProjectionBuilder::build(&projection, &snapshot, &config(), &store),
            Err(DeliveryProjectionError::NotUtf8 { path: problem }) if problem == path("a.md")
        ));
    }

    #[test]
    fn a_content_store_that_returns_wrong_bytes_fails_closed() {
        let fixture = mixed();

        assert!(matches!(
            DeliveryProjectionBuilder::build(
                &fixture.public(),
                &fixture.snapshot,
                &config(),
                &WrongReadStore
            ),
            Err(DeliveryProjectionError::BlobIdentityMismatch { .. })
        ));
    }

    #[test]
    fn a_content_store_that_reports_a_wrong_written_identity_fails_closed() {
        let fixture = mixed();

        assert!(matches!(
            DeliveryProjectionBuilder::build(
                &fixture.public(),
                &fixture.snapshot,
                &config(),
                &WrongWriteStore(&fixture.store)
            ),
            Err(DeliveryProjectionError::StoredBlobIdentityMismatch { .. })
        ));
    }

    #[test]
    fn a_document_blob_missing_from_the_content_store_fails_closed() {
        let fixture = mixed();
        let lost = Sha256::digest(b"# A\n\n![[img/a.png]]\n");
        fixture.store.blobs.borrow_mut().remove(&lost);

        assert!(matches!(
            fixture.try_build(),
            Err(DeliveryProjectionError::BlobRead {
                source: ContentStoreError::Missing(_),
                ..
            })
        ));
    }

    // 11. Git integration through the port the engine actually uses
    struct RecordingRepository {
        state: CurrentTargetState,
        recorded: RefCell<Option<TextProjection>>,
    }

    impl GitRepository for RecordingRepository {
        type Error = std::convert::Infallible;

        fn read_current(
            &self,
            _: &GitCommitOid,
            _: &ManagedRoot,
        ) -> Result<GitCurrentTarget, Self::Error> {
            Ok(GitCurrentTarget::from_parts(oid(BASE), self.state.clone()))
        }

        fn materialize(
            &self,
            _: &GitCommitOid,
            text: &TextProjection,
        ) -> Result<ReviewedGitTree, Self::Error> {
            *self.recorded.borrow_mut() = Some(text.clone());
            Ok(ReviewedGitTree::from_parts(
                oid(BASE),
                oid(TREE),
                text.projection_sha256(),
                text.snapshot_id(),
                oid(TREE),
                text.managed_root().clone(),
            ))
        }

        fn create_commit(&self, _: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
            unreachable!("a Noop preparation never creates a commit")
        }

        fn inspect_commit(&self, _: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
            unreachable!("preparation never inspects a commit object")
        }
    }

    fn prepare_request<'a>(
        text: &'a TextProjection,
        base: &'a GitCommitOid,
        target_id: &'a PublishTargetId,
        repository: &'a RepositoryLocator,
        target: &'a GitRefTarget,
    ) -> GitPublicationPrepareRequest<'a> {
        GitPublicationPrepareRequest {
            id: PublishRunId::new(1).unwrap(),
            target_id,
            repository,
            target,
            text_projection: text,
            delivery: crate::publish::DeliveryProjectionBinding::new(
                Sha256::new([3; 32]),
                text.projection_sha256(),
            ),
            observed_base: base,
            author_name: "Mineral Publisher",
            author_email: "publisher@example.invalid",
            message: "Publish Mineral content",
            created_at: TimestampMillis::from_unix_millis(1_000),
        }
    }

    #[test]
    fn git_preparation_receives_only_rewritten_text_and_can_never_stage_a_binary() {
        let fixture = mixed();
        let delivery = fixture.build();
        // The current managed subtree already holds exactly the delivered text
        // state, so this preparation is a Noop but still materializes.
        let state = CurrentTargetState::new(
            delivery.text().managed_root().clone(),
            delivery
                .text()
                .files()
                .iter()
                .map(|file| CurrentTargetEntry::new(file.target_path().clone(), file.blob_sha256()))
                .collect(),
        )
        .unwrap();
        let repository = RecordingRepository {
            state,
            recorded: RefCell::new(None),
        };
        let base = GitCommitOid::new(oid(BASE)).unwrap();
        let target_id = PublishTargetId::new("origin:refs/heads/main").unwrap();
        let locator = RepositoryLocator::new("/srv/public-repo").unwrap();
        let target = GitRefTarget::new("origin", "refs/heads/main").unwrap();

        let preparation = GitPublicationPreparer::prepare(
            &repository,
            &prepare_request(delivery.text(), &base, &target_id, &locator, &target),
        )
        .unwrap();
        assert!(
            preparation.publish_run().publication() == crate::publish::PublishRunPublication::Noop
        );

        let recorded = repository
            .recorded
            .borrow()
            .clone()
            .expect("preparation materializes the text side");
        assert_eq!(&recorded, delivery.text());
        assert!(
            recorded
                .files()
                .iter()
                .all(|file| file.target_path().as_str().ends_with(".md")),
            "no binary may reach the Git port"
        );
        // The exact bytes the Git port was asked for are the rewritten ones.
        assert_eq!(
            fixture.delivered(&delivery, "a.md"),
            format!("# A\n\n![]({})\n", expected_url(digest(2)))
        );
        for file in recorded.files() {
            assert_eq!(
                Sha256::digest(&fixture.store.read(file.blob_sha256()).unwrap()),
                file.blob_sha256()
            );
        }
    }

    #[test]
    fn a_prepare_failure_is_reported_through_the_port_error_type() {
        let fixture = mixed();
        let delivery = fixture.build();
        let repository = RecordingRepository {
            state: CurrentTargetState::new(delivery.text().managed_root().clone(), vec![]).unwrap(),
            recorded: RefCell::new(None),
        };
        let base = GitCommitOid::new(oid(BASE)).unwrap();
        let target_id = PublishTargetId::new("origin:refs/heads/main").unwrap();
        let locator = RepositoryLocator::new("/srv/public-repo").unwrap();
        let target = GitRefTarget::new("origin", "refs/heads/main").unwrap();

        // An empty observed subtree against a non-empty delivery is a real change,
        // so a Noop reviewed tree no longer agrees with the plan and prepare must
        // refuse rather than publish the wrong thing.
        let result: Result<_, GitPublicationPrepareError<std::convert::Infallible>> =
            GitPublicationPreparer::prepare(
                &repository,
                &prepare_request(delivery.text(), &base, &target_id, &locator, &target),
            );

        assert_eq!(
            result.unwrap_err(),
            GitPublicationPrepareError::PlanReviewedTreeMismatch
        );
    }
}
