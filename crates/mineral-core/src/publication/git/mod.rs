//! The Git publication boundary: portable Git facts, and the ports a runtime
//! implements to touch a real repository and remote.

mod execute;
mod model;
mod ports;
mod prepare;
mod wire;

pub use execute::{
    DeliveryPublicationExecuteError, DeliveryPublicationExecution, DeliveryPublicationExecutor,
    GitPublicationExecution,
};
pub use model::{
    CasOutcome, GitCommitFacts, GitCommitOid, GitCommitOidError, GitCommitSpec, GitCommitSpecError,
    GitCurrentTarget, GitRefTarget, GitRefTargetError, GitTreeOid, GitTreeOidError,
    LocalCommitState, RefUpdate, RemoteRefState, ReviewedGitTree,
};
pub use ports::{GitRemote, GitRepository};
pub use prepare::{
    GitPublicationPreparation, GitPublicationPrepareError, GitPublicationPrepareRequest,
    GitPublicationPreparer,
};
pub use wire::{CommitSpecWire, CommitSpecWireError};
