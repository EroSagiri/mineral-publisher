use std::{error::Error, fmt};

use crate::domain::{ContentPath, Sha256};

use super::{SourceIdentity, SourceInventory, SourceInventoryEntry, SourceRevision};

/// One durable fact: this engine read exactly this remote revision, hashed the
/// bytes it actually received, and materialized them into the content-addressed
/// store under that identity.
///
/// It is the only thing that authorizes skipping a download on a later run, and it
/// says nothing about bytes that were not read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMaterialization {
    source: SourceIdentity,
    path: ContentPath,
    revision: SourceRevision,
    content_sha256: Sha256,
    content_size: u64,
}

impl SourceMaterialization {
    pub fn new(
        source: SourceIdentity,
        path: ContentPath,
        revision: SourceRevision,
        content_sha256: Sha256,
        content_size: u64,
    ) -> Self {
        Self {
            source,
            path,
            revision,
            content_sha256,
            content_size,
        }
    }

    pub fn source(&self) -> SourceIdentity {
        self.source
    }

    pub fn path(&self) -> &ContentPath {
        &self.path
    }

    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }

    /// The identity of the bytes this engine hashed while reading.
    pub fn content_sha256(&self) -> Sha256 {
        self.content_sha256
    }

    /// How many bytes were hashed and stored.
    pub fn content_size(&self) -> u64 {
        self.content_size
    }
}

/// The runtime facts one refresh decision is allowed to rest on.
///
/// A runtime answers two questions about durable facts it owns; the engine decides
/// what to do with the answers. Nothing here is allowed to consult the remote
/// namespace, and nothing here may report a remote validator as if it were a
/// content identity.
pub trait SourceRefreshFacts {
    type Error: Error + 'static;

    /// The materialization recorded for exactly this revision, if any.
    ///
    /// A revision that was never read, or a different revision of the same path,
    /// must answer `None` rather than a neighbouring fact.
    fn materialization(
        &self,
        source: SourceIdentity,
        path: &ContentPath,
        revision: &SourceRevision,
    ) -> Result<Option<SourceMaterialization>, Self::Error>;

    /// The size of a stored content blob, or `None` when it is absent.
    ///
    /// This is the integrity question the engine asks about its own store before it
    /// trusts a durable binding: a binding whose bytes are gone, or whose stored
    /// size contradicts the binding, proves nothing.
    fn stored_blob_size(&self, identity: Sha256) -> Result<Option<u64>, Self::Error>;
}

/// What one refresh attempt will do with one inventory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceRefreshStep {
    /// The remote revision is already materialized, and the blob that holds it is
    /// still present with the recorded size.
    Reuse {
        entry: SourceInventoryEntry,
        content_sha256: Sha256,
        content_size: u64,
    },
    /// Nothing durable proves these bytes, so they must be read exactly.
    Fetch { entry: SourceInventoryEntry },
}

impl SourceRefreshStep {
    pub fn entry(&self) -> &SourceInventoryEntry {
        match self {
            Self::Reuse { entry, .. } | Self::Fetch { entry } => entry,
        }
    }

    pub fn path(&self) -> &ContentPath {
        self.entry().path()
    }
}

/// The decision for one complete inventory: which entries to re-read, and which
/// already-proven facts to reuse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRefreshPlan {
    source: SourceIdentity,
    steps: Vec<SourceRefreshStep>,
}

impl SourceRefreshPlan {
    pub fn source(&self) -> SourceIdentity {
        self.source
    }

    pub fn steps(&self) -> &[SourceRefreshStep] {
        &self.steps
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn reused(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, SourceRefreshStep::Reuse { .. }))
            .count()
    }

    pub fn to_fetch(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, SourceRefreshStep::Fetch { .. }))
            .count()
    }

    /// The paths this plan authorizes reading from the remote namespace.
    pub fn fetch_paths(&self) -> Vec<&ContentPath> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                SourceRefreshStep::Fetch { entry } => Some(entry.path()),
                SourceRefreshStep::Reuse { .. } => None,
            })
            .collect()
    }
}

/// Decides what one inventory requires, from durable facts alone.
///
/// The rule is deliberately conservative in one direction and strict in the other:
/// a revision is reused only when a durable binding names that exact revision and
/// the engine's own store still holds a blob of the recorded size under the
/// recorded identity; anything else is fetched again, and a store that contradicts
/// a binding stops the run instead of choosing which side to believe.
pub fn plan_source_refresh<F: SourceRefreshFacts>(
    inventory: &SourceInventory,
    facts: &F,
) -> Result<SourceRefreshPlan, SourceRefreshError<F::Error>> {
    let mut steps = Vec::with_capacity(inventory.entries().len());

    for entry in inventory.entries() {
        let stored = facts
            .materialization(inventory.source(), entry.path(), entry.revision())
            .map_err(SourceRefreshError::Facts)?;

        let Some(materialization) = stored else {
            steps.push(SourceRefreshStep::Fetch {
                entry: entry.clone(),
            });
            continue;
        };

        if materialization.source() != inventory.source()
            || materialization.path() != entry.path()
            || materialization.revision() != entry.revision()
        {
            return Err(SourceRefreshError::MismatchedFact {
                path: entry.path().clone(),
            });
        }

        match facts
            .stored_blob_size(materialization.content_sha256())
            .map_err(SourceRefreshError::Facts)?
        {
            Some(size) if size == materialization.content_size() => {
                steps.push(SourceRefreshStep::Reuse {
                    entry: entry.clone(),
                    content_sha256: materialization.content_sha256(),
                    content_size: materialization.content_size(),
                });
            }
            Some(found_size) => {
                return Err(SourceRefreshError::ConflictingMaterialization {
                    path: entry.path().clone(),
                    revision: entry.revision().clone(),
                    bound_size: materialization.content_size(),
                    stored_size: found_size,
                });
            }
            // The blob is gone: the binding proves nothing about bytes that no
            // longer exist, so the object is read again.
            None => steps.push(SourceRefreshStep::Fetch {
                entry: entry.clone(),
            }),
        }
    }

    Ok(SourceRefreshPlan {
        source: inventory.source(),
        steps,
    })
}

/// Why a refresh plan cannot be trusted.
#[derive(Debug)]
pub enum SourceRefreshError<FactsError> {
    /// A durable-fact lookup failed.
    Facts(FactsError),
    /// The facts answered about a key that was not asked for.
    MismatchedFact { path: ContentPath },
    /// A durable binding and the store it points at disagree about the bytes.
    ConflictingMaterialization {
        path: ContentPath,
        revision: SourceRevision,
        bound_size: u64,
        stored_size: u64,
    },
}

impl<FactsError: fmt::Display> fmt::Display for SourceRefreshError<FactsError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Facts(error) => write!(formatter, "could not read source facts: {error}"),
            Self::MismatchedFact { path } => {
                write!(
                    formatter,
                    "source facts answered about a different key than {path}"
                )
            }
            Self::ConflictingMaterialization {
                path,
                revision,
                bound_size,
                stored_size,
            } => write!(
                formatter,
                "materialization of {path} at revision {revision} records {bound_size} bytes \
                 but the content store holds {stored_size}"
            ),
        }
    }
}

impl<FactsError: Error + 'static> Error for SourceRefreshError<FactsError> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Facts(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;
    use crate::{
        domain::Sha256,
        source::{SourceInventoryEntry, SourceKind},
    };

    #[derive(Default)]
    struct Facts {
        materializations: BTreeMap<(String, String), SourceMaterialization>,
        blobs: HashMap<Sha256, u64>,
    }

    impl SourceRefreshFacts for Facts {
        type Error = std::convert::Infallible;

        fn materialization(
            &self,
            _: SourceIdentity,
            path: &ContentPath,
            revision: &SourceRevision,
        ) -> Result<Option<SourceMaterialization>, Self::Error> {
            Ok(self
                .materializations
                .get(&(path.as_str().to_owned(), revision.encoded().to_owned()))
                .cloned())
        }

        fn stored_blob_size(&self, identity: Sha256) -> Result<Option<u64>, Self::Error> {
            Ok(self.blobs.get(&identity).copied())
        }
    }

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

    fn bound(
        path: &str,
        revision: &str,
        identity: Sha256,
        size: u64,
    ) -> ((String, String), SourceMaterialization) {
        (
            (path.to_owned(), format!("v1:{revision}")),
            SourceMaterialization::new(
                source(),
                ContentPath::new(path).unwrap(),
                SourceRevision::versioned(revision).unwrap(),
                identity,
                size,
            ),
        )
    }

    #[test]
    fn an_entry_without_a_binding_is_fetched() {
        let facts = Facts::default();

        let plan = plan_source_refresh(&inventory(&[("a.md", "r1", 3)]), &facts).unwrap();

        assert_eq!(plan.to_fetch(), 1);
        assert_eq!(plan.reused(), 0);
        assert_eq!(plan.fetch_paths(), [&ContentPath::new("a.md").unwrap()]);
    }

    #[test]
    fn a_revision_whose_blob_is_still_present_is_reused() {
        let identity = Sha256::digest(b"abc");
        let mut facts = Facts::default();
        let (key, materialization) = bound("a.md", "r1", identity, 3);
        facts.materializations.insert(key, materialization);
        facts.blobs.insert(identity, 3);

        let plan = plan_source_refresh(&inventory(&[("a.md", "r1", 3)]), &facts).unwrap();

        assert_eq!(plan.reused(), 1);
        assert_eq!(plan.to_fetch(), 0);
        assert!(matches!(
            &plan.steps()[0],
            SourceRefreshStep::Reuse { content_sha256, content_size: 3, .. } if *content_sha256 == identity
        ));
    }

    #[test]
    fn a_changed_revision_is_fetched_again() {
        let identity = Sha256::digest(b"abc");
        let mut facts = Facts::default();
        let (key, materialization) = bound("a.md", "r1", identity, 3);
        facts.materializations.insert(key, materialization);
        facts.blobs.insert(identity, 3);

        let plan = plan_source_refresh(&inventory(&[("a.md", "r2", 3)]), &facts).unwrap();

        assert_eq!(plan.to_fetch(), 1);
        assert_eq!(plan.reused(), 0);
    }

    #[test]
    fn a_binding_whose_blob_disappeared_is_fetched_again() {
        let identity = Sha256::digest(b"abc");
        let mut facts = Facts::default();
        let (key, materialization) = bound("a.md", "r1", identity, 3);
        facts.materializations.insert(key, materialization);

        let plan = plan_source_refresh(&inventory(&[("a.md", "r1", 3)]), &facts).unwrap();

        assert_eq!(plan.to_fetch(), 1);
        assert_eq!(plan.reused(), 0);
    }

    #[test]
    fn a_binding_that_contradicts_the_store_fails_closed() {
        let identity = Sha256::digest(b"abc");
        let mut facts = Facts::default();
        let (key, materialization) = bound("a.md", "r1", identity, 3);
        facts.materializations.insert(key, materialization);
        facts.blobs.insert(identity, 99);

        let error = plan_source_refresh(&inventory(&[("a.md", "r1", 3)]), &facts).unwrap_err();

        assert!(matches!(
            error,
            SourceRefreshError::ConflictingMaterialization {
                bound_size: 3,
                stored_size: 99,
                ..
            }
        ));
    }

    #[test]
    fn a_zero_byte_materialization_is_a_real_fact() {
        let identity = Sha256::digest(b"");
        let mut facts = Facts::default();
        let (key, materialization) = bound("empty.md", "r1", identity, 0);
        facts.materializations.insert(key, materialization);
        facts.blobs.insert(identity, 0);

        let plan = plan_source_refresh(&inventory(&[("empty.md", "r1", 0)]), &facts).unwrap();

        assert_eq!(plan.reused(), 1);
        assert_eq!(plan.to_fetch(), 0);
    }
}
