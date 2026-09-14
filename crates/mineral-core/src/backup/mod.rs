//! The private backup pipeline: a byte-faithful restorable copy of one Snapshot.
//!
//! It shares Source, Snapshot, CAS and the Git infrastructure with the public
//! pipeline and forks from Projection onwards. Nothing here consults public policy,
//! privacy, review, sanitization or URL rewriting: a private file, an unpublished
//! file and an original photo with its EXIF all belong in a backup.
//!
//! Three invariants are the reason this module exists in the shape it has:
//!
//! 1. `LFS oid == Snapshot/CAS SHA-256`: the pointer stored in the tree names the
//!    Snapshot's own bytes, so Git LFS restores exactly them.
//! 2. `LFS size == Snapshot file size`: identity and size are frozen together in the
//!    delivery projection and the durable intent.
//! 3. A Git backup ref only ever moves after every required LFS object is confirmed
//!    present: a commit whose LFS objects are missing must never look like a backup.

mod delivery;
mod execute;
mod lfs;
mod manifest;
mod projection;
mod run;
mod tree;
mod wire;

pub use delivery::{
    BACKUP_DELIVERY_VERSION, BACKUP_GIT_ATTRIBUTES_PATH, BACKUP_MANIFEST_PATH, BackupDeliveryError,
    BackupDeliveryFile, BackupDeliveryProjection, BackupManifest, BackupManifestEntry,
    BackupManifestError, BackupRepresentation, BackupRepresentationKind, build_backup_delivery,
    render_manifest, verify_manifest_pointer,
};
pub use execute::{
    BackupExecutionError, BackupExecutionOutcome, BackupVerificationError,
    BackupVerificationReport, execute_backup, verify_backup,
};
pub use lfs::{
    LFS_POINTER_VERSION, LfsPointer, LfsPointerError, LfsRemote, LfsUpload, LfsUploadPlan,
    RequiredLfsObject, build_lfs_pointer, parse_lfs_pointer,
};
pub use manifest::{
    BACKUP_MANIFEST_FORMAT_VERSION, ManifestPathMismatch, check_manifest_paths, manifest_entry,
};
pub use projection::{
    BACKUP_PROJECTION_VERSION, BACKUP_VAULT_ROOT, BackupProjection, BackupProjectionError,
    BackupProjectionFile, BackupRepresentationPolicy, TypeFirstBackupRepresentationPolicy,
    backup_managed_root,
};
pub use run::{BackupRun, BackupRunError, BackupRunId, BackupRunIdError, BackupRunStore};
pub use tree::{
    BackupGitRepository, BackupReviewedTree, BackupTreeEntry, BackupTreeProjection,
    BackupTreeReader, build_backup_tree,
};
pub use wire::{BACKUP_DELIVERY_WIRE_VERSION, BackupDeliveryWire, BackupDeliveryWireError};
