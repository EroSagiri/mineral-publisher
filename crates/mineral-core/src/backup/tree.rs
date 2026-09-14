use std::{error::Error, fmt};

use crate::{
    domain::{ContentPath, Sha256, SnapshotId},
    publication::git::{GitCommitOid, GitCommitSpec, GitTreeOid, LocalCommitState},
};

use super::{
    delivery::{
        BACKUP_GIT_ATTRIBUTES_PATH, BACKUP_MANIFEST_PATH, BackupDeliveryError,
        BackupDeliveryProjection,
    },
    projection::BackupProjection,
};

/// One blob the backup tree holds at one path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupTreeEntry {
    path: ContentPath,
    blob_sha256: Sha256,
}

impl BackupTreeEntry {
    pub fn new(path: ContentPath, blob_sha256: Sha256) -> Self {
        Self { path, blob_sha256 }
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    /// The blob the Git tree must hold: original bytes for text, pointer bytes for
    /// an LFS path.
    pub fn blob_sha256(&self) -> Sha256 {
        self.blob_sha256
    }
}

/// The complete tree one backup commit holds.
///
/// Every Snapshot path is shifted under `vault/` so relative references inside the
/// vault stay valid; the managed `.gitattributes` and the manifest live beside it at
/// the repository root. The tree holds only small blobs: original text bytes and
/// pointer bytes, never a large binary payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupTreeProjection {
    snapshot_id: SnapshotId,
    delivery_sha256: Sha256,
    source_projection_sha256: Sha256,
    files: Vec<BackupTreeEntry>,
    manifest: BackupTreeEntry,
    git_attributes: BackupTreeEntry,
}

impl BackupTreeProjection {
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn delivery_sha256(&self) -> Sha256 {
        self.delivery_sha256
    }

    pub fn source_projection_sha256(&self) -> Sha256 {
        self.source_projection_sha256
    }

    /// The `vault/**` entries, in canonical order.
    pub fn files(&self) -> &[BackupTreeEntry] {
        &self.files
    }

    pub fn manifest(&self) -> &BackupTreeEntry {
        &self.manifest
    }

    pub fn git_attributes(&self) -> &BackupTreeEntry {
        &self.git_attributes
    }

    /// Every entry the tree holds, managed files first, then the two metadata files.
    pub fn entries(&self) -> impl Iterator<Item = &BackupTreeEntry> {
        self.files
            .iter()
            .chain([&self.manifest, &self.git_attributes])
    }

    pub fn len(&self) -> usize {
        self.files.len() + 2
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Builds the tree one backup delivery materializes.
///
/// It is a pure mapping from the delivery: text paths hold the Snapshot's own blob,
/// binary paths hold the pointer blob, and the two metadata blobs sit at the root.
/// No byte is read here, and no source blob is touched.
pub fn build_backup_tree(
    delivery: &BackupDeliveryProjection,
) -> Result<BackupTreeProjection, BackupDeliveryError> {
    let files = delivery
        .files()
        .iter()
        .map(|file| {
            BackupTreeEntry::new(
                BackupProjection::tree_path(file.path()),
                file.representation().tree_blob_sha256(),
            )
        })
        .collect::<Vec<_>>();

    Ok(BackupTreeProjection {
        snapshot_id: delivery.snapshot_id(),
        delivery_sha256: delivery.delivery_sha256(),
        source_projection_sha256: delivery.source_projection_sha256(),
        files,
        manifest: BackupTreeEntry::new(
            ContentPath::new(BACKUP_MANIFEST_PATH).expect("the manifest path is canonical"),
            delivery.manifest_blob_sha256(),
        ),
        git_attributes: BackupTreeEntry::new(
            ContentPath::new(BACKUP_GIT_ATTRIBUTES_PATH).expect("the attributes path is canonical"),
            delivery.git_attributes_blob_sha256(),
        ),
    })
}

/// The exact tree one runtime materialized for one backup commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupReviewedTree {
    base_commit: GitCommitOid,
    base_tree_oid: GitTreeOid,
    tree_oid: GitTreeOid,
    delivery_sha256: Sha256,
    snapshot_id: SnapshotId,
}

impl BackupReviewedTree {
    pub fn from_parts(
        base_commit: GitCommitOid,
        base_tree_oid: GitTreeOid,
        tree_oid: GitTreeOid,
        delivery_sha256: Sha256,
        snapshot_id: SnapshotId,
    ) -> Self {
        Self {
            base_commit,
            base_tree_oid,
            tree_oid,
            delivery_sha256,
            snapshot_id,
        }
    }

    pub fn base_commit(&self) -> &GitCommitOid {
        &self.base_commit
    }

    pub fn base_tree_oid(&self) -> &GitTreeOid {
        &self.base_tree_oid
    }

    pub fn tree_oid(&self) -> &GitTreeOid {
        &self.tree_oid
    }

    pub fn delivery_sha256(&self) -> Sha256 {
        self.delivery_sha256
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn is_noop(&self) -> bool {
        self.tree_oid == self.base_tree_oid
    }
}

impl fmt::Display for BackupReviewedTree {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "backup tree {} on base commit {}",
            self.tree_oid.as_str(),
            self.base_commit.as_str()
        )
    }
}

/// Reads one backup commit back out of a Git object database.
///
/// `backup verify` is a reader, so it gets its own narrow seam: it never writes, and
/// it never interprets the commit beyond the paths and blobs it asks for.
pub trait BackupTreeReader {
    type Error: Error + 'static;

    /// Every path the commit's tree holds.
    fn list_paths(&self, commit: &GitCommitOid) -> Result<Vec<ContentPath>, Self::Error>;

    /// The exact blob bytes at one path, or `None` when the path is absent.
    fn read_blob(
        &self,
        commit: &GitCommitOid,
        path: &ContentPath,
    ) -> Result<Option<Vec<u8>>, Self::Error>;
}

/// The backup-side Git seam.
///
/// It is deliberately narrower than the public `GitRepository`: the only projection
/// it materializes is a [`BackupTreeProjection`], which holds pointer blobs for
/// binary paths. A runtime implements both traits over one object database; the
/// engine never learns how either one is written.
pub trait BackupGitRepository {
    type Error: Error + 'static;

    /// Writes every blob of the tree and reports the exact tree it created.
    fn materialize_backup(
        &self,
        base: &GitCommitOid,
        tree: &BackupTreeProjection,
    ) -> Result<BackupReviewedTree, Self::Error>;

    /// Creates the commit object described by the frozen specification.
    fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error>;

    /// Reports the facts of one commit in the runtime's own object database.
    fn inspect_commit(&self, commit: &GitCommitOid) -> Result<LocalCommitState, Self::Error>;
}

impl<T: BackupGitRepository> BackupGitRepository for &T {
    type Error = T::Error;

    fn materialize_backup(
        &self,
        base: &GitCommitOid,
        tree: &BackupTreeProjection,
    ) -> Result<BackupReviewedTree, Self::Error> {
        (**self).materialize_backup(base, tree)
    }

    fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
        (**self).create_commit(spec)
    }

    fn inspect_commit(&self, commit: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
        (**self).inspect_commit(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backup::{
            delivery::build_backup_delivery, projection::TypeFirstBackupRepresentationPolicy,
        },
        domain::{Snapshot, SnapshotFile, SourceId},
        ports::{BlobStore, ContentStoreError},
    };
    use std::{cell::RefCell, collections::HashMap, time::SystemTime};

    #[derive(Default)]
    struct MemoryStore(RefCell<HashMap<Sha256, Vec<u8>>>);

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            let identity = Sha256::digest(content);
            self.0.borrow_mut().insert(identity, content.to_vec());
            Ok(identity)
        }
    }

    fn tree() -> BackupTreeProjection {
        let store = MemoryStore::default();
        let snapshot = Snapshot::new(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("vault").unwrap(),
            vec![
                SnapshotFile::new(
                    ContentPath::new("notes/a.md").unwrap(),
                    5,
                    Sha256::digest(b"note\n"),
                    None,
                ),
                SnapshotFile::new(
                    ContentPath::new("attachments/a.jpg").unwrap(),
                    3,
                    Sha256::digest(b"jpg"),
                    None,
                ),
            ],
        )
        .unwrap();
        let projection = BackupProjection::build(&snapshot).unwrap();
        let delivery =
            build_backup_delivery(&projection, &TypeFirstBackupRepresentationPolicy, &store)
                .unwrap();
        build_backup_tree(&delivery).unwrap()
    }

    #[test]
    fn every_path_is_shifted_under_the_vault_and_binary_paths_hold_pointers() {
        let tree = tree();

        let paths = tree
            .files()
            .iter()
            .map(|entry| entry.path().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["vault/attachments/a.jpg", "vault/notes/a.md"],
            "relative paths inside the vault stay valid"
        );
        assert_eq!(
            tree.files()[1].blob_sha256(),
            Sha256::digest(b"note\n"),
            "a text path holds the original bytes"
        );
        assert_ne!(
            tree.files()[0].blob_sha256(),
            Sha256::digest(b"jpg"),
            "a binary path holds pointer bytes, not the payload"
        );
        assert_eq!(tree.manifest().path().as_str(), BACKUP_MANIFEST_PATH);
        assert_eq!(
            tree.git_attributes().path().as_str(),
            BACKUP_GIT_ATTRIBUTES_PATH
        );
        assert_eq!(tree.len(), 4);
    }

    #[test]
    fn the_tree_is_deterministic_for_one_delivery() {
        assert_eq!(tree(), tree());
    }
}
