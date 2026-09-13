use std::{error::Error, fmt};

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::domain::{ContentPath, Sha256};

use super::{SourceIdentity, SourceRevision};

/// Version of the inventory-identity encoding.
pub const SOURCE_INVENTORY_IDENTITY_VERSION: u8 = 1;

/// One object an inventory observed, as the remote namespace described it.
///
/// It carries exactly what a remote listing can prove about one logical file: the
/// canonical vault-relative path it maps to, the opaque remote revision that was
/// observed, and the size the remote reported. It deliberately carries no content
/// identity — nothing here has been read yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceInventoryEntry {
    path: ContentPath,
    revision: SourceRevision,
    reported_size: u64,
}

impl SourceInventoryEntry {
    pub fn new(path: ContentPath, revision: SourceRevision, reported_size: u64) -> Self {
        Self {
            path,
            revision,
            reported_size,
        }
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }

    /// The size the remote reported, which is not yet evidence about any bytes.
    pub fn reported_size(&self) -> u64 {
        self.reported_size
    }
}

/// One complete, canonical listing of a source namespace.
///
/// An inventory is a fact about what the remote said, at the moment it said it. It
/// is sorted by [`ContentPath`], a path may appear once, and its identity covers
/// every entry — so two listings that observed the same namespace in a different
/// order produce one identity, and any changed path, revision or reported size
/// produces a different one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceInventory {
    source: SourceIdentity,
    entries: Vec<SourceInventoryEntry>,
}

impl SourceInventory {
    /// Builds a canonical inventory, refusing a namespace that lists one path twice.
    pub fn new(
        source: SourceIdentity,
        entries: impl IntoIterator<Item = SourceInventoryEntry>,
    ) -> Result<Self, SourceInventoryError> {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(duplicate) = entries
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
            .map(|pair| pair[0].path.clone())
        {
            return Err(SourceInventoryError::DuplicatePath(duplicate));
        }
        Ok(Self { source, entries })
    }

    pub fn source(&self) -> SourceIdentity {
        self.source
    }

    pub fn entries(&self) -> &[SourceInventoryEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The identity of exactly this observation.
    ///
    /// It is an inventory fact, not a content identity: it says "the remote
    /// namespace looked exactly like this", which is what stabilization compares.
    pub fn identity(&self) -> SourceInventoryIdentity {
        let mut hasher = Sha256Hasher::new();
        hasher.update(b"mineral.source-inventory");
        hasher.update([SOURCE_INVENTORY_IDENTITY_VERSION]);
        hasher.update(self.source.as_sha256().as_bytes());
        for entry in &self.entries {
            hasher.update(
                u64::try_from(entry.path.as_str().len())
                    .unwrap_or(u64::MAX)
                    .to_be_bytes(),
            );
            hasher.update(entry.path.as_str().as_bytes());
            hasher.update(
                u64::try_from(entry.revision.encoded().len())
                    .unwrap_or(u64::MAX)
                    .to_be_bytes(),
            );
            hasher.update(entry.revision.encoded().as_bytes());
            hasher.update(entry.reported_size.to_be_bytes());
        }
        SourceInventoryIdentity(Sha256::new(hasher.finalize().into()))
    }
}

/// Deterministic identity of one observed inventory.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceInventoryIdentity(Sha256);

impl SourceInventoryIdentity {
    pub fn as_sha256(self) -> Sha256 {
        self.0
    }
}

impl fmt::Display for SourceInventoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Why a listing cannot be used as an inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceInventoryError {
    /// Two remote objects mapped to the same logical path, so the listing does not
    /// describe a set of files.
    DuplicatePath(ContentPath),
}

impl fmt::Display for SourceInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePath(path) => {
                write!(
                    formatter,
                    "source listing maps more than one object to {path}"
                )
            }
        }
    }
}

impl Error for SourceInventoryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;

    fn source() -> SourceIdentity {
        SourceIdentity::of(SourceKind::ObjectStore, "bucket/vault/").unwrap()
    }

    fn entry(path: &str, revision: &str, size: u64) -> SourceInventoryEntry {
        SourceInventoryEntry::new(
            ContentPath::new(path).unwrap(),
            SourceRevision::versioned(revision).unwrap(),
            size,
        )
    }

    #[test]
    fn listing_order_does_not_change_the_inventory_identity() {
        let first =
            SourceInventory::new(source(), [entry("b.md", "r1", 2), entry("a.md", "r2", 1)])
                .unwrap();
        let second =
            SourceInventory::new(source(), [entry("a.md", "r2", 1), entry("b.md", "r1", 2)])
                .unwrap();

        assert_eq!(first.entries()[0].path().as_str(), "a.md");
        assert_eq!(first, second);
        assert_eq!(first.identity(), second.identity());
    }

    #[test]
    fn the_inventory_identity_covers_path_revision_and_size() {
        let base = SourceInventory::new(source(), [entry("a.md", "r1", 1)]).unwrap();
        let other_path = SourceInventory::new(source(), [entry("b.md", "r1", 1)]).unwrap();
        let other_revision = SourceInventory::new(source(), [entry("a.md", "r2", 1)]).unwrap();
        let other_size = SourceInventory::new(source(), [entry("a.md", "r1", 2)]).unwrap();
        let other_source = SourceInventory::new(
            SourceIdentity::of(SourceKind::ObjectStore, "b/v/").unwrap(),
            [entry("a.md", "r1", 1)],
        )
        .unwrap();

        assert_ne!(base.identity(), other_path.identity());
        assert_ne!(base.identity(), other_revision.identity());
        assert_ne!(base.identity(), other_size.identity());
        assert_ne!(base.identity(), other_source.identity());
    }

    #[test]
    fn two_objects_for_one_logical_path_fail_closed() {
        let result =
            SourceInventory::new(source(), [entry("a.md", "r1", 1), entry("a.md", "r2", 2)]);

        assert_eq!(
            result.unwrap_err(),
            SourceInventoryError::DuplicatePath(ContentPath::new("a.md").unwrap())
        );
    }
}
