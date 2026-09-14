use std::{error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::{
    domain::{ContentPath, Sha256, SnapshotId, SourceId},
    ports::{BlobStore, ContentStoreError},
};

use super::{
    lfs::{LfsPointerError, RequiredLfsObject, build_lfs_pointer, parse_lfs_pointer},
    projection::{
        BackupProjection, BackupProjectionError, BackupRepresentationPolicy, hash_length_prefixed,
    },
};

/// Version of the backup-delivery identity encoding.
pub const BACKUP_DELIVERY_VERSION: u8 = 1;

/// Where the deterministic manifest lives inside the backup repository.
pub const BACKUP_MANIFEST_PATH: &str = ".mineral-backup/manifest";

/// Where the managed Git attributes live inside the backup repository.
pub const BACKUP_GIT_ATTRIBUTES_PATH: &str = ".gitattributes";

/// How one path is stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupRepresentationKind {
    /// Ordinary Git blob holding the original bytes.
    Git,
    /// Git LFS pointer blob; the bytes live in the LFS endpoint.
    Lfs,
}

impl BackupRepresentationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Lfs => "lfs",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "git" => Some(Self::Git),
            "lfs" => Some(Self::Lfs),
            _ => None,
        }
    }
}

/// What one path holds inside the backup Git tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupRepresentation {
    /// The tree holds the original bytes.
    GitBlob { blob_sha256: Sha256, size: u64 },
    /// The tree holds a pointer; the original bytes live in LFS.
    ///
    /// Invariants 1 and 2 are frozen here: `source_sha256` is both the Snapshot
    /// identity and the LFS OID, and `size` is the Snapshot size.
    GitLfs {
        source_sha256: Sha256,
        size: u64,
        pointer_blob_sha256: Sha256,
    },
}

impl BackupRepresentation {
    pub fn kind(&self) -> BackupRepresentationKind {
        match self {
            Self::GitBlob { .. } => BackupRepresentationKind::Git,
            Self::GitLfs { .. } => BackupRepresentationKind::Lfs,
        }
    }

    /// The identity a verifier must find after restoring this path.
    pub fn source_sha256(&self) -> Sha256 {
        match self {
            Self::GitBlob { blob_sha256, .. } => *blob_sha256,
            Self::GitLfs { source_sha256, .. } => *source_sha256,
        }
    }

    /// The size of the original file, frozen with its identity (Invariant 2).
    pub fn source_size(&self) -> u64 {
        match self {
            Self::GitBlob { size, .. } | Self::GitLfs { size, .. } => *size,
        }
    }

    /// The blob the Git tree stores for this path.
    pub fn tree_blob_sha256(&self) -> Sha256 {
        match self {
            Self::GitBlob { blob_sha256, .. } => *blob_sha256,
            Self::GitLfs {
                pointer_blob_sha256,
                ..
            } => *pointer_blob_sha256,
        }
    }
}

/// One path of the backup delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupDeliveryFile {
    path: ContentPath,
    representation: BackupRepresentation,
}

impl BackupDeliveryFile {
    /// Rebuilds one delivery file from its durable form.
    pub fn from_parts(path: ContentPath, representation: BackupRepresentation) -> Self {
        Self {
            path,
            representation,
        }
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn representation(&self) -> &BackupRepresentation {
        &self.representation
    }
}

/// The complete, frozen description of one backup delivery.
///
/// It is what a resume needs: the file set, each path's representation, the pointer,
/// attribute and manifest blobs, and every LFS object the commit requires. The
/// delivery identity covers all of it, so two runs that would publish different
/// bytes can never share an identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupDeliveryProjection {
    snapshot_id: SnapshotId,
    source_id: SourceId,
    source_projection_sha256: Sha256,
    files: Vec<BackupDeliveryFile>,
    required_lfs_objects: Vec<RequiredLfsObject>,
    manifest_blob_sha256: Sha256,
    git_attributes_blob_sha256: Sha256,
    delivery_sha256: Sha256,
}

impl BackupDeliveryProjection {
    /// Rebuilds one delivery from already-final parts (durable decode).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        snapshot_id: SnapshotId,
        source_id: SourceId,
        source_projection_sha256: Sha256,
        files: Vec<BackupDeliveryFile>,
        required_lfs_objects: Vec<RequiredLfsObject>,
        manifest_blob_sha256: Sha256,
        git_attributes_blob_sha256: Sha256,
    ) -> Result<Self, BackupDeliveryError> {
        let mut files = files;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(duplicate) = files
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
            .map(|pair| pair[0].path.clone())
        {
            return Err(BackupDeliveryError::DuplicatePath(duplicate));
        }
        let mut objects = required_lfs_objects;
        objects.sort();
        if let Some(conflict) = objects
            .windows(2)
            .find(|pair| pair[0].oid() == pair[1].oid() && pair[0].size() != pair[1].size())
        {
            return Err(BackupDeliveryError::ConflictingLfsObject {
                oid: conflict[0].oid(),
            });
        }
        objects.dedup();

        let delivery_sha256 = identity(
            snapshot_id,
            source_projection_sha256,
            &files,
            &objects,
            manifest_blob_sha256,
            git_attributes_blob_sha256,
        );
        Ok(Self {
            snapshot_id,
            source_id,
            source_projection_sha256,
            files,
            required_lfs_objects: objects,
            manifest_blob_sha256,
            git_attributes_blob_sha256,
            delivery_sha256,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    pub fn source_projection_sha256(&self) -> Sha256 {
        self.source_projection_sha256
    }

    pub fn files(&self) -> &[BackupDeliveryFile] {
        &self.files
    }

    pub fn required_lfs_objects(&self) -> &[RequiredLfsObject] {
        &self.required_lfs_objects
    }

    pub fn manifest_blob_sha256(&self) -> Sha256 {
        self.manifest_blob_sha256
    }

    pub fn git_attributes_blob_sha256(&self) -> Sha256 {
        self.git_attributes_blob_sha256
    }

    pub fn delivery_sha256(&self) -> Sha256 {
        self.delivery_sha256
    }

    /// The deterministic manifest of this delivery, as bytes that are already in CAS.
    pub fn manifest_bytes(&self) -> Result<Vec<u8>, BackupManifestError> {
        render_manifest(
            self.snapshot_id,
            &self.source_id,
            self.source_projection_sha256,
            &self.files,
        )
    }
}

/// Chooses each path's representation, stores every derived blob in CAS and freezes
/// the complete delivery.
///
/// Derived blobs are the LFS pointers, the managed `.gitattributes` and the manifest.
/// The source blobs are never written or modified here: a backup adds derived blobs
/// to CAS and reuses the Snapshot's own blobs for the Git side.
pub fn build_backup_delivery<B: BlobStore>(
    projection: &BackupProjection,
    policy: &impl BackupRepresentationPolicy,
    blobs: &B,
) -> Result<BackupDeliveryProjection, BackupDeliveryError> {
    let mut files = Vec::with_capacity(projection.files().len());
    let mut required_objects = Vec::new();

    for file in projection.files() {
        let kind = policy.choose(file.path(), file.size(), None);
        let representation = match kind {
            BackupRepresentationKind::Git => BackupRepresentation::GitBlob {
                blob_sha256: file.source_sha256(),
                size: file.size(),
            },
            BackupRepresentationKind::Lfs => {
                // Invariant 1: the pointer names the Snapshot bytes themselves.
                let pointer = build_lfs_pointer(file.source_sha256(), file.size());
                let pointer_blob_sha256 =
                    blobs.store(&pointer).map_err(BackupDeliveryError::Blob)?;
                required_objects.push(RequiredLfsObject::new(file.source_sha256(), file.size()));
                BackupRepresentation::GitLfs {
                    source_sha256: file.source_sha256(),
                    size: file.size(),
                    pointer_blob_sha256,
                }
            }
        };
        files.push(BackupDeliveryFile {
            path: file.path().clone(),
            representation,
        });
    }

    let git_attributes_blob_sha256 = blobs
        .store(&policy.git_attributes())
        .map_err(BackupDeliveryError::Blob)?;
    let manifest = render_manifest(
        projection.snapshot_id(),
        projection.source_id(),
        projection.projection_sha256(),
        &files,
    )
    .map_err(BackupDeliveryError::Manifest)?;
    let manifest_blob_sha256 = blobs.store(&manifest).map_err(BackupDeliveryError::Blob)?;

    BackupDeliveryProjection::from_parts(
        projection.snapshot_id(),
        projection.source_id().clone(),
        projection.projection_sha256(),
        files,
        required_objects,
        manifest_blob_sha256,
        git_attributes_blob_sha256,
    )
}

fn identity(
    snapshot_id: SnapshotId,
    source_projection_sha256: Sha256,
    files: &[BackupDeliveryFile],
    objects: &[RequiredLfsObject],
    manifest_blob_sha256: Sha256,
    git_attributes_blob_sha256: Sha256,
) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(b"mineral.backup-delivery");
    hasher.update([BACKUP_DELIVERY_VERSION]);
    hasher.update(snapshot_id.get().to_be_bytes());
    hasher.update(source_projection_sha256.as_bytes());
    hasher.update(manifest_blob_sha256.as_bytes());
    hasher.update(git_attributes_blob_sha256.as_bytes());
    for file in files {
        hash_length_prefixed(&mut hasher, file.path.as_str());
        hasher.update([match file.representation.kind() {
            BackupRepresentationKind::Git => 0,
            BackupRepresentationKind::Lfs => 1,
        }]);
        hasher.update(file.representation.source_sha256().as_bytes());
        hasher.update(file.representation.source_size().to_be_bytes());
        hasher.update(file.representation.tree_blob_sha256().as_bytes());
    }
    for object in objects {
        hasher.update(object.oid().as_bytes());
        hasher.update(object.size().to_be_bytes());
    }
    Sha256::new(hasher.finalize().into())
}

/// Renders the deterministic backup manifest.
///
/// The manifest is provenance, never authority: it is generated from the frozen
/// delivery and can be checked against the tree, but a verifier trusts the tree and
/// the pointer bytes, not the manifest's own claims.
pub fn render_manifest(
    snapshot_id: SnapshotId,
    source_id: &SourceId,
    source_projection_sha256: Sha256,
    files: &[BackupDeliveryFile],
) -> Result<Vec<u8>, BackupManifestError> {
    let mut text = String::new();
    text.push_str("mineral-backup-manifest v1\n");
    text.push_str(&format!("snapshot {}\n", snapshot_id.get()));
    text.push_str(&format!("source {}\n", source_id.as_str()));
    text.push_str(&format!("projection {source_projection_sha256}\n"));
    for file in files {
        let path = file.path().as_str();
        if path.chars().any(char::is_control) {
            return Err(BackupManifestError::UnrepresentablePath(
                file.path().clone(),
            ));
        }
        text.push_str(&format!(
            "{} {} {} {path}\n",
            file.representation.kind().as_str(),
            file.representation.source_size(),
            file.representation.source_sha256()
        ));
    }
    Ok(text.into_bytes())
}

/// One manifest line, as a verifier reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupManifestEntry {
    path: ContentPath,
    sha256: Sha256,
    size: u64,
    storage: BackupRepresentationKind,
}

impl BackupManifestEntry {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn sha256(&self) -> Sha256 {
        self.sha256
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn storage(&self) -> BackupRepresentationKind {
        self.storage
    }
}

/// A parsed backup manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupManifest {
    snapshot_id: SnapshotId,
    source_id: SourceId,
    source_projection_sha256: Sha256,
    entries: Vec<BackupManifestEntry>,
}

impl BackupManifest {
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    pub fn source_projection_sha256(&self) -> Sha256 {
        self.source_projection_sha256
    }

    pub fn entries(&self) -> &[BackupManifestEntry] {
        &self.entries
    }

    /// Parses one manifest, refusing anything it does not fully understand.
    pub fn parse(bytes: &[u8]) -> Result<Self, BackupManifestError> {
        let text = std::str::from_utf8(bytes).map_err(|_| BackupManifestError::Damaged)?;
        let mut lines = text.split('\n');
        if lines.next() != Some("mineral-backup-manifest v1") {
            return Err(BackupManifestError::UnknownVersion);
        }
        let snapshot_id = parse_field(lines.next(), "snapshot")?
            .parse::<u64>()
            .ok()
            .and_then(|value| SnapshotId::new(value).ok())
            .ok_or(BackupManifestError::Damaged)?;
        let source_id = SourceId::new(parse_field(lines.next(), "source")?)
            .map_err(|_| BackupManifestError::Damaged)?;
        let source_projection_sha256 = parse_sha256(parse_field(lines.next(), "projection")?)?;

        let mut entries = Vec::new();
        let mut saw_end = false;
        for line in lines {
            if line.is_empty() {
                saw_end = true;
                break;
            }
            let mut fields = line.splitn(4, ' ');
            let storage = fields
                .next()
                .and_then(BackupRepresentationKind::parse)
                .ok_or(BackupManifestError::Damaged)?;
            let size = fields
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or(BackupManifestError::Damaged)?;
            let sha256 = parse_sha256(fields.next().ok_or(BackupManifestError::Damaged)?)?;
            let path = fields.next().ok_or(BackupManifestError::Damaged)?;
            let path = ContentPath::new(path).map_err(|_| BackupManifestError::Damaged)?;
            entries.push(BackupManifestEntry {
                path,
                sha256,
                size,
                storage,
            });
        }
        if !saw_end || entries.is_empty() && !text.ends_with('\n') {
            return Err(BackupManifestError::Damaged);
        }
        Ok(Self {
            snapshot_id,
            source_id,
            source_projection_sha256,
            entries,
        })
    }
}

fn parse_field<'a>(line: Option<&'a str>, name: &str) -> Result<&'a str, BackupManifestError> {
    line.and_then(|line| line.strip_prefix(&format!("{name} ")))
        .filter(|value| !value.is_empty())
        .ok_or(BackupManifestError::Damaged)
}

fn parse_sha256(value: &str) -> Result<Sha256, BackupManifestError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BackupManifestError::Damaged);
    }
    let mut digest = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(chunk).map_err(|_| BackupManifestError::Damaged)?;
        digest[index] = u8::from_str_radix(text, 16).map_err(|_| BackupManifestError::Damaged)?;
    }
    Ok(Sha256::new(digest))
}

/// Why a backup manifest is not usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupManifestError {
    Damaged,
    UnknownVersion,
    UnrepresentablePath(ContentPath),
    /// The manifest's own LFS pointer disagrees with its recorded identity.
    PointerMismatch {
        path: ContentPath,
    },
    Pointer(LfsPointerError),
}

impl fmt::Display for BackupManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Damaged => formatter.write_str("backup manifest is damaged"),
            Self::UnknownVersion => formatter.write_str("backup manifest uses an unknown version"),
            Self::UnrepresentablePath(path) => {
                write!(formatter, "backup manifest cannot describe path {path:?}")
            }
            Self::PointerMismatch { path } => {
                write!(formatter, "backup manifest and pointer disagree for {path}")
            }
            Self::Pointer(error) => write!(formatter, "backup pointer is unusable: {error}"),
        }
    }
}

impl Error for BackupManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Pointer(error) => Some(error),
            _ => None,
        }
    }
}

/// Checks one manifest entry against the pointer bytes the tree holds for it.
pub fn verify_manifest_pointer(
    entry: &BackupManifestEntry,
    pointer_bytes: &[u8],
) -> Result<(), BackupManifestError> {
    let pointer = parse_lfs_pointer(pointer_bytes).map_err(BackupManifestError::Pointer)?;
    if pointer.oid() != entry.sha256 || pointer.size() != entry.size {
        return Err(BackupManifestError::PointerMismatch {
            path: entry.path.clone(),
        });
    }
    Ok(())
}

/// Why a backup delivery cannot be built.
#[derive(Debug)]
pub enum BackupDeliveryError {
    Blob(ContentStoreError),
    Projection(BackupProjectionError),
    Manifest(BackupManifestError),
    DuplicatePath(ContentPath),
    ConflictingLfsObject { oid: Sha256 },
}

impl fmt::Display for BackupDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blob(error) => {
                write!(formatter, "could not store a derived backup blob: {error}")
            }
            Self::Projection(error) => write!(formatter, "{error}"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::DuplicatePath(path) => write!(formatter, "backup delivery repeats {path}"),
            Self::ConflictingLfsObject { oid } => write!(
                formatter,
                "one LFS object identity was recorded with two different sizes: {oid}"
            ),
        }
    }
}

impl Error for BackupDeliveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Blob(error) => Some(error),
            Self::Projection(error) => Some(error),
            Self::Manifest(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, time::SystemTime};

    use super::*;
    use crate::{
        backup::projection::TypeFirstBackupRepresentationPolicy,
        domain::{Snapshot, SnapshotFile},
    };

    #[derive(Default)]
    struct MemoryStore {
        blobs: RefCell<HashMap<Sha256, Vec<u8>>>,
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
            let identity = Sha256::digest(content);
            self.blobs.borrow_mut().insert(identity, content.to_vec());
            Ok(identity)
        }
    }

    fn snapshot(entries: &[(&str, &[u8])]) -> Snapshot {
        let files = entries
            .iter()
            .map(|(path, bytes)| {
                SnapshotFile::new(
                    ContentPath::new(*path).unwrap(),
                    bytes.len() as u64,
                    Sha256::digest(bytes),
                    None,
                )
            })
            .collect::<Vec<_>>();
        Snapshot::new(
            SnapshotId::new(3).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("bedrock").unwrap(),
            files,
        )
        .unwrap()
    }

    fn delivery(entries: &[(&str, &[u8])]) -> (BackupDeliveryProjection, MemoryStore) {
        let store = MemoryStore::default();
        let projection = BackupProjection::build(&snapshot(entries)).unwrap();
        let delivery =
            build_backup_delivery(&projection, &TypeFirstBackupRepresentationPolicy, &store)
                .unwrap();
        (delivery, store)
    }

    #[test]
    fn text_goes_to_git_and_binary_goes_to_lfs_with_the_snapshot_identity() {
        let image = b"original jpeg bytes";
        let (delivery, _) = delivery(&[("notes/a.md", b"# note"), ("attachments/a.jpg", image)]);

        let note = delivery
            .files()
            .iter()
            .find(|file| file.path().as_str() == "notes/a.md")
            .unwrap();
        assert_eq!(
            note.representation(),
            &BackupRepresentation::GitBlob {
                blob_sha256: Sha256::digest(b"# note"),
                size: 6,
            }
        );

        let photo = delivery
            .files()
            .iter()
            .find(|file| file.path().as_str() == "attachments/a.jpg")
            .unwrap();
        let BackupRepresentation::GitLfs {
            source_sha256,
            size,
            pointer_blob_sha256,
        } = photo.representation()
        else {
            panic!("a jpg must be an LFS object");
        };
        // Invariants 1 and 2.
        assert_eq!(*source_sha256, Sha256::digest(image));
        assert_eq!(*size, image.len() as u64);
        assert_eq!(
            delivery.required_lfs_objects(),
            [RequiredLfsObject::new(
                Sha256::digest(image),
                image.len() as u64
            )]
        );
        // The tree stores the pointer, never the 20 MB payload.
        assert_ne!(*pointer_blob_sha256, *source_sha256);
    }

    #[test]
    fn the_pointer_blob_is_in_cas_and_the_source_blob_is_untouched() {
        let image = b"big binary payload";
        let (_delivery, store) = delivery(&[("attachments/a.jpg", image)]);

        let pointer = build_lfs_pointer(Sha256::digest(image), image.len() as u64);
        let pointer_identity = Sha256::digest(&pointer);

        assert_eq!(store.read(pointer_identity).unwrap(), pointer);
        // Pointers exist; the source blob was never written or rewritten by delivery.
        assert!(matches!(
            store.read(Sha256::digest(image)),
            Err(ContentStoreError::Missing(_))
        ));
    }

    #[test]
    fn the_manifest_is_deterministic_and_describes_every_path() {
        let (first, _) = delivery(&[("notes/a.md", b"a"), ("img/a.png", b"png")]);
        let (second, _) = delivery(&[("img/a.png", b"png"), ("notes/a.md", b"a")]);

        assert_eq!(first.delivery_sha256(), second.delivery_sha256());
        let manifest = BackupManifest::parse(&first.manifest_bytes().unwrap()).unwrap();
        assert_eq!(manifest.snapshot_id().get(), 3);
        assert_eq!(manifest.source_id().as_str(), "bedrock");
        assert_eq!(manifest.entries().len(), 2);
        assert_eq!(manifest.entries()[0].path().as_str(), "img/a.png");
        assert_eq!(
            manifest.entries()[0].storage(),
            BackupRepresentationKind::Lfs
        );
        assert_eq!(manifest.entries()[0].sha256(), Sha256::digest(b"png"));
        assert_eq!(manifest.entries()[0].size(), 3);
        assert_eq!(
            manifest.entries()[1].storage(),
            BackupRepresentationKind::Git
        );
    }

    #[test]
    fn the_delivery_identity_covers_representation_and_pointer() {
        let (text_like, _) = delivery(&[("a.txt", b"same bytes")]);
        let (binary_like, _) = delivery(&[("a.bin", b"same bytes")]);

        assert_ne!(text_like.delivery_sha256(), binary_like.delivery_sha256());
        assert_eq!(
            text_like.files()[0].representation().source_sha256(),
            binary_like.files()[0].representation().source_sha256()
        );
    }

    #[test]
    fn one_blob_referenced_twice_requires_one_lfs_object() {
        let (delivery, _) = delivery(&[("img/a.jpg", b"shared"), ("img/b.jpg", b"shared")]);

        assert_eq!(delivery.required_lfs_objects().len(), 1);
        assert_eq!(delivery.files().len(), 2);
    }

    /// Renaming a binary changes the tree and the delivery identity, but not the
    /// object the endpoint must hold.
    #[test]
    fn a_rename_never_requires_a_new_lfs_object() {
        let (before, _) = delivery(&[("img/a.jpg", b"payload")]);
        let (after, _) = delivery(&[("img/renamed.jpg", b"payload")]);

        assert_ne!(before.delivery_sha256(), after.delivery_sha256());
        assert_eq!(before.required_lfs_objects(), after.required_lfs_objects());
        assert_eq!(after.required_lfs_objects().len(), 1);
    }

    #[test]
    fn a_manifest_pointer_round_trip_is_verified() {
        let (delivery, store) = delivery(&[("img/a.jpg", b"payload")]);
        let entry = BackupManifest::parse(&delivery.manifest_bytes().unwrap())
            .unwrap()
            .entries()
            .to_vec()
            .remove(0);
        let pointer = store
            .read(delivery.files()[0].representation().tree_blob_sha256())
            .unwrap();

        verify_manifest_pointer(&entry, &pointer).unwrap();

        let mut tampered = entry.clone();
        tampered.size += 1;
        assert!(matches!(
            verify_manifest_pointer(&tampered, &pointer),
            Err(BackupManifestError::PointerMismatch { .. })
        ));
    }

    #[test]
    fn a_damaged_manifest_fails_closed() {
        for damaged in [
            &b"mineral-backup-manifest v2\nsnapshot 3\n"[..],
            b"mineral-backup-manifest v1\nsnapshot 0\nsource bedrock\nprojection 00\n",
            b"mineral-backup-manifest v1\nsnapshot 3\nsource bedrock\nprojection 0000000000000000000000000000000000000000000000000000000000000000\ngit 1 0000000000000000000000000000000000000000000000000000000000000000 a.md",
            b"mineral-backup-manifest v1\nsnapshot 3\nsource bedrock\nprojection 0000000000000000000000000000000000000000000000000000000000000000\nternary 1 0000000000000000000000000000000000000000000000000000000000000000 a.md\n",
        ] {
            assert!(BackupManifest::parse(damaged).is_err(), "{damaged:?}");
        }
    }
}
