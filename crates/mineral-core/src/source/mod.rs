//! Portable source-domain facts: what a remote namespace contained, which revision
//! of each file was observed, and what this engine proved about the bytes it read.
//!
//! Nothing here knows how a namespace is reached. An adapter lists it, reads it and
//! materializes it; the engine owns the rules that decide when an observation may be
//! accepted, when a durable binding may skip a download, and how a complete source
//! state is assembled. In particular a remote validator (an ETag, an object version,
//! an upload time) is only ever an opaque [`SourceRevision`] here — it is never a
//! content identity, and nothing in this module can turn one into a
//! [`crate::domain::Sha256`].

mod identity;
mod inventory;
mod refresh;
mod revision;
mod scan;

pub use identity::{SOURCE_IDENTITY_VERSION, SourceIdentity, SourceIdentityError, SourceKind};
pub use inventory::{
    SOURCE_INVENTORY_IDENTITY_VERSION, SourceInventory, SourceInventoryEntry, SourceInventoryError,
    SourceInventoryIdentity,
};
pub use refresh::{
    SourceMaterialization, SourceRefreshError, SourceRefreshFacts, SourceRefreshPlan,
    SourceRefreshStep, plan_source_refresh,
};
pub use revision::{
    MAX_SOURCE_REVISION_LENGTH, SOURCE_REVISION_VERSION, SourceRevision, SourceRevisionError,
};
pub use scan::{
    DEFAULT_MAX_SCAN_ATTEMPTS, SourceMaterializationError, SourceMaterializationSet,
    SourceMaterializedEntry, SourceScan, SourceScanError, StabilizedSource, stabilize_scan,
};
