use std::{error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::{
    domain::{ContentPath, Sha256, Snapshot, SnapshotId, SourceId},
    workflow::ManagedRoot,
};

use super::delivery::BackupRepresentationKind;

/// Version of the backup-projection identity encoding.
pub const BACKUP_PROJECTION_VERSION: u8 = 1;

/// The managed root every Snapshot path is shifted under.
///
/// The backup repository never claims the repository root for the vault: the vault
/// lives under `vault/`, so the backup repo can hold its own metadata
/// (`.gitattributes`, `.mineral-backup/manifest`) without colliding with a user path
/// and without changing any relative path inside the vault.
pub const BACKUP_VAULT_ROOT: &str = "vault";

/// One Snapshot file, as the backup sees it.
///
/// Invariants 1 and 2 start here: `source_sha256` is the Snapshot file's content
/// identity and `size` is its size. Nothing in the backup pipeline may rewrite
/// either of them, and no path is added, dropped or renamed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupProjectionFile {
    path: ContentPath,
    source_sha256: Sha256,
    size: u64,
}

impl BackupProjectionFile {
    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn source_sha256(&self) -> Sha256 {
        self.source_sha256
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

/// The complete, byte-faithful view of one Snapshot that a backup publishes.
///
/// It is built straight from a stabilized Snapshot: every Snapshot file appears
/// exactly once, in canonical path order, with its original identity and size. It is
/// deliberately *not* a `PublicProjection`: no policy, review, privacy decision,
/// sanitization or URL rewrite takes part in it, so private files, unpublished files
/// and original EXIF all survive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupProjection {
    snapshot_id: SnapshotId,
    source_id: SourceId,
    files: Vec<BackupProjectionFile>,
    projection_sha256: Sha256,
}

impl BackupProjection {
    /// Builds the backup view of one Snapshot.
    pub fn build(snapshot: &Snapshot) -> Result<Self, BackupProjectionError> {
        let mut files = snapshot
            .files()
            .iter()
            .map(|file| BackupProjectionFile {
                path: file.path().clone(),
                source_sha256: file.sha256(),
                size: file.size(),
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(duplicate) = files
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
            .map(|pair| pair[0].path.clone())
        {
            return Err(BackupProjectionError::DuplicatePath(duplicate));
        }
        // The manifest is line-oriented, so a path that cannot be written on one
        // line would make the backup impossible to describe. It is refused here,
        // before any byte reaches a repository.
        if let Some(path) = files
            .iter()
            .find(|file| file.path.as_str().chars().any(char::is_control))
            .map(|file| file.path.clone())
        {
            return Err(BackupProjectionError::UnrepresentablePath(path));
        }

        let projection_sha256 = identity(snapshot.id(), &files);
        Ok(Self {
            snapshot_id: snapshot.id(),
            source_id: snapshot.source_id().clone(),
            files,
            projection_sha256,
        })
    }

    /// Rebuilds one projection from already-final files (durable decode).
    pub fn from_parts(
        snapshot_id: SnapshotId,
        source_id: SourceId,
        files: Vec<BackupProjectionFile>,
    ) -> Result<Self, BackupProjectionError> {
        let mut files = files;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(duplicate) = files
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
            .map(|pair| pair[0].path.clone())
        {
            return Err(BackupProjectionError::DuplicatePath(duplicate));
        }
        let projection_sha256 = identity(snapshot_id, &files);
        Ok(Self {
            snapshot_id,
            source_id,
            files,
            projection_sha256,
        })
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    /// The source this Snapshot came from, recorded in the backup manifest.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    pub fn files(&self) -> &[BackupProjectionFile] {
        &self.files
    }

    pub fn projection_sha256(&self) -> Sha256 {
        self.projection_sha256
    }

    /// Where one Snapshot path lives inside the backup repository.
    pub fn tree_path(path: &ContentPath) -> ContentPath {
        ContentPath::new(format!("{BACKUP_VAULT_ROOT}/{}", path.as_str()))
            .expect("a canonical path under a canonical root stays canonical")
    }
}

fn identity(snapshot_id: SnapshotId, files: &[BackupProjectionFile]) -> Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(b"mineral.backup-projection");
    hasher.update([BACKUP_PROJECTION_VERSION]);
    hasher.update(snapshot_id.get().to_be_bytes());
    for file in files {
        hash_length_prefixed(&mut hasher, file.path.as_str());
        hasher.update(file.source_sha256.as_bytes());
        hasher.update(file.size.to_be_bytes());
    }
    Sha256::new(hasher.finalize().into())
}

pub(crate) fn hash_length_prefixed(hasher: &mut Sha256Hasher, value: &str) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value.as_bytes());
}

/// Which representation one path takes in the backup repository.
///
/// It is a *stable property of the path*, not of the current file size: a file that
/// grows from 9 MB to 11 MB must not silently move between Git and LFS, because that
/// would rewrite history for a path whose role never changed.
pub trait BackupRepresentationPolicy {
    fn choose(
        &self,
        path: &ContentPath,
        size: u64,
        content_type: Option<&str>,
    ) -> BackupRepresentationKind;

    /// The managed `.gitattributes` bytes this policy needs in the backup repo.
    ///
    /// It belongs to the policy, not to the pipeline: a policy that stores different
    /// paths in LFS must also describe those paths to Git LFS on checkout.
    fn git_attributes(&self) -> Vec<u8>;
}

/// The default policy: text goes to Git, binary goes to LFS, decided by extension.
///
/// `size` and `content_type` are accepted so a future policy can use them, but this
/// one deliberately ignores size and never reads the file's bytes: classification
/// must not require touching asset content.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TypeFirstBackupRepresentationPolicy;

impl TypeFirstBackupRepresentationPolicy {
    /// Extensions that are always stored as LFS objects, lowercase and without a dot.
    pub const LFS_EXTENSIONS: &'static [&'static str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "avif", "bmp", "tiff", "tif", "pdf", "mp4", "mov",
        "webm", "zip", "7z", "tar", "gz", "mp3", "wav", "flac",
    ];

    /// The managed `.gitattributes` content for this policy.
    ///
    /// `* -text` keeps every path byte-exact on checkout (no EOL rewriting), and the
    /// LFS filter lines make `git lfs pull` restore the binary paths. The file is
    /// generated, so a user's own `vault/.gitattributes` stays ordinary backup
    /// content.
    pub fn managed_git_attributes(&self) -> Vec<u8> {
        let mut text = String::from(
            "# Managed by Mineral. This file is regenerated on every backup; edit the\n\
             # policy in Mineral's configuration instead of editing this file.\n\
             #\n\
             # Every path is stored byte-exact (-text), and binary assets are Git LFS\n\
             # pointers whose OID is the SHA-256 of the original bytes.\n\
             * -text\n",
        );
        for extension in Self::LFS_EXTENSIONS {
            text.push_str(&format!(
                "*.{extension} filter=lfs diff=lfs merge=lfs -text\n"
            ));
        }
        text.into_bytes()
    }

    fn is_lfs_extension(path: &ContentPath) -> bool {
        path_extension(path).is_some_and(|extension| {
            Self::LFS_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&extension))
        })
    }
}

impl BackupRepresentationPolicy for TypeFirstBackupRepresentationPolicy {
    fn git_attributes(&self) -> Vec<u8> {
        self.managed_git_attributes()
    }

    fn choose(
        &self,
        path: &ContentPath,
        _size: u64,
        _content_type: Option<&str>,
    ) -> BackupRepresentationKind {
        if Self::is_lfs_extension(path) {
            BackupRepresentationKind::Lfs
        } else {
            BackupRepresentationKind::Git
        }
    }
}

fn path_extension(path: &ContentPath) -> Option<String> {
    let name = path.as_str().rsplit('/').next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty()).then(|| extension.to_owned())
}

/// The managed root of one backup tree, as the Git seam wants it.
pub fn backup_managed_root() -> ManagedRoot {
    ManagedRoot::new(BACKUP_VAULT_ROOT).expect("the backup root is a valid managed root")
}

/// Why a Snapshot cannot become a backup projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupProjectionError {
    /// One path appeared twice, so the Snapshot does not describe a file set.
    DuplicatePath(ContentPath),
    /// A path contains a control character and cannot be described on one line.
    UnrepresentablePath(ContentPath),
}

impl fmt::Display for BackupProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePath(path) => write!(formatter, "backup projection repeats {path}"),
            Self::UnrepresentablePath(path) => {
                write!(formatter, "backup projection cannot describe path {path:?}")
            }
        }
    }
}

impl Error for BackupProjectionError {}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::domain::{Snapshot, SnapshotFile, SourceId};

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
            SnapshotId::new(7).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("vault").unwrap(),
            files,
        )
        .unwrap()
    }

    #[test]
    fn every_snapshot_file_enters_the_projection_exactly_once_in_order() {
        let snapshot = snapshot(&[
            ("notes/b.md", b"b"),
            ("private/secret.md", b"secret"),
            ("attachments/photo.jpg", b"photo"),
            ("a.md", b"a"),
        ]);

        let projection = BackupProjection::build(&snapshot).unwrap();

        let paths = projection
            .files()
            .iter()
            .map(|file| file.path().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "a.md",
                "attachments/photo.jpg",
                "notes/b.md",
                "private/secret.md"
            ],
            "private content is backed up too, and order is canonical"
        );
        assert_eq!(projection.files().len(), snapshot.files().len());
    }

    #[test]
    fn projection_identity_is_deterministic_and_covers_every_input() {
        let first = BackupProjection::build(&snapshot(&[("a.md", b"a"), ("b.md", b"b")])).unwrap();
        let reordered =
            BackupProjection::build(&snapshot(&[("b.md", b"b"), ("a.md", b"a")])).unwrap();
        let changed =
            BackupProjection::build(&snapshot(&[("a.md", b"a"), ("b.md", b"c")])).unwrap();

        assert_eq!(first.projection_sha256(), reordered.projection_sha256());
        assert_ne!(first.projection_sha256(), changed.projection_sha256());
        assert_eq!(
            first.files()[0].source_sha256(),
            Sha256::digest(b"a"),
            "the projection keeps the Snapshot identity, never a re-hash of its own"
        );
        assert_eq!(first.files()[0].size(), 1);
    }

    #[test]
    fn the_tree_shift_keeps_relative_paths_inside_the_vault() {
        let path = ContentPath::new("notes/a.md").unwrap();

        assert_eq!(
            BackupProjection::tree_path(&path).as_str(),
            "vault/notes/a.md"
        );
        assert_eq!(backup_managed_root().as_str(), "vault");
    }

    #[test]
    fn a_described_path_that_cannot_be_written_on_one_line_fails_closed() {
        let file = SnapshotFile::new(
            ContentPath::new("weird\tname.md").unwrap(),
            1,
            Sha256::digest(b"x"),
            None,
        );
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("vault").unwrap(),
            vec![file],
        );

        // A Snapshot that refuses the path outright is also fail-closed.
        if let Ok(snapshot) = snapshot {
            assert!(matches!(
                BackupProjection::build(&snapshot),
                Err(BackupProjectionError::UnrepresentablePath(_))
            ));
        }
    }

    #[test]
    fn the_default_policy_decides_by_type_not_by_size() {
        let policy = TypeFirstBackupRepresentationPolicy;

        for (path, expected) in [
            ("notes/a.md", BackupRepresentationKind::Git),
            ("notes/a.txt", BackupRepresentationKind::Git),
            ("data/photo.jpg", BackupRepresentationKind::Lfs),
            ("data/PHOTO.JPEG", BackupRepresentationKind::Lfs),
            ("data/archive.7z", BackupRepresentationKind::Lfs),
            ("notes/no-extension", BackupRepresentationKind::Git),
            ("notes/.gitattributes", BackupRepresentationKind::Git),
        ] {
            let path = ContentPath::new(path).unwrap();
            assert_eq!(
                policy.choose(&path, 20 * 1024 * 1024, None),
                expected,
                "{path}"
            );
            assert_eq!(
                policy.choose(&path, 10, None),
                expected,
                "a small or large file keeps its representation: {path}"
            );
        }
    }

    #[test]
    fn the_managed_git_attributes_are_deterministic_and_cover_every_lfs_extension() {
        let policy = TypeFirstBackupRepresentationPolicy;

        let first = String::from_utf8(policy.git_attributes()).unwrap();
        let second = String::from_utf8(policy.git_attributes()).unwrap();

        assert_eq!(first, second);
        assert!(first.contains("* -text\n"));
        for extension in TypeFirstBackupRepresentationPolicy::LFS_EXTENSIONS {
            assert!(
                first.contains(&format!(
                    "*.{extension} filter=lfs diff=lfs merge=lfs -text\n"
                )),
                "{extension} is missing from the managed attributes"
            );
        }
        assert!(first.ends_with('\n'));
    }
}
