use std::{error::Error, fmt, io::Cursor};

use exif::Context as ExifContext;
use image::{GenericImageView, ImageDecoder, ImageReader};
use serde::{Deserialize, Serialize};

use crate::{
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId},
    storage::{ContentStoreError, LocalContentStore},
};

use super::CandidateAssetSet;

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
    #[cfg(test)]
    pub(crate) fn new(width: u32, height: u32) -> Self {
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
    #[cfg(test)]
    pub(crate) fn new(
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
    #[cfg(test)]
    pub(crate) fn from_assets_for_test(snapshot_id: SnapshotId, assets: Vec<CheckedAsset>) -> Self {
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

/// Checks only the assets already selected by `CandidateAssetSet`.
#[derive(Clone, Debug)]
pub struct AssetProgramCheck {
    content_store: LocalContentStore,
}

impl AssetProgramCheck {
    pub fn new(content_store: LocalContentStore) -> Self {
        Self { content_store }
    }

    pub fn run(
        &self,
        candidates: &CandidateAssetSet,
        snapshot: &Snapshot,
    ) -> Result<AssetCheckResult, AssetProgramCheckError> {
        if candidates.snapshot_id() != snapshot.id() {
            return Err(AssetProgramCheckError::SnapshotMismatch {
                candidate_snapshot_id: candidates.snapshot_id(),
                snapshot_id: snapshot.id(),
            });
        }

        let mut assets = Vec::with_capacity(candidates.entries().len());
        for candidate in candidates.entries() {
            let Some(file) = snapshot
                .files()
                .binary_search_by(|file| file.path().cmp(candidate.path()))
                .ok()
                .map(|index| &snapshot.files()[index])
            else {
                assets.push(CheckedAsset {
                    path: candidate.path().clone(),
                    dependents: candidate.dependents().to_vec(),
                    sha256: None,
                    actual_type: ActualAssetType::Unknown,
                    size: None,
                    image_dimensions: None,
                    findings: vec![AssetCheckFinding::SnapshotFileMissing],
                });
                continue;
            };

            if is_markdown(file.path()) {
                assets.push(empty_checked_asset(
                    candidate.path().clone(),
                    candidate.dependents().to_vec(),
                    file,
                    AssetCheckFinding::SnapshotFileIsMarkdown,
                ));
                continue;
            }

            let bytes = match self.content_store.read(file.sha256()) {
                Ok(bytes) => bytes,
                Err(ContentStoreError::Missing(sha256)) => {
                    assets.push(empty_checked_asset(
                        candidate.path().clone(),
                        candidate.dependents().to_vec(),
                        file,
                        AssetCheckFinding::MissingBlob { sha256 },
                    ));
                    continue;
                }
                Err(ContentStoreError::Corrupt { expected, actual }) => {
                    assets.push(empty_checked_asset(
                        candidate.path().clone(),
                        candidate.dependents().to_vec(),
                        file,
                        AssetCheckFinding::CorruptBlob { expected, actual },
                    ));
                    continue;
                }
                Err(source) => {
                    return Err(AssetProgramCheckError::ContentStore {
                        path: candidate.path().clone(),
                        source,
                    });
                }
            };

            assets.push(check_bytes(
                candidate.path().clone(),
                candidate.dependents().to_vec(),
                file,
                &bytes,
            ));
        }

        Ok(AssetCheckResult {
            snapshot_id: snapshot.id(),
            assets,
        })
    }
}

fn empty_checked_asset(
    path: ContentPath,
    dependents: Vec<ContentPath>,
    file: &SnapshotFile,
    finding: AssetCheckFinding,
) -> CheckedAsset {
    CheckedAsset {
        path,
        dependents,
        sha256: Some(file.sha256()),
        actual_type: ActualAssetType::Unknown,
        size: None,
        image_dimensions: None,
        findings: vec![finding],
    }
}

fn check_bytes(
    path: ContentPath,
    dependents: Vec<ContentPath>,
    file: &SnapshotFile,
    bytes: &[u8],
) -> CheckedAsset {
    let actual_type = detect_actual_type(bytes);
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut findings = Vec::new();
    if actual_size != file.size() {
        findings.push(AssetCheckFinding::SizeMismatch {
            expected: file.size(),
            actual: actual_size,
        });
    }
    if let (Some(path_extension), Some(actual_extension)) =
        (path_extension(&path), actual_type.extension())
        && !extensions_match(path_extension, actual_extension)
    {
        findings.push(AssetCheckFinding::ExtensionContentMismatch {
            path_extension: path_extension.to_ascii_lowercase(),
            actual_extension: actual_extension.to_owned(),
        });
    }

    let image_dimensions = match &actual_type {
        ActualAssetType::Image { .. } => check_image(bytes, &mut findings),
        ActualAssetType::Pdf => None,
        ActualAssetType::OtherBinary { .. } => {
            findings.push(AssetCheckFinding::UnsupportedType);
            None
        }
        ActualAssetType::Unknown => {
            findings.push(AssetCheckFinding::UnknownType);
            None
        }
    };

    CheckedAsset {
        path,
        dependents,
        sha256: Some(file.sha256()),
        actual_type,
        size: Some(actual_size),
        image_dimensions,
        findings,
    }
}

pub(super) fn detect_actual_type(bytes: &[u8]) -> ActualAssetType {
    if bytes.starts_with(b"%PDF-") {
        return ActualAssetType::Pdf;
    }

    if let Some(kind) = infer::get(bytes) {
        let media_type = kind.mime_type().to_owned();
        let extension = kind.extension().to_owned();
        if kind.matcher_type() == infer::MatcherType::Image {
            ActualAssetType::Image {
                media_type,
                extension,
            }
        } else {
            ActualAssetType::OtherBinary {
                media_type,
                extension,
            }
        }
    } else {
        ActualAssetType::Unknown
    }
}

pub(super) fn check_image(
    bytes: &[u8],
    findings: &mut Vec<AssetCheckFinding>,
) -> Option<ImageDimensions> {
    let image = match ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(reader) => match reader.decode() {
            Ok(image) => image,
            Err(_) => {
                findings.push(AssetCheckFinding::DecodeFailed);
                return None;
            }
        },
        Err(_) => {
            findings.push(AssetCheckFinding::DecodeFailed);
            return None;
        }
    };
    let (width, height) = image.dimensions();

    inspect_metadata(bytes, findings);
    Some(ImageDimensions { width, height })
}

fn inspect_metadata(bytes: &[u8], findings: &mut Vec<AssetCheckFinding>) {
    let reader = match ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(reader) => reader,
        Err(_) => {
            findings.push(AssetCheckFinding::MetadataInspectionFailed);
            return;
        }
    };
    let mut decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(_) => {
            findings.push(AssetCheckFinding::MetadataInspectionFailed);
            return;
        }
    };

    match decoder.exif_metadata() {
        Ok(Some(raw_exif)) => {
            findings.push(AssetCheckFinding::ExifMetadataPresent);
            match exif::Reader::new().read_raw(raw_exif) {
                Ok(exif) => {
                    if exif
                        .fields()
                        .any(|field| field.tag.context() == ExifContext::Gps)
                    {
                        findings.push(AssetCheckFinding::GpsMetadataPresent);
                    }
                }
                Err(_) => findings.push(AssetCheckFinding::MetadataInspectionFailed),
            }
        }
        Ok(None) => {}
        Err(_) => findings.push(AssetCheckFinding::MetadataInspectionFailed),
    }
    match decoder.xmp_metadata() {
        Ok(Some(_)) => findings.push(AssetCheckFinding::XmpMetadataPresent),
        Ok(None) => {}
        Err(_) => findings.push(AssetCheckFinding::MetadataInspectionFailed),
    }
}

fn is_markdown(path: &ContentPath) -> bool {
    path_extension(path).is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn path_extension(path: &ContentPath) -> Option<&str> {
    let name = path.as_str().rsplit('/').next()?;
    let (_, extension) = name.rsplit_once('.')?;
    (!extension.is_empty()).then_some(extension)
}

fn extensions_match(path_extension: &str, actual_extension: &str) -> bool {
    fn canonical(extension: &str) -> &str {
        if extension.eq_ignore_ascii_case("jpg")
            || extension.eq_ignore_ascii_case("jpeg")
            || extension.eq_ignore_ascii_case("jpe")
        {
            "jpeg"
        } else if extension.eq_ignore_ascii_case("tif") || extension.eq_ignore_ascii_case("tiff") {
            "tiff"
        } else {
            extension
        }
    }

    canonical(path_extension).eq_ignore_ascii_case(canonical(actual_extension))
}

#[derive(Debug)]
pub enum AssetProgramCheckError {
    SnapshotMismatch {
        candidate_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    ContentStore {
        path: ContentPath,
        source: ContentStoreError,
    },
}

impl fmt::Display for AssetProgramCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch {
                candidate_snapshot_id,
                snapshot_id,
            } => write!(
                formatter,
                "candidate asset snapshot {candidate_snapshot_id:?} does not match snapshot {snapshot_id:?}"
            ),
            Self::ContentStore { path, .. } => {
                write!(
                    formatter,
                    "could not read immutable content for asset {path}"
                )
            }
        }
    }
}

impl Error for AssetProgramCheckError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ContentStore { source, .. } => Some(source),
            Self::SnapshotMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
        time::SystemTime,
    };

    use image::{
        ExtendedColorType, ImageEncoder,
        codecs::{jpeg::JpegEncoder, png::PngEncoder},
    };

    use crate::{
        content::{
            AnalyzedMarkdown, AssetDependencyGraph, MarkdownReferenceParser, Resolution,
            ResolvedReference,
        },
        domain::SourceId,
        policy::{PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId},
        workflow::PublicPolicyRunResult,
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-asset-check-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn snapshot_id(value: u64) -> SnapshotId {
        SnapshotId::new(value).unwrap()
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let pixels = vec![0_u8; (width * height * 3) as usize];
        PngEncoder::new(&mut bytes)
            .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }

    fn zip() -> Vec<u8> {
        b"PK\x03\x04\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec()
    }

    fn jpeg_with_gps_exif() -> Vec<u8> {
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .write_image(&[0, 0, 0], 1, 1, ExtendedColorType::Rgb8)
            .unwrap();

        // Minimal TIFF data with GPSLatitudeRef. This builds a test fixture;
        // production metadata parsing remains delegated to kamadak-exif.
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x8825_u16.to_le_bytes());
        tiff.extend_from_slice(&4_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&26_u32.to_le_bytes());
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&2_u16.to_le_bytes());
        tiff.extend_from_slice(&2_u32.to_le_bytes());
        tiff.extend_from_slice(b"N\0\0\0");
        tiff.extend_from_slice(&0_u32.to_le_bytes());

        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let segment_length = u16::try_from(payload.len() + 2).unwrap();
        let mut with_exif = jpeg[..2].to_vec();
        with_exif.extend_from_slice(&[0xff, 0xe1]);
        with_exif.extend_from_slice(&segment_length.to_be_bytes());
        with_exif.extend_from_slice(&payload);
        with_exif.extend_from_slice(&jpeg[2..]);
        with_exif
    }

    fn analyzed(document: &str, assets: &[&str]) -> AnalyzedMarkdown {
        let markdown = assets
            .iter()
            .map(|asset| format!("![[{asset}]]"))
            .collect::<Vec<_>>()
            .join(" ");
        let references = MarkdownReferenceParser::parse(&markdown)
            .into_iter()
            .zip(assets.iter())
            .map(|(reference, asset)| {
                ResolvedReference::new(reference, Resolution::ResolvedAsset { path: path(asset) })
            })
            .collect();
        AnalyzedMarkdown::new(
            SnapshotFile::new(
                path(document),
                markdown.len() as u64,
                Sha256::digest(markdown.as_bytes()),
                None,
            ),
            references,
        )
    }

    fn candidates(id: SnapshotId, documents: &[(&str, &[&str])]) -> CandidateAssetSet {
        let analyses = documents
            .iter()
            .map(|(document, assets)| analyzed(document, assets))
            .collect::<Vec<_>>();
        let outcomes = documents
            .iter()
            .enumerate()
            .map(|(index, (document, _))| {
                ReviewRun::rehydrate(
                    ReviewRunId::new(index as u64 + 1).unwrap(),
                    id,
                    path(document),
                    Sha256::digest(document.as_bytes()),
                    PolicyIdentity::new("public", "1", Sha256::digest(b"policy")).unwrap(),
                    (PublicPolicyDecision::ReviewApproved, None),
                    0,
                )
            })
            .collect();
        let policy = PublicPolicyRunResult::from_document_outcomes_for_test(id, outcomes);
        CandidateAssetSet::select(&policy, &AssetDependencyGraph::build(id, &analyses)).unwrap()
    }

    fn snapshot(id: SnapshotId, files: Vec<SnapshotFile>) -> Snapshot {
        Snapshot::new(
            id,
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files,
        )
        .unwrap()
    }

    fn stored_file(store: &LocalContentStore, file_path: &str, bytes: &[u8]) -> SnapshotFile {
        let sha256 = store.store(bytes).unwrap();
        SnapshotFile::new(path(file_path), bytes.len() as u64, sha256, None)
    }

    fn check(
        store: LocalContentStore,
        candidates: &CandidateAssetSet,
        snapshot: &Snapshot,
    ) -> AssetCheckResult {
        AssetProgramCheck::new(store)
            .run(candidates, snapshot)
            .unwrap()
    }

    #[test]
    fn valid_image_is_decoded_from_bytes_with_dimensions() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = png(2, 3);
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["image.png"])]),
            &snapshot(id, vec![stored_file(&store, "image.png", &bytes)]),
        );

        assert!(result.is_clean());
        assert_eq!(
            result.assets()[0].image_dimensions(),
            Some(ImageDimensions {
                width: 2,
                height: 3
            })
        );
        assert!(
            matches!(result.assets()[0].actual_type(), ActualAssetType::Image { media_type, extension } if media_type == "image/png" && extension == "png")
        );
    }

    #[test]
    fn exif_and_gps_metadata_are_reported_without_modifying_the_blob() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = jpeg_with_gps_exif();
        let original_sha256 = Sha256::digest(&bytes);
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["photo.jpg"])]),
            &snapshot(id, vec![stored_file(&store, "photo.jpg", &bytes)]),
        );

        assert_eq!(
            result.assets()[0].findings(),
            [
                AssetCheckFinding::ExifMetadataPresent,
                AssetCheckFinding::GpsMetadataPresent,
            ]
        );
        assert_eq!(result.assets()[0].sha256(), Some(original_sha256));
        assert_eq!(store.read(original_sha256).unwrap(), bytes);
    }

    #[test]
    fn corrupt_image_signature_is_not_a_pass() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = b"\x89PNG\r\n\x1a\ncorrupt";
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["bad.png"])]),
            &snapshot(id, vec![stored_file(&store, "bad.png", bytes)]),
        );

        assert_eq!(
            result.assets()[0].findings(),
            [AssetCheckFinding::DecodeFailed]
        );
        assert_eq!(result.assets()[0].image_dimensions(), None);
    }

    #[test]
    fn disguised_extension_uses_actual_bytes_and_retains_mismatch() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = zip();
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["fake.png"])]),
            &snapshot(id, vec![stored_file(&store, "fake.png", &bytes)]),
        );

        assert!(
            matches!(result.assets()[0].actual_type(), ActualAssetType::OtherBinary { media_type, extension } if media_type == "application/zip" && extension == "zip")
        );
        assert_eq!(
            result.assets()[0].findings(),
            [
                AssetCheckFinding::ExtensionContentMismatch {
                    path_extension: "png".to_owned(),
                    actual_extension: "zip".to_owned()
                },
                AssetCheckFinding::UnsupportedType,
            ]
        );
    }

    #[test]
    fn pdf_is_identified_by_immutable_bytes() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = b"%PDF-1.7\n";
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["report.pdf"])]),
            &snapshot(id, vec![stored_file(&store, "report.pdf", bytes)]),
        );

        assert_eq!(result.assets()[0].actual_type(), &ActualAssetType::Pdf);
        assert!(result.assets()[0].is_clean());
    }

    #[test]
    fn recognized_binary_and_unknown_bytes_are_explicit() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let archive = zip();
        let unknown = b"not a recognized format";
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["archive.zip", "data.bin"])]),
            &snapshot(
                id,
                vec![
                    stored_file(&store, "archive.zip", &archive),
                    stored_file(&store, "data.bin", unknown),
                ],
            ),
        );

        assert!(matches!(
            result.assets()[0].actual_type(),
            ActualAssetType::OtherBinary { .. }
        ));
        assert_eq!(
            result.assets()[0].findings(),
            [AssetCheckFinding::UnsupportedType]
        );
        assert_eq!(result.assets()[1].actual_type(), &ActualAssetType::Unknown);
        assert_eq!(
            result.assets()[1].findings(),
            [AssetCheckFinding::UnknownType]
        );
    }

    #[test]
    fn missing_and_corrupt_blobs_are_per_asset_findings() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let missing = Sha256::digest(b"missing");
        let corrupt = store.store(b"original").unwrap();
        fs::write(store.root().join(corrupt.to_string()), b"corrupt").unwrap();
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("a.md", &["missing.bin", "corrupt.bin"])]),
            &snapshot(
                id,
                vec![
                    SnapshotFile::new(path("missing.bin"), 7, missing, None),
                    SnapshotFile::new(path("corrupt.bin"), 8, corrupt, None),
                ],
            ),
        );

        assert!(
            matches!(result.assets()[0].findings(), [AssetCheckFinding::CorruptBlob { expected, .. }] if *expected == corrupt)
        );
        assert_eq!(
            result.assets()[1].findings(),
            [AssetCheckFinding::MissingBlob { sha256: missing }]
        );
    }

    #[test]
    fn missing_snapshot_path_and_markdown_candidate_are_explicit() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let id = snapshot_id(1);
        let markdown = stored_file(&store, "wrong.md", b"body");
        let result = check(
            store,
            &candidates(id, &[("a.md", &["missing.png", "wrong.md"])]),
            &snapshot(id, vec![markdown]),
        );

        assert_eq!(
            result.assets()[0].findings(),
            [AssetCheckFinding::SnapshotFileMissing]
        );
        assert_eq!(
            result.assets()[1].findings(),
            [AssetCheckFinding::SnapshotFileIsMarkdown]
        );
    }

    #[test]
    fn snapshot_mismatch_is_an_execution_error() {
        let directory = TestDirectory::new();
        let result = AssetProgramCheck::new(LocalContentStore::new(directory.path())).run(
            &candidates(snapshot_id(1), &[]),
            &snapshot(snapshot_id(2), vec![]),
        );

        assert!(matches!(
            result,
            Err(AssetProgramCheckError::SnapshotMismatch { .. })
        ));
    }

    #[test]
    fn size_mismatch_is_reported_while_bytes_are_still_checked() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = png(1, 1);
        let mut file = stored_file(&store, "image.png", &bytes);
        file = SnapshotFile::new(file.path().clone(), file.size() + 1, file.sha256(), None);
        let id = snapshot_id(1);
        let result = check(
            store,
            &candidates(id, &[("a.md", &["image.png"])]),
            &snapshot(id, vec![file]),
        );

        assert!(matches!(
            result.assets()[0].findings(),
            [AssetCheckFinding::SizeMismatch { .. }]
        ));
        assert_eq!(
            result.assets()[0].image_dimensions(),
            Some(ImageDimensions {
                width: 1,
                height: 1
            })
        );
    }

    #[test]
    fn shared_asset_is_checked_once_with_all_dependents() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let bytes = png(1, 1);
        let id = snapshot_id(1);
        let result = check(
            store.clone(),
            &candidates(id, &[("b.md", &["shared.png"]), ("a.md", &["shared.png"])]),
            &snapshot(id, vec![stored_file(&store, "shared.png", &bytes)]),
        );

        assert_eq!(result.assets().len(), 1);
        assert_eq!(
            result.assets()[0].dependents(),
            [path("a.md"), path("b.md")]
        );
    }

    #[test]
    fn only_candidates_are_checked_and_output_is_deterministic() {
        let directory = TestDirectory::new();
        let store = LocalContentStore::new(directory.path());
        let a = png(1, 1);
        let z = png(2, 2);
        let orphan = png(3, 3);
        let id = snapshot_id(1);
        let snapshot = snapshot(
            id,
            vec![
                stored_file(&store, "z.png", &z),
                stored_file(&store, "orphan.png", &orphan),
                stored_file(&store, "a.png", &a),
            ],
        );
        let first = check(
            store.clone(),
            &candidates(id, &[("b.md", &["z.png"]), ("a.md", &["a.png"])]),
            &snapshot,
        );
        let second = check(
            store,
            &candidates(id, &[("a.md", &["a.png"]), ("b.md", &["z.png"])]),
            &snapshot,
        );

        assert_eq!(first, second);
        assert_eq!(
            first
                .assets()
                .iter()
                .map(|asset| asset.path().as_str())
                .collect::<Vec<_>>(),
            ["a.png", "z.png"]
        );
    }
}
