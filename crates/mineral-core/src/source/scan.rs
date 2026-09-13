use std::{convert::Infallible, error::Error, fmt, time::SystemTime};

use crate::domain::{
    ContentPath, Sha256, Snapshot, SnapshotError, SnapshotFile, SnapshotId, SourceId,
};

use super::SourceInventory;

/// How many times one attempt re-reads the namespace before giving up.
///
/// A remote namespace other writers can change is never going to hold still on
/// demand, so the scan gets a small, fixed budget instead of an unbounded loop.
pub const DEFAULT_MAX_SCAN_ATTEMPTS: usize = 3;

/// The runtime seam one stabilized scan needs.
///
/// A runtime answers two questions: "what is there right now?" and "materialize
/// exactly this". It does not decide when a scan is stable, and it does not decide
/// which entries may be reused — the engine owns both rules.
pub trait SourceScan {
    type Error: Error + 'static;

    /// Reads one complete, canonical inventory of the namespace.
    fn inventory(&self) -> Result<SourceInventory, Self::Error>;

    /// Materializes every entry of exactly this inventory into the content store.
    ///
    /// Reuse decisions come from the engine's own refresh rule; a runtime that
    /// fetches more than the plan asks for is not implementing this contract.
    fn materialize(
        &self,
        inventory: &SourceInventory,
    ) -> Result<SourceMaterializationSet, Self::Error>;
}

/// One entry whose bytes are proven to be in the content store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMaterializedEntry {
    path: ContentPath,
    content_sha256: Sha256,
    content_size: u64,
}

impl SourceMaterializedEntry {
    pub fn new(path: ContentPath, content_sha256: Sha256, content_size: u64) -> Self {
        Self {
            path,
            content_sha256,
            content_size,
        }
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn content_sha256(&self) -> Sha256 {
        self.content_sha256
    }

    pub fn content_size(&self) -> u64 {
        self.content_size
    }
}

/// Every fact a complete source state needs, in canonical order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMaterializationSet {
    entries: Vec<SourceMaterializedEntry>,
}

impl SourceMaterializationSet {
    pub fn new(
        entries: impl IntoIterator<Item = SourceMaterializedEntry>,
    ) -> Result<Self, SourceMaterializationError> {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(duplicate) = entries
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
            .map(|pair| pair[0].path.clone())
        {
            return Err(SourceMaterializationError::DuplicatePath(duplicate));
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[SourceMaterializedEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn get(&self, path: &ContentPath) -> Option<&SourceMaterializedEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.cmp(path))
            .ok()
            .map(|index| &self.entries[index])
    }
}

/// Why a materialized set cannot describe a source state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceMaterializationError {
    /// One logical path was materialized twice.
    DuplicatePath(ContentPath),
}

impl fmt::Display for SourceMaterializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePath(path) => write!(formatter, "materialized {path} more than once"),
        }
    }
}

impl Error for SourceMaterializationError {}

/// One namespace observation the scan accepted as stable, together with the bytes
/// it materialized for exactly that observation.
///
/// The claim is precise and deliberately weaker than "an atomic snapshot of the
/// bucket": every file was read under its own exact remote revision, and the
/// canonical listing observed before and after materialization was identical. A
/// writer that changes the namespace concurrently can still be observed by the next
/// run; what cannot happen is a file whose bytes were never verified, or a file
/// silently missing from the assembled state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StabilizedSource {
    inventory: SourceInventory,
    materialized: SourceMaterializationSet,
}

impl StabilizedSource {
    pub fn inventory(&self) -> &SourceInventory {
        &self.inventory
    }

    pub fn materialized(&self) -> &SourceMaterializationSet {
        &self.materialized
    }

    /// Assembles the complete source state.
    ///
    /// Paths, sizes and content identities come from the materialized facts, never
    /// from the listing: the remote's reported size is cross-checked during
    /// stabilization and then stops being the authority.
    pub fn snapshot(
        &self,
        id: SnapshotId,
        created_at: SystemTime,
        source_id: SourceId,
    ) -> Result<Snapshot, SnapshotError> {
        let files = self
            .materialized
            .entries()
            .iter()
            .map(|entry| {
                SnapshotFile::new(
                    entry.path().clone(),
                    entry.content_size(),
                    entry.content_sha256(),
                    None,
                )
            })
            .collect::<Vec<_>>();
        Snapshot::new(id, created_at, source_id, files)
    }

    /// The observation this state was accepted for.
    pub fn inventory_identity(&self) -> super::SourceInventoryIdentity {
        self.inventory.identity()
    }
}

/// Stabilizes one scan: inventory, materialize, inventory again, accept only when
/// both observations describe the same namespace.
///
/// A changed namespace is not an error by itself — it means the attempt observed a
/// remote state that no longer exists, so the engine starts over with a new budget.
/// The materializations the failed attempt proved are durable and are reused by the
/// next attempt; nothing correct is thrown away to make the retry look atomic.
pub fn stabilize_scan<S: SourceScan>(
    scan: &S,
    max_attempts: usize,
) -> Result<StabilizedSource, SourceScanError<S::Error>> {
    let attempts = max_attempts.max(1);

    for _ in 0..attempts {
        let before = scan.inventory().map_err(SourceScanError::Scan)?;
        let materialized = scan.materialize(&before).map_err(SourceScanError::Scan)?;
        let after = scan.inventory().map_err(SourceScanError::Scan)?;

        if before.identity() != after.identity() {
            continue;
        }

        validate_materialization(&before, &materialized)?;
        return Ok(StabilizedSource {
            inventory: before,
            materialized,
        });
    }

    Err(SourceScanError::Unstable { attempts })
}

fn validate_materialization<E>(
    inventory: &SourceInventory,
    materialized: &SourceMaterializationSet,
) -> Result<(), SourceScanError<E>> {
    for fact in materialized.entries() {
        if !inventory
            .entries()
            .iter()
            .any(|entry| entry.path() == fact.path())
        {
            return Err(SourceScanError::UnknownMaterialization(fact.path().clone()));
        }
    }
    for entry in inventory.entries() {
        let Some(fact) = materialized.get(entry.path()) else {
            return Err(SourceScanError::MissingMaterialization(
                entry.path().clone(),
            ));
        };
        if fact.content_size() != entry.reported_size() {
            return Err(SourceScanError::SizeMismatch {
                path: entry.path().clone(),
                reported_size: entry.reported_size(),
                materialized_size: fact.content_size(),
            });
        }
    }
    Ok(())
}

/// Why a scan did not produce a source state.
#[derive(Debug)]
pub enum SourceScanError<ScanError> {
    /// The runtime could not read or materialize the namespace.
    Scan(ScanError),
    /// The namespace kept changing, so no observation could be accepted.
    Unstable { attempts: usize },
    /// The runtime materialized a path the inventory never listed.
    UnknownMaterialization(ContentPath),
    /// The runtime did not materialize a path the inventory listed.
    MissingMaterialization(ContentPath),
    /// The bytes the runtime materialized are not the size the remote reported.
    SizeMismatch {
        path: ContentPath,
        reported_size: u64,
        materialized_size: u64,
    },
    /// One logical path was materialized twice.
    DuplicateMaterialization(ContentPath),
}

impl<ScanError: fmt::Display> fmt::Display for SourceScanError<ScanError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan(error) => write!(formatter, "could not scan the source: {error}"),
            Self::Unstable { attempts } => write!(
                formatter,
                "source namespace changed during {attempts} consecutive scans"
            ),
            Self::UnknownMaterialization(path) => {
                write!(formatter, "source scan materialized unlisted path {path}")
            }
            Self::MissingMaterialization(path) => {
                write!(formatter, "source scan left {path} unverified")
            }
            Self::SizeMismatch {
                path,
                reported_size,
                materialized_size,
            } => write!(
                formatter,
                "source scan read {materialized_size} bytes for {path} but the remote reported \
                 {reported_size}"
            ),
            Self::DuplicateMaterialization(path) => {
                write!(formatter, "source scan materialized {path} more than once")
            }
        }
    }
}

impl<ScanError: Error + 'static> Error for SourceScanError<ScanError> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Infallible> for SourceScanError<Infallible> {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use super::*;
    use crate::source::{SourceIdentity, SourceInventoryEntry, SourceKind, SourceRevision};

    fn source() -> SourceIdentity {
        SourceIdentity::of(SourceKind::ObjectStore, "bucket/vault/").unwrap()
    }

    fn inventory(entries: &[(&str, &str, u64)]) -> SourceInventory {
        SourceInventory::new(
            source(),
            entries.iter().map(|(path, revision, size)| {
                SourceInventoryEntry::new(
                    ContentPath::new(*path).unwrap(),
                    SourceRevision::versioned(*revision).unwrap(),
                    *size,
                )
            }),
        )
        .unwrap()
    }

    /// A scan whose observations are scripted, and whose materializations are
    /// durable across attempts the way a real one's bindings are.
    struct FakeScan {
        observations: RefCell<VecDeque<SourceInventory>>,
        last: RefCell<Option<SourceInventory>>,
        durable: RefCell<std::collections::BTreeMap<(String, String), Sha256>>,
        fetches: RefCell<Vec<String>>,
        reuses: RefCell<Vec<String>>,
    }

    impl FakeScan {
        fn new(observations: Vec<SourceInventory>) -> Self {
            Self {
                observations: RefCell::new(observations.into()),
                last: RefCell::new(None),
                durable: RefCell::new(std::collections::BTreeMap::new()),
                fetches: RefCell::new(Vec::new()),
                reuses: RefCell::new(Vec::new()),
            }
        }

        fn fetches(&self) -> Vec<String> {
            self.fetches.borrow().clone()
        }

        fn reuses(&self) -> Vec<String> {
            self.reuses.borrow().clone()
        }
    }

    impl SourceScan for FakeScan {
        type Error = Infallible;

        fn inventory(&self) -> Result<SourceInventory, Self::Error> {
            let next = self.observations.borrow_mut().pop_front();
            match next {
                Some(inventory) => {
                    *self.last.borrow_mut() = Some(inventory.clone());
                    Ok(inventory)
                }
                None => Ok(self
                    .last
                    .borrow()
                    .clone()
                    .expect("a scripted scan always has an observation")),
            }
        }

        fn materialize(
            &self,
            inventory: &SourceInventory,
        ) -> Result<SourceMaterializationSet, Self::Error> {
            let mut entries = Vec::new();
            for entry in inventory.entries() {
                let key = (
                    entry.path().as_str().to_owned(),
                    entry.revision().encoded().to_owned(),
                );
                let known = self.durable.borrow().get(&key).copied();
                let identity = match known {
                    Some(identity) => {
                        self.reuses.borrow_mut().push(key.0.clone());
                        identity
                    }
                    None => {
                        let identity = Sha256::digest(key.0.as_bytes());
                        self.durable.borrow_mut().insert(key, identity);
                        self.fetches
                            .borrow_mut()
                            .push(entry.path().as_str().to_owned());
                        identity
                    }
                };
                entries.push(SourceMaterializedEntry::new(
                    entry.path().clone(),
                    identity,
                    entry.reported_size(),
                ));
            }
            Ok(SourceMaterializationSet::new(entries).unwrap())
        }
    }

    #[test]
    fn a_stable_scan_is_accepted() {
        let scan = FakeScan::new(vec![inventory(&[("a.md", "r1", 3)])]);

        let stabilized = stabilize_scan(&scan, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();

        assert_eq!(stabilized.materialized().len(), 1);
        assert_eq!(scan.fetches(), ["a.md"]);
        assert!(scan.reuses().is_empty());
    }

    #[test]
    fn a_scan_that_changes_and_then_holds_still_is_accepted_and_reuses_its_work() {
        let first = inventory(&[("a.md", "r1", 3)]);
        let second = inventory(&[("a.md", "r1", 3), ("b.md", "r1", 1)]);
        let scan = FakeScan::new(vec![first, second.clone(), second]);

        let stabilized = stabilize_scan(&scan, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();

        assert_eq!(stabilized.inventory().len(), 2);
        assert_eq!(scan.fetches(), ["a.md", "b.md"]);
        assert_eq!(
            scan.reuses(),
            ["a.md"],
            "the second attempt must reuse what the first one already proved"
        );
    }

    #[test]
    fn a_scan_that_keeps_changing_is_refused() {
        let scan = FakeScan::new(vec![
            inventory(&[("a.md", "r1", 1)]),
            inventory(&[("a.md", "r2", 1)]),
            inventory(&[("a.md", "r3", 1)]),
            inventory(&[("a.md", "r4", 1)]),
            inventory(&[("a.md", "r5", 1)]),
            inventory(&[("a.md", "r6", 1)]),
        ]);

        let error = stabilize_scan(&scan, 3).unwrap_err();

        assert!(matches!(error, SourceScanError::Unstable { attempts: 3 }));
    }

    #[test]
    fn a_bounded_budget_is_never_zero() {
        let scan = FakeScan::new(vec![inventory(&[])]);

        assert!(stabilize_scan(&scan, 0).is_ok());
    }

    #[test]
    fn a_missing_or_extra_materialization_fails_closed() {
        struct WrongScan(SourceMaterializationSet);

        impl SourceScan for WrongScan {
            type Error = Infallible;

            fn inventory(&self) -> Result<SourceInventory, Self::Error> {
                Ok(inventory(&[("a.md", "r1", 3)]))
            }

            fn materialize(
                &self,
                _: &SourceInventory,
            ) -> Result<SourceMaterializationSet, Self::Error> {
                Ok(self.0.clone())
            }
        }

        let missing = WrongScan(SourceMaterializationSet::new([]).unwrap());
        assert!(matches!(
            stabilize_scan(&missing, 1),
            Err(SourceScanError::MissingMaterialization(path)) if path.as_str() == "a.md"
        ));

        let unknown = WrongScan(
            SourceMaterializationSet::new([SourceMaterializedEntry::new(
                ContentPath::new("other.md").unwrap(),
                Sha256::digest(b"x"),
                3,
            )])
            .unwrap(),
        );
        assert!(matches!(
            stabilize_scan(&unknown, 1),
            Err(SourceScanError::UnknownMaterialization(path)) if path.as_str() == "other.md"
        ));

        let wrong_size = WrongScan(
            SourceMaterializationSet::new([SourceMaterializedEntry::new(
                ContentPath::new("a.md").unwrap(),
                Sha256::digest(b"x"),
                4,
            )])
            .unwrap(),
        );
        assert!(matches!(
            stabilize_scan(&wrong_size, 1),
            Err(SourceScanError::SizeMismatch {
                reported_size: 3,
                materialized_size: 4,
                ..
            })
        ));
    }

    #[test]
    fn the_assembled_snapshot_comes_from_materialized_bytes() {
        let scan = FakeScan::new(vec![inventory(&[("a.md", "r1", 3)])]);
        let stabilized = stabilize_scan(&scan, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();

        let snapshot = stabilized
            .snapshot(
                SnapshotId::new(1).unwrap(),
                SystemTime::UNIX_EPOCH,
                SourceId::new("r2-vault").unwrap(),
            )
            .unwrap();

        assert_eq!(snapshot.files().len(), 1);
        assert_eq!(snapshot.files()[0].path().as_str(), "a.md");
        assert_eq!(snapshot.files()[0].size(), 3);
        assert_eq!(snapshot.files()[0].sha256(), Sha256::digest(b"a.md"));
        assert_eq!(snapshot.files()[0].content_type(), None);
        // A remote listing never becomes a content identity on its own.
        assert_ne!(
            snapshot.files()[0].sha256(),
            Sha256::digest(
                SourceRevision::versioned("r1")
                    .unwrap()
                    .encoded()
                    .as_bytes()
            )
        );
    }
}
