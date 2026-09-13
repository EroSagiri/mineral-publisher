//! Native Git publication: the adapters a runtime implements, plus the
//! composition root that binds them to the portable engine.
//!
//! There is exactly one publication path. Preparation is
//! [`GitPublicationPreparer`] and execution is [`GitPublicationExecutor`], both
//! defined by the engine; everything in this module either produces a fact a
//! runtime alone can observe or persists one the engine decided.

mod git_commit_object;
mod git_current_target;
mod git_projection_materializer;
mod git_publication;
mod git_remote;
mod git_repository;
mod remote_observation;

pub use mineral_core::publication::git::{
    CasOutcome, CommitSpecWire, CommitSpecWireError, GitCommitFacts, GitCommitOid,
    GitCommitOidError, GitCommitSpec, GitCommitSpecError, GitCurrentTarget,
    GitPublicationExecuteError, GitPublicationExecution, GitPublicationExecutor,
    GitPublicationPreparation, GitPublicationPrepareError, GitPublicationPrepareRequest,
    GitPublicationPreparer, GitRefTarget, GitRefTargetError, GitRemote, GitRepository, GitTreeOid,
    GitTreeOidError, LocalCommitState, RefUpdate, RemoteRefState, ReviewedGitTree,
};
pub use mineral_core::publish::{
    PublishReconciliation, PublishReconciliationError, PublishRun, PublishRunError, PublishRunId,
    PublishRunIdError, PublishRunPublication, PublishRunStore, PublishTargetId,
    PublishTargetIdError, ReadyToPush, RemoteObservationId, RemoteObservationIdError,
    RemoteObservationIdGenerator, RemoteObservationStore, RemoteRefObservation, RepositoryLocator,
    RepositoryLocatorError,
};

pub use git_commit_object::{
    GitCommitMetadata, GitCommitMetadataError, GitCommitObjectCreator, GitCommitObjectError,
};
pub use git_current_target::{GitCurrentTargetAdapter, GitCurrentTargetError};
pub use git_projection_materializer::{
    GitProjectionMaterializationError, GitProjectionMaterializer,
};
pub use git_publication::{
    GitPublicationApplication, GitPublicationApplicationError, GitPublicationApplicationResult,
    PublishRunIdGenerator, SequentialPublishRunIdGenerator, SequentialPublishRunIdGeneratorError,
    SequentialRemoteObservationIdGenerator, SequentialRemoteObservationIdGeneratorError,
    UuidPublishRunIdGenerator, UuidPublishRunIdGeneratorError, UuidRemoteObservationIdGenerator,
    UuidRemoteObservationIdGeneratorError,
};
pub use git_remote::{GitRemoteAdapter, GitRemoteError};
pub use git_repository::{
    GitRepositoryAdapter, GitRepositoryAdapterError, GitRepositoryIdentity,
    GitRepositoryIdentityError,
};
pub use remote_observation::GitRemoteObserver;
