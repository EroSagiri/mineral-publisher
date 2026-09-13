use std::{collections::BTreeMap, error::Error, fmt, io::Cursor};

use image::{ImageFormat, codecs::jpeg::JpegEncoder};

use crate::{
    domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId},
    ports::{BlobStore, ContentStoreError},
};

use super::{
    ActualAssetType, AssetCheckFinding, AssetCheckResult, AssetContentType, AssetReviewRunId,
    CheckedAsset, EffectiveReviewDecision, EffectiveReviewSet, ImageDimensions,
    ImageSanitizationFormat, SanitizationTransformation, SanitizedAsset, SanitizedAssetSet,
    asset_program_check::{check_image, detect_actual_type},
};

const JPEG_QUALITY: u8 = 90;

pub struct AssetSanitizer<S: BlobStore> {
    content_store: S,
}

impl<S: BlobStore> AssetSanitizer<S> {
    pub fn new(content_store: S) -> Self {
        Self { content_store }
    }

    pub fn with_content_store(content_store: S) -> Self {
        Self { content_store }
    }

    pub fn sanitize(
        &self,
        effective_reviews: &EffectiveReviewSet,
        checks: &AssetCheckResult,
        snapshot: &Snapshot,
    ) -> Result<SanitizedAssetSet, AssetSanitizationError> {
        if effective_reviews.snapshot_id() != snapshot.id() {
            return Err(AssetSanitizationError::EffectiveReviewSnapshotMismatch {
                effective_snapshot_id: effective_reviews.snapshot_id(),
                snapshot_id: snapshot.id(),
            });
        }
        if checks.snapshot_id() != snapshot.id() {
            return Err(AssetSanitizationError::CheckSnapshotMismatch {
                check_snapshot_id: checks.snapshot_id(),
                snapshot_id: snapshot.id(),
            });
        }

        let checked_by_path = checked_assets_by_path(checks)?;
        let mut plans = Vec::new();
        for review in effective_reviews
            .assets()
            .iter()
            .filter(|review| review.decision() == EffectiveReviewDecision::Approved)
        {
            let path = review.content_path();
            let file = snapshot_file(snapshot, path)?;
            let checked = checked_by_path
                .get(path)
                .copied()
                .ok_or_else(|| AssetSanitizationError::CheckResultMissing(path.clone()))?;
            let format = supported_format(path, checked)?;
            validate_check_binding(path, file, checked)?;
            plans.push((review.review_run_id(), file, checked, format));
        }

        let mut assets = Vec::with_capacity(plans.len());
        for (review_run_id, file, checked, format) in plans {
            assets.push(self.sanitize_one(review_run_id, file, checked, format)?);
        }

        Ok(SanitizedAssetSet::from_assets(snapshot.id(), assets))
    }

    fn sanitize_one(
        &self,
        review_run_id: AssetReviewRunId,
        file: &SnapshotFile,
        checked: &CheckedAsset,
        format: ImageSanitizationFormat,
    ) -> Result<SanitizedAsset, AssetSanitizationError> {
        let path = file.path();
        let source = self.content_store.read(file.sha256()).map_err(|source| {
            AssetSanitizationError::SourceRead {
                path: path.clone(),
                source,
            }
        })?;
        let source_size = usize_to_u64(source.len());
        if source_size != file.size() {
            return Err(AssetSanitizationError::SourceSizeMismatch {
                path: path.clone(),
                expected: file.size(),
                actual: source_size,
            });
        }

        let mut source_findings = Vec::new();
        let source_dimensions = verify_image(path, &source, format, &mut source_findings)?;
        if source_dimensions
            != checked
                .image_dimensions()
                .expect("preflight requires dimensions")
        {
            return Err(AssetSanitizationError::SourceDimensionsMismatch {
                path: path.clone(),
                checked: checked
                    .image_dimensions()
                    .expect("preflight requires dimensions"),
                actual: source_dimensions,
            });
        }
        if source_findings
            .iter()
            .any(|finding| matches!(finding, AssetCheckFinding::MetadataInspectionFailed))
        {
            return Err(AssetSanitizationError::MetadataInspectionFailed(
                path.clone(),
            ));
        }

        let needs_reencode = source_findings.iter().any(is_metadata_finding)
            || checked.findings().iter().any(is_metadata_finding);
        if !needs_reencode {
            // Published unchanged: the media type is the one the program check
            // actually detected in these very bytes.
            return Ok(SanitizedAsset::from_parts(
                path.clone(),
                review_run_id,
                file.sha256(),
                file.sha256(),
                file.size(),
                published_content_type(path, checked, format, false)?,
                vec![SanitizationTransformation::Identity],
            ));
        }

        let published =
            reencode(&source, format).map_err(|_| AssetSanitizationError::ImageEncodeFailed {
                path: path.clone(),
                format,
            })?;
        verify_published(path, &published, format, source_dimensions)?;
        let expected_published_sha256 = Sha256::digest(&published);
        let published_sha256 = self.content_store.store(&published).map_err(|source| {
            AssetSanitizationError::PublishedWrite {
                path: path.clone(),
                source,
            }
        })?;
        if published_sha256 != expected_published_sha256 {
            return Err(AssetSanitizationError::PublishedHashMismatch {
                path: path.clone(),
                expected: expected_published_sha256,
                actual: published_sha256,
            });
        }
        let stored = self
            .content_store
            .read(published_sha256)
            .map_err(|source| AssetSanitizationError::PublishedRead {
                path: path.clone(),
                source,
            })?;
        if stored != published {
            return Err(AssetSanitizationError::PublishedContentMismatch(
                path.clone(),
            ));
        }
        verify_published(path, &stored, format, source_dimensions)?;

        Ok(SanitizedAsset::from_parts(
            path.clone(),
            review_run_id,
            file.sha256(),
            published_sha256,
            usize_to_u64(published.len()),
            // Re-encoded: the published media type is the encoder's output format,
            // never the type the source bytes happened to have.
            published_content_type(path, checked, format, true)?,
            vec![
                SanitizationTransformation::StripMetadata,
                SanitizationTransformation::ReencodeImage { format },
            ],
        ))
    }
}

/// The media type of the bytes sanitization is about to publish.
fn published_content_type(
    path: &ContentPath,
    checked: &CheckedAsset,
    format: ImageSanitizationFormat,
    reencoded: bool,
) -> Result<AssetContentType, AssetSanitizationError> {
    let media_type = if reencoded {
        format.media_type().to_owned()
    } else {
        checked
            .actual_type()
            .media_type()
            .map(str::to_owned)
            .ok_or_else(|| AssetSanitizationError::PublishedContentTypeUnavailable(path.clone()))?
    };
    AssetContentType::new(media_type)
        .map_err(|_| AssetSanitizationError::PublishedContentTypeUnavailable(path.clone()))
}

fn checked_assets_by_path(
    checks: &AssetCheckResult,
) -> Result<BTreeMap<ContentPath, &CheckedAsset>, AssetSanitizationError> {
    let mut by_path = BTreeMap::new();
    for checked in checks.assets() {
        if by_path.insert(checked.path().clone(), checked).is_some() {
            return Err(AssetSanitizationError::DuplicateCheckResult(
                checked.path().clone(),
            ));
        }
    }
    Ok(by_path)
}

fn snapshot_file<'a>(
    snapshot: &'a Snapshot,
    path: &ContentPath,
) -> Result<&'a SnapshotFile, AssetSanitizationError> {
    snapshot
        .files()
        .binary_search_by(|file| file.path().cmp(path))
        .ok()
        .map(|index| &snapshot.files()[index])
        .ok_or_else(|| AssetSanitizationError::SnapshotFileMissing(path.clone()))
}

fn validate_check_binding(
    path: &ContentPath,
    file: &SnapshotFile,
    checked: &CheckedAsset,
) -> Result<(), AssetSanitizationError> {
    if checked.sha256() != Some(file.sha256()) || checked.size() != Some(file.size()) {
        return Err(AssetSanitizationError::CheckIdentityMismatch {
            path: path.clone(),
            snapshot_sha256: file.sha256(),
            checked_sha256: checked.sha256(),
            snapshot_size: file.size(),
            checked_size: checked.size(),
        });
    }
    if checked.image_dimensions().is_none() {
        return Err(AssetSanitizationError::CheckDidNotConfirmImageDecode(
            path.clone(),
        ));
    }
    if let Some(finding) = checked.findings().iter().find(|finding| {
        !matches!(
            finding,
            AssetCheckFinding::ExtensionContentMismatch { .. }
                | AssetCheckFinding::ExifMetadataPresent
                | AssetCheckFinding::GpsMetadataPresent
                | AssetCheckFinding::XmpMetadataPresent
        )
    }) {
        return Err(AssetSanitizationError::UnsafeProgramFinding {
            path: path.clone(),
            finding: finding.clone(),
        });
    }
    Ok(())
}

fn supported_format(
    path: &ContentPath,
    checked: &CheckedAsset,
) -> Result<ImageSanitizationFormat, AssetSanitizationError> {
    match checked.actual_type() {
        ActualAssetType::Image { extension, .. }
            if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") =>
        {
            Ok(ImageSanitizationFormat::Jpeg)
        }
        ActualAssetType::Image { extension, .. } if extension.eq_ignore_ascii_case("png") => {
            Ok(ImageSanitizationFormat::Png)
        }
        actual_type => Err(AssetSanitizationError::UnsupportedForSanitization {
            path: path.clone(),
            actual_type: actual_type.clone(),
        }),
    }
}

fn verify_image(
    path: &ContentPath,
    bytes: &[u8],
    expected_format: ImageSanitizationFormat,
    findings: &mut Vec<AssetCheckFinding>,
) -> Result<ImageDimensions, AssetSanitizationError> {
    let actual_type = detect_actual_type(bytes);
    let actual_format = match &actual_type {
        ActualAssetType::Image { extension, .. }
            if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") =>
        {
            Some(ImageSanitizationFormat::Jpeg)
        }
        ActualAssetType::Image { extension, .. } if extension.eq_ignore_ascii_case("png") => {
            Some(ImageSanitizationFormat::Png)
        }
        _ => None,
    };
    if actual_format != Some(expected_format) {
        return Err(AssetSanitizationError::ActualTypeMismatch {
            path: path.clone(),
            expected: expected_format,
            actual: actual_type,
        });
    }
    check_image(bytes, findings)
        .ok_or_else(|| AssetSanitizationError::ImageDecodeFailed(path.clone()))
}

fn verify_published(
    path: &ContentPath,
    bytes: &[u8],
    format: ImageSanitizationFormat,
    source_dimensions: ImageDimensions,
) -> Result<(), AssetSanitizationError> {
    let mut findings = Vec::new();
    let dimensions = verify_image(path, bytes, format, &mut findings)?;
    if dimensions != source_dimensions {
        return Err(AssetSanitizationError::PublishedDimensionsChanged {
            path: path.clone(),
            source: source_dimensions,
            published: dimensions,
        });
    }
    if let Some(finding) = findings.into_iter().find(|finding| {
        matches!(
            finding,
            AssetCheckFinding::ExifMetadataPresent
                | AssetCheckFinding::GpsMetadataPresent
                | AssetCheckFinding::XmpMetadataPresent
                | AssetCheckFinding::MetadataInspectionFailed
        )
    }) {
        return Err(AssetSanitizationError::PublishedMetadataPresent {
            path: path.clone(),
            finding,
        });
    }
    Ok(())
}

fn reencode(bytes: &[u8], format: ImageSanitizationFormat) -> image::ImageResult<Vec<u8>> {
    let source_format = match format {
        ImageSanitizationFormat::Jpeg => ImageFormat::Jpeg,
        ImageSanitizationFormat::Png => ImageFormat::Png,
    };
    let image = image::load_from_memory_with_format(bytes, source_format)?;
    let mut published = Vec::new();
    match format {
        ImageSanitizationFormat::Jpeg => {
            JpegEncoder::new_with_quality(&mut published, JPEG_QUALITY).encode_image(&image)?;
        }
        ImageSanitizationFormat::Png => {
            image.write_to(&mut Cursor::new(&mut published), ImageFormat::Png)?;
        }
    }
    Ok(published)
}

fn is_metadata_finding(finding: &AssetCheckFinding) -> bool {
    matches!(
        finding,
        AssetCheckFinding::ExifMetadataPresent
            | AssetCheckFinding::GpsMetadataPresent
            | AssetCheckFinding::XmpMetadataPresent
    )
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[derive(Debug)]
pub enum AssetSanitizationError {
    EffectiveReviewSnapshotMismatch {
        effective_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    CheckSnapshotMismatch {
        check_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    DuplicateCheckResult(ContentPath),
    CheckResultMissing(ContentPath),
    SnapshotFileMissing(ContentPath),
    CheckIdentityMismatch {
        path: ContentPath,
        snapshot_sha256: Sha256,
        checked_sha256: Option<Sha256>,
        snapshot_size: u64,
        checked_size: Option<u64>,
    },
    CheckDidNotConfirmImageDecode(ContentPath),
    UnsafeProgramFinding {
        path: ContentPath,
        finding: AssetCheckFinding,
    },
    UnsupportedForSanitization {
        path: ContentPath,
        actual_type: ActualAssetType,
    },
    SourceRead {
        path: ContentPath,
        source: ContentStoreError,
    },
    SourceSizeMismatch {
        path: ContentPath,
        expected: u64,
        actual: u64,
    },
    ActualTypeMismatch {
        path: ContentPath,
        expected: ImageSanitizationFormat,
        actual: ActualAssetType,
    },
    ImageDecodeFailed(ContentPath),
    SourceDimensionsMismatch {
        path: ContentPath,
        checked: ImageDimensions,
        actual: ImageDimensions,
    },
    MetadataInspectionFailed(ContentPath),
    ImageEncodeFailed {
        path: ContentPath,
        format: ImageSanitizationFormat,
    },
    PublishedHashMismatch {
        path: ContentPath,
        expected: Sha256,
        actual: Sha256,
    },
    PublishedDimensionsChanged {
        path: ContentPath,
        source: ImageDimensions,
        published: ImageDimensions,
    },
    PublishedMetadataPresent {
        path: ContentPath,
        finding: AssetCheckFinding,
    },
    PublishedWrite {
        path: ContentPath,
        source: ContentStoreError,
    },
    PublishedRead {
        path: ContentPath,
        source: ContentStoreError,
    },
    PublishedContentMismatch(ContentPath),
    PublishedContentTypeUnavailable(ContentPath),
}

impl fmt::Display for AssetSanitizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EffectiveReviewSnapshotMismatch { .. } => {
                formatter.write_str("effective review set belongs to a different snapshot")
            }
            Self::CheckSnapshotMismatch { .. } => {
                formatter.write_str("asset check result belongs to a different snapshot")
            }
            Self::DuplicateCheckResult(path) => write!(formatter, "duplicate asset check: {path}"),
            Self::CheckResultMissing(path) => write!(formatter, "asset check missing: {path}"),
            Self::SnapshotFileMissing(path) => write!(formatter, "snapshot asset missing: {path}"),
            Self::CheckIdentityMismatch { path, .. } => {
                write!(
                    formatter,
                    "asset check identity does not match snapshot: {path}"
                )
            }
            Self::CheckDidNotConfirmImageDecode(path) => {
                write!(
                    formatter,
                    "asset check did not confirm image decoding: {path}"
                )
            }
            Self::UnsafeProgramFinding { path, finding } => {
                write!(
                    formatter,
                    "asset has unsafe program finding {finding:?}: {path}"
                )
            }
            Self::UnsupportedForSanitization { path, actual_type } => {
                write!(formatter, "cannot safely sanitize {path} ({actual_type:?})")
            }
            Self::SourceRead { path, .. } => {
                write!(formatter, "could not read source asset: {path}")
            }
            Self::SourceSizeMismatch { path, .. } => {
                write!(
                    formatter,
                    "source asset size does not match snapshot: {path}"
                )
            }
            Self::ActualTypeMismatch { path, .. } => {
                write!(
                    formatter,
                    "asset actual type changed from checked type: {path}"
                )
            }
            Self::ImageDecodeFailed(path) => write!(formatter, "could not decode image: {path}"),
            Self::SourceDimensionsMismatch { path, .. } => {
                write!(
                    formatter,
                    "image dimensions do not match asset check: {path}"
                )
            }
            Self::MetadataInspectionFailed(path) => {
                write!(
                    formatter,
                    "could not reliably inspect image metadata: {path}"
                )
            }
            Self::ImageEncodeFailed { path, .. } => {
                write!(formatter, "could not encode sanitized image: {path}")
            }
            Self::PublishedHashMismatch { path, .. } => {
                write!(
                    formatter,
                    "content store returned a wrong published hash: {path}"
                )
            }
            Self::PublishedDimensionsChanged { path, .. } => {
                write!(formatter, "sanitization changed image dimensions: {path}")
            }
            Self::PublishedMetadataPresent { path, finding } => {
                write!(
                    formatter,
                    "sanitized image still has metadata {finding:?}: {path}"
                )
            }
            Self::PublishedWrite { path, .. } => {
                write!(formatter, "could not store sanitized image: {path}")
            }
            Self::PublishedRead { path, .. } => {
                write!(formatter, "could not verify stored sanitized image: {path}")
            }
            Self::PublishedContentMismatch(path) => {
                write!(formatter, "stored sanitized image bytes changed: {path}")
            }
            Self::PublishedContentTypeUnavailable(path) => write!(
                formatter,
                "could not determine the published media type of the sanitized bytes: {path}"
            ),
        }
    }
}

impl Error for AssetSanitizationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceRead { source, .. }
            | Self::PublishedWrite { source, .. }
            | Self::PublishedRead { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
        rc::Rc,
        time::SystemTime,
    };

    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

    use crate::{
        domain::{SnapshotFile, SourceId},
        workflow::{EffectiveAssetReview, EffectiveReviewDecision},
    };

    use super::*;

    #[derive(Default)]
    struct StoreState {
        blobs: RefCell<HashMap<Sha256, Vec<u8>>>,
        reads: Cell<usize>,
        writes: Cell<usize>,
        fail_write: Cell<bool>,
        corrupt_published_read: Cell<bool>,
    }

    #[derive(Clone, Default)]
    struct TestStore(Rc<StoreState>);

    impl TestStore {
        fn insert(&self, bytes: &[u8]) -> Sha256 {
            let sha256 = Sha256::digest(bytes);
            self.0.blobs.borrow_mut().insert(sha256, bytes.to_vec());
            sha256
        }
    }

    impl BlobStore for TestStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0.reads.set(self.0.reads.get() + 1);
            let bytes = self
                .0
                .blobs
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))?;
            if self.0.corrupt_published_read.get() && self.0.reads.get() > 1 {
                return Ok(b"changed after write".to_vec());
            }
            Ok(bytes)
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            self.0.writes.set(self.0.writes.get() + 1);
            let identity = Sha256::digest(content);
            if self.0.fail_write.get() {
                return Err(ContentStoreError::Missing(identity));
            }
            self.0.blobs.borrow_mut().insert(identity, content.to_vec());
            Ok(identity)
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn snapshot_id(value: u64) -> SnapshotId {
        SnapshotId::new(value).unwrap()
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&[5, 10, 15, 20, 25, 30], 2, 1, ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }

    fn jpeg() -> Vec<u8> {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 85)
            .encode(&[10, 20, 30, 40, 50, 60], 2, 1, ExtendedColorType::Rgb8)
            .unwrap();
        bytes
    }

    fn insert_jpeg_app1(jpeg: &[u8], payload: &[u8]) -> Vec<u8> {
        let length = u16::try_from(payload.len() + 2).unwrap();
        let mut result = jpeg[..2].to_vec();
        result.extend_from_slice(&[0xff, 0xe1]);
        result.extend_from_slice(&length.to_be_bytes());
        result.extend_from_slice(payload);
        result.extend_from_slice(&jpeg[2..]);
        result
    }

    fn jpeg_with_gps_exif() -> Vec<u8> {
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
        insert_jpeg_app1(&jpeg(), &payload)
    }

    fn jpeg_with_xmp() -> Vec<u8> {
        insert_jpeg_app1(
            &jpeg(),
            b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta xmlns:x='adobe:ns:meta/'/>",
        )
    }

    fn actual_type(extension: &str) -> ActualAssetType {
        ActualAssetType::Image {
            media_type: format!("image/{extension}"),
            extension: extension.to_owned(),
        }
    }

    fn fixture(
        id: SnapshotId,
        entries: Vec<(
            &str,
            Vec<u8>,
            EffectiveReviewDecision,
            Vec<AssetCheckFinding>,
        )>,
        store: &TestStore,
    ) -> (Snapshot, EffectiveReviewSet, AssetCheckResult) {
        let mut files = Vec::new();
        let mut reviews = Vec::new();
        let mut checks = Vec::new();
        for (index, (name, bytes, decision, findings)) in entries.into_iter().enumerate() {
            let content_path = path(name);
            let sha256 = store.insert(&bytes);
            let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("bin");
            files.push(SnapshotFile::new(
                content_path.clone(),
                usize_to_u64(bytes.len()),
                sha256,
                None,
            ));
            reviews.push(EffectiveAssetReview::from_parts_for_test(
                content_path.clone(),
                AssetReviewRunId::new(index as u64 + 1).unwrap(),
                decision,
                vec![path("a.md"), path("b.md")],
            ));
            let (asset_type, dimensions) = match extension {
                "jpg" | "jpeg" => (actual_type("jpg"), Some(ImageDimensions::new(2, 1))),
                "png" => (actual_type("png"), Some(ImageDimensions::new(2, 1))),
                "pdf" => (ActualAssetType::Pdf, None),
                _ => (ActualAssetType::Unknown, None),
            };
            checks.push(CheckedAsset::new(
                content_path,
                vec![path("a.md"), path("b.md")],
                Some(sha256),
                asset_type,
                Some(usize_to_u64(bytes.len())),
                dimensions,
                findings,
            ));
        }
        (
            Snapshot::new(
                id,
                SystemTime::UNIX_EPOCH,
                SourceId::new("test").unwrap(),
                files,
            )
            .unwrap(),
            EffectiveReviewSet::from_assets(id, reviews),
            AssetCheckResult::from_assets(id, checks),
        )
    }

    #[test]
    fn clean_png_is_an_explicit_identity_without_a_write() {
        let store = TestStore::default();
        let bytes = png();
        let source_sha256 = Sha256::digest(&bytes);
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(1),
            vec![(
                "clean.png",
                bytes,
                EffectiveReviewDecision::Approved,
                vec![],
            )],
            &store,
        );

        let result = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();

        assert_eq!(result.snapshot_id(), snapshot_id(1));
        assert_eq!(
            result.published_sha256(&path("clean.png")),
            Some(source_sha256)
        );
        assert!(result.get(&path("clean.png")).unwrap().is_identity());
        assert_eq!(store.0.reads.get(), 1);
        assert_eq!(store.0.writes.get(), 0);
    }

    #[test]
    fn jpeg_exif_and_gps_are_removed_and_source_blob_is_immutable() {
        let store = TestStore::default();
        let source = jpeg_with_gps_exif();
        let source_sha256 = Sha256::digest(&source);
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(2),
            vec![(
                "photo.jpg",
                source.clone(),
                EffectiveReviewDecision::Approved,
                vec![
                    AssetCheckFinding::ExifMetadataPresent,
                    AssetCheckFinding::GpsMetadataPresent,
                ],
            )],
            &store,
        );

        let first = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();
        let second = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();
        let asset = first.get(&path("photo.jpg")).unwrap();
        let published = store.0.blobs.borrow()[&asset.published_sha256()].clone();
        let mut findings = Vec::new();
        check_image(&published, &mut findings).unwrap();

        assert_ne!(asset.source_sha256(), asset.published_sha256());
        assert_eq!(
            asset.transformations(),
            [
                SanitizationTransformation::StripMetadata,
                SanitizationTransformation::ReencodeImage {
                    format: ImageSanitizationFormat::Jpeg
                }
            ]
        );
        assert!(!findings.iter().any(is_metadata_finding));
        assert_eq!(first, second);
        assert_eq!(snapshot.files()[0].sha256(), source_sha256);
        assert_eq!(store.0.blobs.borrow()[&source_sha256], source);
    }

    #[test]
    fn jpeg_xmp_is_removed_when_fixture_is_supported_by_decoder() {
        let store = TestStore::default();
        let source = jpeg_with_xmp();
        let mut source_findings = Vec::new();
        check_image(&source, &mut source_findings).unwrap();
        assert!(source_findings.contains(&AssetCheckFinding::XmpMetadataPresent));
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(3),
            vec![(
                "xmp.jpg",
                source,
                EffectiveReviewDecision::Approved,
                vec![AssetCheckFinding::XmpMetadataPresent],
            )],
            &store,
        );

        let result = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();
        let published = store.0.blobs.borrow()[&result.assets()[0].published_sha256()].clone();
        let mut findings = Vec::new();
        check_image(&published, &mut findings).unwrap();
        assert!(!findings.contains(&AssetCheckFinding::XmpMetadataPresent));
    }

    /// The published media type is a fact about the bytes sanitization actually
    /// produced, never the label the vault path or the source bytes carried.
    #[test]
    fn published_content_type_describes_the_final_representation() {
        // Published unchanged: the type the program check detected in the bytes.
        let store = TestStore::default();
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(11),
            vec![(
                "clean.png",
                png(),
                EffectiveReviewDecision::Approved,
                vec![],
            )],
            &store,
        );
        let identity = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();
        let asset = identity.get(&path("clean.png")).unwrap();
        assert!(asset.is_identity());
        assert_eq!(asset.published_content_type().as_str(), "image/png");

        // Re-encoded: the encoder's output format. The fixture's source label is
        // `image/jpg`, so keeping it would prove the source type leaked through.
        let store = TestStore::default();
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(12),
            vec![(
                "photo.jpg",
                jpeg_with_gps_exif(),
                EffectiveReviewDecision::Approved,
                vec![AssetCheckFinding::GpsMetadataPresent],
            )],
            &store,
        );
        let reencoded = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();
        let asset = reencoded.get(&path("photo.jpg")).unwrap();
        assert_eq!(
            asset.transformations(),
            [
                SanitizationTransformation::StripMetadata,
                SanitizationTransformation::ReencodeImage {
                    format: ImageSanitizationFormat::Jpeg
                }
            ]
        );
        assert_eq!(asset.published_content_type().as_str(), "image/jpeg");
        assert_ne!(
            asset.published_content_type().as_str(),
            "image/jpg",
            "the source type must not be published as the final representation"
        );
        assert_eq!(
            asset.published_size(),
            u64::try_from(store.0.blobs.borrow()[&asset.published_sha256()].len()).unwrap()
        );
    }

    #[test]
    fn rejected_and_pending_assets_are_not_read_written_or_returned() {
        let store = TestStore::default();
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(4),
            vec![
                (
                    "rejected.png",
                    png(),
                    EffectiveReviewDecision::Rejected,
                    vec![],
                ),
                (
                    "pending.png",
                    png(),
                    EffectiveReviewDecision::PendingHumanReview,
                    vec![],
                ),
            ],
            &store,
        );

        let result = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();

        assert!(result.assets().is_empty());
        assert_eq!(store.0.reads.get(), 0);
        assert_eq!(store.0.writes.get(), 0);
    }

    #[test]
    fn shared_asset_is_sanitized_once() {
        let store = TestStore::default();
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(5),
            vec![(
                "shared.png",
                png(),
                EffectiveReviewDecision::Approved,
                vec![],
            )],
            &store,
        );

        let result = AssetSanitizer::with_content_store(store.clone())
            .sanitize(&reviews, &checks, &snapshot)
            .unwrap();

        assert_eq!(result.assets().len(), 1);
        assert_eq!(store.0.reads.get(), 1);
        assert_eq!(store.0.writes.get(), 0);
    }

    #[test]
    fn approved_pdf_and_unknown_binary_fail_closed_without_io() {
        for (name, bytes) in [
            ("report.pdf", b"%PDF-1.7".to_vec()),
            ("data.bin", vec![1, 2, 3]),
        ] {
            let store = TestStore::default();
            let (snapshot, reviews, checks) = fixture(
                snapshot_id(6),
                vec![(name, bytes, EffectiveReviewDecision::Approved, vec![])],
                &store,
            );

            assert!(matches!(
                AssetSanitizer::with_content_store(store.clone())
                    .sanitize(&reviews, &checks, &snapshot),
                Err(AssetSanitizationError::UnsupportedForSanitization { .. })
            ));
            assert_eq!(store.0.reads.get(), 0);
            assert_eq!(store.0.writes.get(), 0);
        }
    }

    #[test]
    fn snapshot_mismatch_fails_before_io() {
        let store = TestStore::default();
        let (snapshot, _, checks) = fixture(
            snapshot_id(7),
            vec![("a.png", png(), EffectiveReviewDecision::Approved, vec![])],
            &store,
        );
        let (_, reviews, _) = fixture(
            snapshot_id(8),
            vec![("a.png", png(), EffectiveReviewDecision::Approved, vec![])],
            &store,
        );

        assert!(matches!(
            AssetSanitizer::with_content_store(store.clone())
                .sanitize(&reviews, &checks, &snapshot),
            Err(AssetSanitizationError::EffectiveReviewSnapshotMismatch { .. })
        ));
        assert_eq!(store.0.reads.get(), 0);
        assert_eq!(store.0.writes.get(), 0);
    }

    #[test]
    fn published_write_failure_is_explicit() {
        let store = TestStore::default();
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(9),
            vec![(
                "photo.jpg",
                jpeg_with_gps_exif(),
                EffectiveReviewDecision::Approved,
                vec![AssetCheckFinding::GpsMetadataPresent],
            )],
            &store,
        );
        store.0.fail_write.set(true);

        assert!(matches!(
            AssetSanitizer::with_content_store(store).sanitize(&reviews, &checks, &snapshot),
            Err(AssetSanitizationError::PublishedWrite { .. })
        ));
    }

    #[test]
    fn corrupt_post_write_read_fails_closed() {
        let store = TestStore::default();
        let (snapshot, reviews, checks) = fixture(
            snapshot_id(10),
            vec![(
                "photo.jpg",
                jpeg_with_gps_exif(),
                EffectiveReviewDecision::Approved,
                vec![AssetCheckFinding::ExifMetadataPresent],
            )],
            &store,
        );
        store.0.corrupt_published_read.set(true);

        assert!(matches!(
            AssetSanitizer::with_content_store(store).sanitize(&reviews, &checks, &snapshot),
            Err(AssetSanitizationError::PublishedContentMismatch(_))
        ));
    }
}
