use crate::domain::{ContentPath, Sha256, SnapshotId};

use super::{AssetContentType, AssetReviewRunId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageSanitizationFormat {
    Jpeg,
    Png,
}

impl ImageSanitizationFormat {
    /// The media type of the bytes this format produces.
    ///
    /// One definition of "what a re-encoded image actually is", so a re-encoded
    /// asset can never keep advertising its source type.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SanitizationTransformation {
    Identity,
    StripMetadata,
    ReencodeImage { format: ImageSanitizationFormat },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SanitizedAsset {
    path: ContentPath,
    review_run_id: AssetReviewRunId,
    source_sha256: Sha256,
    published_sha256: Sha256,
    published_size: u64,
    published_content_type: AssetContentType,
    transformations: Vec<SanitizationTransformation>,
}

impl SanitizedAsset {
    /// Builds one sanitized-asset record.
    ///
    /// Runtime-side constructor: the engine defines the record, and whichever
    /// adapter performs sanitization (the native image pipeline today, a hosted
    /// image service later) fills it in.
    pub fn from_parts(
        path: ContentPath,
        review_run_id: AssetReviewRunId,
        source_sha256: Sha256,
        published_sha256: Sha256,
        published_size: u64,
        published_content_type: AssetContentType,
        transformations: Vec<SanitizationTransformation>,
    ) -> Self {
        Self {
            path,
            review_run_id,
            source_sha256,
            published_sha256,
            published_size,
            published_content_type,
            transformations,
        }
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn review_run_id(&self) -> AssetReviewRunId {
        self.review_run_id
    }

    pub fn source_sha256(&self) -> Sha256 {
        self.source_sha256
    }

    pub fn published_sha256(&self) -> Sha256 {
        self.published_sha256
    }

    pub fn published_size(&self) -> u64 {
        self.published_size
    }

    /// The media type of the bytes that will actually be served.
    pub fn published_content_type(&self) -> &AssetContentType {
        &self.published_content_type
    }

    pub fn transformations(&self) -> &[SanitizationTransformation] {
        &self.transformations
    }

    pub fn is_identity(&self) -> bool {
        self.source_sha256 == self.published_sha256
            && self.transformations == [SanitizationTransformation::Identity]
    }
}

/// Snapshot-bound mapping from reviewed source assets to immutable publication blobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SanitizedAssetSet {
    snapshot_id: SnapshotId,
    assets: Vec<SanitizedAsset>,
}

impl SanitizedAssetSet {
    /// Builds a Snapshot-bound sanitized-asset set in canonical `ContentPath` order.
    ///
    /// The ordering is not cosmetic: `get` binary-searches this vector, so
    /// unordered input would silently break lookups.
    pub fn from_assets(snapshot_id: SnapshotId, mut assets: Vec<SanitizedAsset>) -> Self {
        assets.sort_by(|left, right| left.path.cmp(&right.path));
        Self {
            snapshot_id,
            assets,
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn assets(&self) -> &[SanitizedAsset] {
        &self.assets
    }

    pub fn get(&self, path: &ContentPath) -> Option<&SanitizedAsset> {
        self.assets
            .binary_search_by(|asset| asset.path().cmp(path))
            .ok()
            .map(|index| &self.assets[index])
    }

    pub fn published_sha256(&self, path: &ContentPath) -> Option<Sha256> {
        self.get(path).map(SanitizedAsset::published_sha256)
    }

    pub fn transformations(&self, path: &ContentPath) -> Option<&[SanitizationTransformation]> {
        self.get(path).map(SanitizedAsset::transformations)
    }
}
