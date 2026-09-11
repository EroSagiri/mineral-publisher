use std::{collections::BTreeMap, error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::domain::{ContentPath, Sha256, Snapshot, SnapshotFile, SnapshotId};

use super::FinalPublicationSet;

/// A canonical path relative to the publication target root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectionTargetPath(String);

impl ProjectionTargetPath {
    pub fn new(path: impl Into<String>) -> Result<Self, ProjectionTargetPathError> {
        let path = path.into();
        validate_relative_path(&path).map(|()| Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectionTargetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The target subtree a Publisher authorizes Mineral Publisher to control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedRoot(ProjectionTargetPath);

impl ManagedRoot {
    pub fn new(path: impl Into<String>) -> Result<Self, ProjectionTargetPathError> {
        ProjectionTargetPath::new(path).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn target_for(
        &self,
        source_path: &ContentPath,
    ) -> Result<ProjectionTargetPath, ProjectionTargetPathError> {
        ProjectionTargetPath::new(format!("{}/{}", self.as_str(), source_path.as_str()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionEntryKind {
    Markdown,
    Asset,
}

/// The V1 publication mode of a managed file.
///
/// Projection entries are always ordinary, non-executable files. Keeping this
/// publisher-neutral fact explicit prevents a Git materializer from changing
/// mode behind a byte-only `PublishPlan`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PublicationFileMode {
    Regular,
    Executable,
}

/// One exact blob at one exact path in the complete desired target tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionEntry {
    target_path: ProjectionTargetPath,
    blob_sha256: Sha256,
    kind: ProjectionEntryKind,
    source_path: ContentPath,
    source_sha256: Sha256,
}

impl ProjectionEntry {
    pub fn target_path(&self) -> &ProjectionTargetPath {
        &self.target_path
    }

    /// The final bytes identity a Publisher must materialize.
    pub fn blob_sha256(&self) -> Sha256 {
        self.blob_sha256
    }

    pub fn file_mode(&self) -> PublicationFileMode {
        PublicationFileMode::Regular
    }

    pub fn kind(&self) -> ProjectionEntryKind {
        self.kind
    }

    pub fn source_path(&self) -> &ContentPath {
        &self.source_path
    }

    /// The immutable Snapshot identity from which this entry was derived.
    pub fn source_sha256(&self) -> Sha256 {
        self.source_sha256
    }
}

/// The complete, deterministic desired state for the managed public subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicProjection {
    snapshot_id: SnapshotId,
    managed_root: ManagedRoot,
    entries: Vec<ProjectionEntry>,
    projection_sha256: Sha256,
}

impl PublicProjection {
    /// Maps an already-closed publication set without reading any blob bytes.
    pub fn build(
        publication_set: &FinalPublicationSet,
        snapshot: &Snapshot,
        managed_root: ManagedRoot,
    ) -> Result<Self, PublicProjectionError> {
        if publication_set.snapshot_id() != snapshot.id() {
            return Err(PublicProjectionError::SnapshotMismatch {
                publication_set_snapshot_id: publication_set.snapshot_id(),
                snapshot_id: snapshot.id(),
            });
        }

        let snapshot_files = snapshot
            .files()
            .iter()
            .map(|file| (file.path(), file))
            .collect::<BTreeMap<_, _>>();
        let mut entries = BTreeMap::new();

        for path in publication_set.markdown_paths() {
            let file = snapshot_file(&snapshot_files, path, ProjectionEntryKind::Markdown)?;
            insert_entry(
                &mut entries,
                ProjectionEntry {
                    target_path: managed_root.target_for(path)?,
                    blob_sha256: file.sha256(),
                    kind: ProjectionEntryKind::Markdown,
                    source_path: path.clone(),
                    source_sha256: file.sha256(),
                },
            )?;
        }

        for asset in publication_set.assets() {
            let path = asset.path();
            let file = snapshot_file(&snapshot_files, path, ProjectionEntryKind::Asset)?;
            if asset.source_sha256() != file.sha256() {
                return Err(PublicProjectionError::AssetSourceIdentityMismatch {
                    path: path.clone(),
                    snapshot_sha256: file.sha256(),
                    asset_source_sha256: asset.source_sha256(),
                });
            }
            insert_entry(
                &mut entries,
                ProjectionEntry {
                    target_path: managed_root.target_for(path)?,
                    blob_sha256: asset.published_sha256(),
                    kind: ProjectionEntryKind::Asset,
                    source_path: path.clone(),
                    source_sha256: asset.source_sha256(),
                },
            )?;
        }

        let entries = entries.into_values().collect::<Vec<_>>();
        let projection_sha256 = projection_identity(&entries);
        Ok(Self {
            snapshot_id: snapshot.id(),
            managed_root,
            entries,
            projection_sha256,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn managed_root(&self) -> &ManagedRoot {
        &self.managed_root
    }

    pub fn entries(&self) -> &[ProjectionEntry] {
        &self.entries
    }

    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }
}

fn snapshot_file<'a>(
    files: &BTreeMap<&ContentPath, &'a SnapshotFile>,
    path: &ContentPath,
    kind: ProjectionEntryKind,
) -> Result<&'a SnapshotFile, PublicProjectionError> {
    files
        .get(path)
        .copied()
        .ok_or_else(|| PublicProjectionError::SourceMissingFromSnapshot {
            path: path.clone(),
            kind,
        })
}

fn insert_entry(
    entries: &mut BTreeMap<ProjectionTargetPath, ProjectionEntry>,
    entry: ProjectionEntry,
) -> Result<(), PublicProjectionError> {
    if let Some(existing) = entries.insert(entry.target_path.clone(), entry.clone()) {
        return Err(PublicProjectionError::TargetPathCollision {
            target_path: entry.target_path,
            existing_source_path: existing.source_path,
            existing_kind: existing.kind,
            incoming_source_path: entry.source_path,
            incoming_kind: entry.kind,
        });
    }
    Ok(())
}

fn projection_identity(entries: &[ProjectionEntry]) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(b"mineral-publisher-public-projection-v1\0");
    for entry in entries {
        let path = entry.target_path.as_str().as_bytes();
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path);
        hasher.update(entry.blob_sha256.as_bytes());
    }
    Sha256::new(hasher.finalize().into())
}

fn validate_relative_path(path: &str) -> Result<(), ProjectionTargetPathError> {
    if path.is_empty() {
        return Err(ProjectionTargetPathError::Empty);
    }
    let bytes = path.as_bytes();
    let has_windows_drive_prefix =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.starts_with('/') || path.contains(['\\', '\0']) || has_windows_drive_prefix {
        return Err(ProjectionTargetPathError::NotCanonical);
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(ProjectionTargetPathError::NotCanonical);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionTargetPathError {
    Empty,
    NotCanonical,
}

impl fmt::Display for ProjectionTargetPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("projection target path cannot be empty"),
            Self::NotCanonical => formatter.write_str(
                "projection target path must be canonical, relative, and use forward slashes",
            ),
        }
    }
}

impl Error for ProjectionTargetPathError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicProjectionError {
    SnapshotMismatch {
        publication_set_snapshot_id: SnapshotId,
        snapshot_id: SnapshotId,
    },
    InvalidTargetPath(ProjectionTargetPathError),
    SourceMissingFromSnapshot {
        path: ContentPath,
        kind: ProjectionEntryKind,
    },
    AssetSourceIdentityMismatch {
        path: ContentPath,
        snapshot_sha256: Sha256,
        asset_source_sha256: Sha256,
    },
    TargetPathCollision {
        target_path: ProjectionTargetPath,
        existing_source_path: ContentPath,
        existing_kind: ProjectionEntryKind,
        incoming_source_path: ContentPath,
        incoming_kind: ProjectionEntryKind,
    },
}

impl From<ProjectionTargetPathError> for PublicProjectionError {
    fn from(error: ProjectionTargetPathError) -> Self {
        Self::InvalidTargetPath(error)
    }
}

impl fmt::Display for PublicProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotMismatch { .. } => formatter
                .write_str("final publication set and snapshot must share one snapshot identity"),
            Self::InvalidTargetPath(error) => write!(formatter, "invalid target path: {error}"),
            Self::SourceMissingFromSnapshot { path, kind } => {
                write!(
                    formatter,
                    "{kind:?} source is missing from snapshot: {path}"
                )
            }
            Self::AssetSourceIdentityMismatch { path, .. } => write!(
                formatter,
                "asset source identity does not match the snapshot blob: {path}"
            ),
            Self::TargetPathCollision { target_path, .. } => {
                write!(formatter, "projection target path collision: {target_path}")
            }
        }
    }
}

impl Error for PublicProjectionError {}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        domain::{SnapshotFile, SourceId},
        workflow::{AssetReviewRunId, SanitizationTransformation, SanitizedAsset},
    };

    use super::*;

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn hash(value: u8) -> Sha256 {
        Sha256::new([value; 32])
    }

    fn snapshot(id: u64, files: &[(&str, Sha256)]) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(id).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files
                .iter()
                .map(|(name, sha256)| SnapshotFile::new(path(name), 1, *sha256, None))
                .collect(),
        )
        .unwrap()
    }

    fn asset(name: &str, source: Sha256, published: Sha256) -> SanitizedAsset {
        SanitizedAsset::from_parts_for_test(
            path(name),
            AssetReviewRunId::new(1).unwrap(),
            source,
            published,
            1,
            if source == published {
                vec![SanitizationTransformation::Identity]
            } else {
                vec![SanitizationTransformation::StripMetadata]
            },
        )
    }

    fn publication_set(
        id: u64,
        markdown: &[&str],
        assets: Vec<SanitizedAsset>,
    ) -> FinalPublicationSet {
        FinalPublicationSet::from_parts_for_test(
            SnapshotId::new(id).unwrap(),
            markdown.iter().map(|value| path(value)).collect(),
            assets,
        )
    }

    fn root() -> ManagedRoot {
        ManagedRoot::new("content").unwrap()
    }

    #[test]
    fn markdown_uses_the_immutable_snapshot_blob() {
        let markdown_sha = hash(1);
        let snapshot = snapshot(1, &[("notes/a.md", markdown_sha)]);
        let set = publication_set(1, &["notes/a.md"], vec![]);

        let projection = PublicProjection::build(&set, &snapshot, root()).unwrap();

        assert_eq!(projection.entries().len(), 1);
        let entry = &projection.entries()[0];
        assert_eq!(entry.target_path().as_str(), "content/notes/a.md");
        assert_eq!(entry.blob_sha256(), markdown_sha);
        assert_eq!(entry.source_sha256(), markdown_sha);
        assert_eq!(entry.kind(), ProjectionEntryKind::Markdown);
    }

    #[test]
    fn transformed_asset_uses_published_blob_and_retains_source_identity() {
        let source = hash(1);
        let published = hash(2);
        let snapshot = snapshot(1, &[("attachments/a.png", source)]);
        let set = publication_set(1, &[], vec![asset("attachments/a.png", source, published)]);

        let projection = PublicProjection::build(&set, &snapshot, root()).unwrap();
        let entry = &projection.entries()[0];

        assert_eq!(entry.target_path().as_str(), "content/attachments/a.png");
        assert_eq!(entry.blob_sha256(), published);
        assert_eq!(entry.source_sha256(), source);
        assert_eq!(entry.kind(), ProjectionEntryKind::Asset);
    }

    #[test]
    fn identity_asset_still_uses_the_published_identity() {
        let identity = hash(3);
        let snapshot = snapshot(1, &[("image.png", identity)]);
        let set = publication_set(1, &[], vec![asset("image.png", identity, identity)]);

        let projection = PublicProjection::build(&set, &snapshot, root()).unwrap();

        assert_eq!(projection.entries()[0].blob_sha256(), identity);
        assert_eq!(projection.entries()[0].source_sha256(), identity);
    }

    #[test]
    fn mixed_entries_are_complete_and_sorted_by_target_path() {
        let snapshot = snapshot(
            1,
            &[
                ("z.md", hash(1)),
                ("attachments/image.png", hash(2)),
                ("a.md", hash(3)),
                ("attachments/diagram.png", hash(4)),
            ],
        );
        let set = publication_set(
            1,
            &["z.md", "a.md"],
            vec![
                asset("attachments/image.png", hash(2), hash(5)),
                asset("attachments/diagram.png", hash(4), hash(6)),
            ],
        );

        let projection = PublicProjection::build(&set, &snapshot, root()).unwrap();
        let paths = projection
            .entries()
            .iter()
            .map(|entry| entry.target_path().as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            [
                "content/a.md",
                "content/attachments/diagram.png",
                "content/attachments/image.png",
                "content/z.md",
            ]
        );
    }

    #[test]
    fn empty_final_set_produces_a_valid_empty_projection() {
        let snapshot = snapshot(1, &[]);
        let set = publication_set(1, &[], vec![]);

        let projection = PublicProjection::build(&set, &snapshot, root()).unwrap();

        assert!(projection.entries().is_empty());
        assert_eq!(projection.snapshot_id(), SnapshotId::new(1).unwrap());
    }

    #[test]
    fn snapshot_mismatch_fails_closed() {
        let snapshot = snapshot(2, &[]);
        let set = publication_set(1, &[], vec![]);

        assert!(matches!(
            PublicProjection::build(&set, &snapshot, root()),
            Err(PublicProjectionError::SnapshotMismatch { .. })
        ));
    }

    #[test]
    fn asset_source_identity_mismatch_fails_closed() {
        let snapshot = snapshot(1, &[("image.png", hash(1))]);
        let set = publication_set(1, &[], vec![asset("image.png", hash(2), hash(3))]);

        assert!(matches!(
            PublicProjection::build(&set, &snapshot, root()),
            Err(PublicProjectionError::AssetSourceIdentityMismatch { .. })
        ));
    }

    #[test]
    fn missing_markdown_snapshot_file_fails_closed() {
        let snapshot = snapshot(1, &[]);
        let set = publication_set(1, &["missing.md"], vec![]);

        assert!(matches!(
            PublicProjection::build(&set, &snapshot, root()),
            Err(PublicProjectionError::SourceMissingFromSnapshot {
                kind: ProjectionEntryKind::Markdown,
                ..
            })
        ));
    }

    #[test]
    fn target_paths_and_managed_roots_reject_escape_and_absolute_forms() {
        for invalid in ["", "../secret", "/absolute", "C:/absolute", "a\\b", "a\0b"] {
            assert!(
                ProjectionTargetPath::new(invalid).is_err(),
                "accepted {invalid:?}"
            );
            assert!(ManagedRoot::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn duplicate_target_path_is_an_explicit_collision() {
        let snapshot = snapshot(1, &[("a.md", hash(1))]);
        let set = publication_set(1, &["a.md", "a.md"], vec![]);

        assert!(matches!(
            PublicProjection::build(&set, &snapshot, root()),
            Err(PublicProjectionError::TargetPathCollision {
                existing_kind: ProjectionEntryKind::Markdown,
                incoming_kind: ProjectionEntryKind::Markdown,
                ..
            })
        ));
    }

    #[test]
    fn markdown_asset_target_collision_is_rejected() {
        let identity = hash(1);
        let snapshot = snapshot(1, &[("same.md", identity)]);
        let set = publication_set(1, &["same.md"], vec![asset("same.md", identity, hash(2))]);

        assert!(matches!(
            PublicProjection::build(&set, &snapshot, root()),
            Err(PublicProjectionError::TargetPathCollision {
                existing_kind: ProjectionEntryKind::Markdown,
                incoming_kind: ProjectionEntryKind::Asset,
                ..
            })
        ));
    }

    #[test]
    fn input_order_does_not_change_entries_or_projection_identity() {
        let snapshot = snapshot(
            1,
            &[("b.md", hash(1)), ("a.md", hash(2)), ("z.png", hash(3))],
        );
        let first = publication_set(1, &["b.md", "a.md"], vec![asset("z.png", hash(3), hash(4))]);
        let second = publication_set(1, &["a.md", "b.md"], vec![asset("z.png", hash(3), hash(4))]);

        let first = PublicProjection::build(&first, &snapshot, root()).unwrap();
        let second = PublicProjection::build(&second, &snapshot, root()).unwrap();

        assert_eq!(first.entries(), second.entries());
        assert_eq!(first.projection_sha256(), second.projection_sha256());
    }
}
