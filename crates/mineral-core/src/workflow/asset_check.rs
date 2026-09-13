use std::error::Error;

use serde::{Deserialize, Serialize};

use crate::domain::{ContentPath, Sha256, Snapshot, SnapshotId};

use super::CandidateAssetSet;

/// The only way the engine runs the deterministic asset program checks.
///
/// The engine owns the sequence (check → policy → review → audit) and the shape
/// of the result. A runtime supplies the ability to read and decode real bytes:
/// the native host uses its own image pipeline today, and another runtime may
/// delegate to a hosted image service. Neither is allowed to decide policy.
pub trait AssetInspector {
    type Error: Error + 'static;

    fn inspect(
        &self,
        candidates: &CandidateAssetSet,
        snapshot: &Snapshot,
    ) -> Result<AssetCheckResult, Self::Error>;
}

/// Deterministic facts inferred from the immutable asset bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ActualAssetType {
    Image {
        media_type: String,
        extension: String,
    },
    Pdf,
    OtherBinary {
        media_type: String,
        extension: String,
    },
    Unknown,
}

impl ActualAssetType {
    pub fn media_type(&self) -> Option<&str> {
        match self {
            Self::Image { media_type, .. } | Self::OtherBinary { media_type, .. } => {
                Some(media_type)
            }
            Self::Pdf => Some("application/pdf"),
            Self::Unknown => None,
        }
    }

    pub fn extension(&self) -> Option<&str> {
        match self {
            Self::Image { extension, .. } | Self::OtherBinary { extension, .. } => Some(extension),
            Self::Pdf => Some("pdf"),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImageDimensions {
    width: u32,
    height: u32,
}

impl ImageDimensions {
    #[doc(hidden)]
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

/// A deterministic problem or sanitization signal found for one asset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AssetCheckFinding {
    SnapshotFileMissing,
    SnapshotFileIsMarkdown,
    MissingBlob {
        sha256: Sha256,
    },
    CorruptBlob {
        expected: Sha256,
        actual: Sha256,
    },
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    ExtensionContentMismatch {
        path_extension: String,
        actual_extension: String,
    },
    DecodeFailed,
    ExifMetadataPresent,
    GpsMetadataPresent,
    XmpMetadataPresent,
    MetadataInspectionFailed,
    UnsupportedType,
    UnknownType,
}

/// The result for exactly one entry supplied by `CandidateAssetSet`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckedAsset {
    path: ContentPath,
    dependents: Vec<ContentPath>,
    sha256: Option<Sha256>,
    actual_type: ActualAssetType,
    size: Option<u64>,
    image_dimensions: Option<ImageDimensions>,
    findings: Vec<AssetCheckFinding>,
}

impl CheckedAsset {
    #[doc(hidden)]
    pub fn new(
        path: ContentPath,
        dependents: Vec<ContentPath>,
        sha256: Option<Sha256>,
        actual_type: ActualAssetType,
        size: Option<u64>,
        image_dimensions: Option<ImageDimensions>,
        findings: Vec<AssetCheckFinding>,
    ) -> Self {
        Self {
            path,
            dependents,
            sha256,
            actual_type,
            size,
            image_dimensions,
            findings,
        }
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn dependents(&self) -> &[ContentPath] {
        &self.dependents
    }

    pub fn sha256(&self) -> Option<Sha256> {
        self.sha256
    }

    pub fn actual_type(&self) -> &ActualAssetType {
        &self.actual_type
    }

    pub fn size(&self) -> Option<u64> {
        self.size
    }

    pub fn image_dimensions(&self) -> Option<ImageDimensions> {
        self.image_dimensions
    }

    pub fn findings(&self) -> &[AssetCheckFinding] {
        &self.findings
    }

    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// A completed check. Its existence does not imply that every asset is clean or approved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetCheckResult {
    snapshot_id: SnapshotId,
    assets: Vec<CheckedAsset>,
}

impl AssetCheckResult {
    pub fn empty(snapshot_id: SnapshotId) -> Self {
        Self {
            snapshot_id,
            assets: Vec::new(),
        }
    }

    /// Builds one completed check from the checked assets, in the order produced.
    ///
    /// This is the runtime-side constructor: the engine defines the result, and
    /// whichever adapter performs the platform checks fills it in.
    pub fn from_assets(snapshot_id: SnapshotId, assets: Vec<CheckedAsset>) -> Self {
        Self {
            snapshot_id,
            assets,
        }
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn assets(&self) -> &[CheckedAsset] {
        &self.assets
    }

    pub fn is_clean(&self) -> bool {
        self.assets.iter().all(CheckedAsset::is_clean)
    }
}
