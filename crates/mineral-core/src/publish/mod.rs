//! Portable publication identities and the publication intent model shared by
//! every runtime.
//!
//! Nothing here knows about Git, the filesystem, or any other concrete target:
//! a target is an opaque identity, and its locator is an opaque string that the
//! runtime resolves. The Git facts an intent is derived from and reconciled
//! against (ref targets, commit and tree identities, observed ref states) live in
//! [`crate::publication::git`]; the execution of a publication stays in the
//! runtime.

mod identity;
mod reconciliation;
mod remote_observation;
mod run;

pub use identity::{
    PublishTargetId, PublishTargetIdError, RepositoryLocator, RepositoryLocatorError,
};
pub use reconciliation::{PublishReconciliation, PublishReconciliationError, ReadyToPush};
pub use remote_observation::{
    RemoteObservationId, RemoteObservationIdError, RemoteObservationIdGenerator,
    RemoteObservationStore, RemoteRefObservation,
};
pub use run::{
    DeliveryProjectionBinding, FrozenPublicScope, FrozenPublicScopeError, PublishRun,
    PublishRunError, PublishRunId, PublishRunIdError, PublishRunPublication, PublishRunStore,
};
