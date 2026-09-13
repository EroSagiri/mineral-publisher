use std::{error::Error, fmt};

use crate::{
    domain::Sha256,
    ports::Clock,
    publication::git::{
        CasOutcome, GitCommitFacts, GitCommitOid, GitRemote, GitRepository, LocalCommitState,
        RefUpdate,
    },
    publish::{
        DeliveryProjectionBinding, PublishReconciliation, PublishReconciliationError, PublishRun,
        PublishRunId, PublishRunStore, RemoteObservationIdGenerator, RemoteObservationStore,
        RemoteRefObservation,
    },
    workflow::DeliveryProjectionStore,
};

/// The explicit outcome of one recovery-safe publication execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitPublicationExecution {
    NoopSatisfied {
        observation: RemoteRefObservation,
    },
    AlreadyPublished {
        observation: RemoteRefObservation,
    },
    Published {
        observation: RemoteRefObservation,
        cas_outcome: CasOutcome,
    },
    PushFailedButRemoteUnchanged {
        observation: RemoteRefObservation,
    },
    RemoteUnchangedAfterSuccessfulPush {
        observation: RemoteRefObservation,
    },
    RemoteChanged {
        observation: RemoteRefObservation,
        cas_outcome: Option<CasOutcome>,
    },
    TargetMissing {
        observation: RemoteRefObservation,
        cas_outcome: Option<CasOutcome>,
    },
    Indeterminate {
        cas_outcome: Option<CasOutcome>,
    },
}

impl GitPublicationExecution {
    pub fn is_satisfied(&self) -> bool {
        matches!(
            self,
            Self::NoopSatisfied { .. } | Self::AlreadyPublished { .. } | Self::Published { .. }
        )
    }
}

/// Executes one durable publication intent against a remote.
///
/// The whole sequence, in order: reload the intent, observe the target, persist
/// that observation, reconcile, and — only when the intent needs a commit and the
/// remote still holds exactly the expected base — show that the desired commit
/// exists locally (rebuilding it from the frozen specification when it does not),
/// perform exactly one compare-and-swap, observe again, persist that observation,
/// reconcile again, and classify.
///
/// Two rules shape everything above:
///
/// * The intent is the only source of truth about what should happen. Persistence
///   is the first step, so an identifier that was never stored produces zero
///   remote effects, and an outcome of "we pushed" is never inferred from a
///   command exit: the remote is re-observed and re-reconciled before the result
///   can say `Published`.
/// * What a runtime reports is a fact, not a verdict. The base the remote held,
///   the parent and tree of the commit about to be published, and the identity of
///   the commit rebuilt from the frozen specification are all checked here.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitPublicationExecutor;

impl GitPublicationExecutor {
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn execute<P, D, O, I, R, M, C>(
        publish_run_id: PublishRunId,
        publish_run_store: &P,
        delivery_projections: &D,
        observation_store: &O,
        observation_ids: &mut I,
        repository: &R,
        remote: &M,
        clock: &C,
    ) -> Result<
        GitPublicationExecution,
        GitPublicationExecuteError<P::Error, D::Error, O::Error, I::Error, R::Error, M::Error>,
    >
    where
        P: PublishRunStore,
        D: DeliveryProjectionStore,
        O: RemoteObservationStore,
        I: RemoteObservationIdGenerator,
        R: GitRepository,
        M: GitRemote,
        C: Clock,
    {
        // Durable reload first: anything that is not a persisted intent cannot
        // reach a remote, so a wrong identifier is harmless by construction.
        let run = publish_run_store
            .get(publish_run_id)
            .map_err(GitPublicationExecuteError::PublishRunLoad)?
            .ok_or(GitPublicationExecuteError::PublishRunNotFound(
                publish_run_id,
            ))?;

        // Observe, persist the fact, then reconcile. The observation is an
        // append-only audit fact, and reconciliation is a decision derived from
        // the intent plus that fact — never the other way around.
        let pre_observation_id = observation_ids
            .next_id()
            .map_err(GitPublicationExecuteError::ObservationId)?;
        let pre_observed_at = clock
            .now()
            .ok_or(GitPublicationExecuteError::ClockUnavailable)?;
        let pre_state = remote
            .observe_ref(run.target())
            .map_err(GitPublicationExecuteError::InitialObservation)?;
        let pre_observation =
            RemoteRefObservation::new(pre_observation_id, &run, pre_state, pre_observed_at);
        observation_store
            .save(&pre_observation)
            .map_err(GitPublicationExecuteError::InitialObservationPersistence)?;

        let ready = match PublishReconciliation::derive(&run, &pre_observation)
            .map_err(GitPublicationExecuteError::Reconciliation)?
        {
            PublishReconciliation::NoopSatisfied => {
                return Ok(GitPublicationExecution::NoopSatisfied {
                    observation: pre_observation,
                });
            }
            PublishReconciliation::AlreadyPublished => {
                return Ok(GitPublicationExecution::AlreadyPublished {
                    observation: pre_observation,
                });
            }
            PublishReconciliation::RemoteChanged { .. } => {
                return Ok(GitPublicationExecution::RemoteChanged {
                    observation: pre_observation,
                    cas_outcome: None,
                });
            }
            PublishReconciliation::TargetMissing => {
                return Ok(GitPublicationExecution::TargetMissing {
                    observation: pre_observation,
                    cas_outcome: None,
                });
            }
            PublishReconciliation::ReadyToPush(ready) => ready,
        };

        let desired_commit = run
            .desired_commit()
            .cloned()
            .expect("reconciliation only asks to push for a run with a desired commit");
        Self::ensure_desired_commit::<P, D, O, I, R, M>(
            repository,
            delivery_projections,
            &run,
            &desired_commit,
        )?;

        // Exactly one compare-and-swap, with the expected previous value stated
        // explicitly: no implementation may approximate it with "push if
        // fast-forward" and still look correct.
        let update = RefUpdate::new(
            run.target().clone(),
            ready.expected_remote_oid().clone(),
            ready.desired_commit_oid().clone(),
        );
        let cas_outcome = remote
            .compare_and_swap(&update)
            .map_err(GitPublicationExecuteError::CompareAndSwap)?;

        // The attempt already happened. What the remote now holds is a new
        // observation, not a conclusion drawn from the attempt's exit status.
        let post_observation_id = observation_ids
            .next_id()
            .map_err(GitPublicationExecuteError::ObservationId)?;
        if post_observation_id == pre_observation_id {
            return Err(GitPublicationExecuteError::PostObservationIdReused);
        }
        let post_observed_at = clock
            .now()
            .ok_or(GitPublicationExecuteError::ClockUnavailable)?;
        let post_state = match remote.observe_ref(run.target()) {
            Ok(state) => state,
            // The side effect may or may not have landed and the runtime cannot
            // say. Recording nothing is the honest outcome: the next attempt
            // re-observes and reconciles from the durable intent.
            Err(_) => {
                return Ok(GitPublicationExecution::Indeterminate {
                    cas_outcome: Some(cas_outcome),
                });
            }
        };
        let post_observation =
            RemoteRefObservation::new(post_observation_id, &run, post_state, post_observed_at);
        observation_store
            .save(&post_observation)
            .map_err(GitPublicationExecuteError::PostObservationPersistence)?;

        match PublishReconciliation::derive(&run, &post_observation)
            .map_err(GitPublicationExecuteError::Reconciliation)?
        {
            PublishReconciliation::AlreadyPublished => Ok(GitPublicationExecution::Published {
                observation: post_observation,
                cas_outcome,
            }),
            PublishReconciliation::ReadyToPush(_) => match cas_outcome {
                CasOutcome::Rejected => Ok(GitPublicationExecution::PushFailedButRemoteUnchanged {
                    observation: post_observation,
                }),
                CasOutcome::Updated => Ok(
                    GitPublicationExecution::RemoteUnchangedAfterSuccessfulPush {
                        observation: post_observation,
                    },
                ),
            },
            PublishReconciliation::RemoteChanged { .. } => {
                Ok(GitPublicationExecution::RemoteChanged {
                    observation: post_observation,
                    cas_outcome: Some(cas_outcome),
                })
            }
            PublishReconciliation::TargetMissing => Ok(GitPublicationExecution::TargetMissing {
                observation: post_observation,
                cas_outcome: Some(cas_outcome),
            }),
            PublishReconciliation::NoopSatisfied => {
                Err(GitPublicationExecuteError::UnexpectedNoopReconciliation)
            }
        }
    }

    /// Shows that the commit this run wants exists locally and is the one the
    /// review covered, rebuilding it when the object is gone.
    ///
    /// Rebuilding uses the frozen specification and nothing else. An intent
    /// persisted before specifications were stored cannot be rebuilt at all, and
    /// inventing one from current configuration is precisely what the durable
    /// model forbids, so that case fails closed instead.
    ///
    /// A commit object needs its tree to exist, and a runtime that lost its object
    /// database lost the tree too. When the run binds a durable delivery
    /// projection, the reviewed tree is rebuilt from that projection and its
    /// identity is checked before the commit is recreated — so the tree that
    /// becomes public is provably the tree that was reviewed, not merely a tree
    /// that happens to still exist. A run written before delivery projections were
    /// captured has nothing to rebuild from; it degrades to the older behaviour
    /// and fails closed if its tree is gone.
    #[allow(clippy::type_complexity)]
    fn ensure_desired_commit<P, D, O, I, R, M>(
        repository: &R,
        delivery_projections: &D,
        run: &PublishRun,
        desired_commit: &GitCommitOid,
    ) -> Result<
        (),
        GitPublicationExecuteError<P::Error, D::Error, O::Error, I::Error, R::Error, M::Error>,
    >
    where
        P: PublishRunStore,
        D: DeliveryProjectionStore,
        O: RemoteObservationStore,
        I: RemoteObservationIdGenerator,
        R: GitRepository,
        M: GitRemote,
    {
        match repository
            .inspect_commit(desired_commit)
            .map_err(GitPublicationExecuteError::LocalCommitInspection)?
        {
            LocalCommitState::Present(facts) => {
                Self::verify_commit_facts::<P, D, O, I, R, M>(run, desired_commit, &facts)
            }
            LocalCommitState::Missing => {
                let Some(spec) = run.commit_spec() else {
                    return Err(GitPublicationExecuteError::LegacyCommitNotReconstructible {
                        desired_commit: desired_commit.clone(),
                    });
                };
                if let Some(binding) = run.delivery_projection_binding() {
                    Self::rematerialize_reviewed_tree::<P, D, O, I, R, M>(
                        repository,
                        delivery_projections,
                        run,
                        &binding,
                    )?;
                }
                let recreated = repository
                    .create_commit(spec)
                    .map_err(GitPublicationExecuteError::CommitCreation)?;
                if &recreated != desired_commit {
                    return Err(GitPublicationExecuteError::ReconstructedCommitMismatch {
                        expected: desired_commit.clone(),
                        actual: recreated,
                    });
                }
                match repository
                    .inspect_commit(desired_commit)
                    .map_err(GitPublicationExecuteError::LocalCommitInspection)?
                {
                    LocalCommitState::Present(facts) => {
                        Self::verify_commit_facts::<P, D, O, I, R, M>(run, desired_commit, &facts)
                    }
                    LocalCommitState::Missing => {
                        Err(GitPublicationExecuteError::ReconstructedCommitAbsent {
                            commit: desired_commit.clone(),
                        })
                    }
                }
            }
        }
    }

    /// Rebuilds the reviewed tree from the run's own durable delivery projection.
    ///
    /// This is the only recovery input: the engine loads exactly the projection the
    /// intent bound, refuses anything the store returns under another identity, and
    /// requires the rebuilt tree to be the tree the intent recorded. Current
    /// configuration is never consulted, and a projection that disagrees with the
    /// run is an error rather than something to prefer.
    #[allow(clippy::type_complexity)]
    fn rematerialize_reviewed_tree<P, D, O, I, R, M>(
        repository: &R,
        delivery_projections: &D,
        run: &PublishRun,
        binding: &DeliveryProjectionBinding,
    ) -> Result<
        (),
        GitPublicationExecuteError<P::Error, D::Error, O::Error, I::Error, R::Error, M::Error>,
    >
    where
        P: PublishRunStore,
        D: DeliveryProjectionStore,
        O: RemoteObservationStore,
        I: RemoteObservationIdGenerator,
        R: GitRepository,
        M: GitRemote,
    {
        let delivery = delivery_projections
            .get(binding.delivery_sha256())
            .map_err(GitPublicationExecuteError::DeliveryProjectionLoad)?
            .ok_or(GitPublicationExecuteError::DeliveryProjectionNotFound {
                id: binding.delivery_sha256(),
            })?;
        // The store answered under a key; the projection has to prove the key.
        if delivery.delivery_sha256() != binding.delivery_sha256() {
            return Err(
                GitPublicationExecuteError::DeliveryProjectionIdentityMismatch {
                    expected: binding.delivery_sha256(),
                    actual: delivery.delivery_sha256(),
                },
            );
        }
        if delivery.text().projection_sha256() != binding.text_projection_sha256() {
            return Err(GitPublicationExecuteError::DeliveryTextProjectionMismatch {
                expected: binding.text_projection_sha256(),
                actual: delivery.text().projection_sha256(),
            });
        }
        let rebuilt = repository
            .materialize(&run.base_commit_oid(), delivery.text())
            .map_err(GitPublicationExecuteError::Rematerialization)?;
        if rebuilt.base_commit() != run.base_commit() {
            return Err(GitPublicationExecuteError::RematerializedBaseMismatch {
                expected: run.base_commit().to_owned(),
                actual: rebuilt.base_commit().to_owned(),
            });
        }
        if rebuilt.tree_oid() != run.reviewed_tree() {
            return Err(GitPublicationExecuteError::RematerializedTreeMismatch {
                expected: run.reviewed_tree().to_owned(),
                actual: rebuilt.tree_oid().to_owned(),
            });
        }
        Ok(())
    }

    /// The `reviewed tree == committed tree` invariant, plus the parent that makes
    /// the commit a child of the base the intent observed.
    #[allow(clippy::type_complexity)]
    fn verify_commit_facts<P, D, O, I, R, M>(
        run: &PublishRun,
        desired_commit: &GitCommitOid,
        facts: &GitCommitFacts,
    ) -> Result<
        (),
        GitPublicationExecuteError<P::Error, D::Error, O::Error, I::Error, R::Error, M::Error>,
    >
    where
        P: PublishRunStore,
        D: DeliveryProjectionStore,
        O: RemoteObservationStore,
        I: RemoteObservationIdGenerator,
        R: GitRepository,
        M: GitRemote,
    {
        if facts.commit() != desired_commit {
            return Err(GitPublicationExecuteError::CommitIdentityMismatch {
                expected: desired_commit.clone(),
                actual: facts.commit().clone(),
            });
        }
        if facts.parent().as_str() != run.base_commit() {
            return Err(GitPublicationExecuteError::CommitParentMismatch {
                expected: run.base_commit().to_owned(),
                actual: facts.parent().as_str().to_owned(),
            });
        }
        if facts.tree().as_str() != run.reviewed_tree() {
            return Err(GitPublicationExecuteError::CommitTreeMismatch {
                expected: run.reviewed_tree().to_owned(),
                actual: facts.tree().as_str().to_owned(),
            });
        }
        Ok(())
    }
}

/// Errors that stopped one execution from producing a durable, meaningful outcome.
#[derive(Debug)]
pub enum GitPublicationExecuteError<P: Error, D: Error, O: Error, I: Error, R: Error, M: Error> {
    PublishRunLoad(P),
    PublishRunNotFound(PublishRunId),
    /// The runtime could not read its durable delivery projections.
    DeliveryProjectionLoad(D),
    /// The run binds a delivery projection that was never captured.
    DeliveryProjectionNotFound {
        id: Sha256,
    },
    /// The store returned a projection that is not the one the key names.
    DeliveryProjectionIdentityMismatch {
        expected: Sha256,
        actual: Sha256,
    },
    /// The loaded projection's text side is not the text tree the run recorded.
    DeliveryTextProjectionMismatch {
        expected: Sha256,
        actual: Sha256,
    },
    /// The runtime could not rebuild the reviewed tree from the stored projection.
    Rematerialization(R),
    /// The rebuilt tree belongs to another base than the intent observed.
    RematerializedBaseMismatch {
        expected: String,
        actual: String,
    },
    /// The rebuilt tree is not the tree the intent recorded.
    RematerializedTreeMismatch {
        expected: String,
        actual: String,
    },
    ObservationId(I),
    ClockUnavailable,
    InitialObservation(M),
    InitialObservationPersistence(O),
    Reconciliation(PublishReconciliationError),
    /// The runtime could not read its own object database.
    LocalCommitInspection(R),
    /// The commit is gone and the intent froze no specification to rebuild it.
    /// Rebuilding from current configuration is not an option.
    LegacyCommitNotReconstructible {
        desired_commit: GitCommitOid,
    },
    /// The runtime could not rebuild the commit from the frozen specification.
    CommitCreation(R),
    /// The rebuilt commit is not the commit the intent froze.
    ReconstructedCommitMismatch {
        expected: GitCommitOid,
        actual: GitCommitOid,
    },
    /// The commit object that is present is absent again immediately after being
    /// rebuilt, which no honest runtime can report.
    ReconstructedCommitAbsent {
        commit: GitCommitOid,
    },
    /// The runtime reported facts for another commit object.
    CommitIdentityMismatch {
        expected: GitCommitOid,
        actual: GitCommitOid,
    },
    CommitParentMismatch {
        expected: String,
        actual: String,
    },
    CommitTreeMismatch {
        expected: String,
        actual: String,
    },
    /// The runtime handed out the same identity twice for two distinct
    /// observations.
    PostObservationIdReused,
    CompareAndSwap(M),
    PostObservation(M),
    PostObservationPersistence(O),
    UnexpectedNoopReconciliation,
}

impl<P: Error, D: Error, O: Error, I: Error, R: Error, M: Error> fmt::Display
    for GitPublicationExecuteError<P, D, O, I, R, M>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublishRunLoad(_) => formatter.write_str("could not load publish run"),
            Self::PublishRunNotFound(id) => {
                write!(formatter, "publish run {} was not found", id.get())
            }
            Self::DeliveryProjectionLoad(_) => {
                formatter.write_str("could not load the durable delivery projection")
            }
            Self::DeliveryProjectionNotFound { id } => write!(
                formatter,
                "the delivery projection {id} this run is bound to was never captured"
            ),
            Self::DeliveryProjectionIdentityMismatch { expected, actual } => write!(
                formatter,
                "the delivery projection store returned {actual} for the requested {expected}"
            ),
            Self::DeliveryTextProjectionMismatch { expected, actual } => write!(
                formatter,
                "the durable delivery projection carries text identity {actual}, not the reviewed {expected}"
            ),
            Self::Rematerialization(_) => formatter
                .write_str("could not rebuild the reviewed tree from the delivery projection"),
            Self::RematerializedBaseMismatch { expected, actual } => write!(
                formatter,
                "the rebuilt tree belongs to base {actual}, not {expected}"
            ),
            Self::RematerializedTreeMismatch { expected, actual } => write!(
                formatter,
                "the rebuilt reviewed tree is {actual}, not {expected}"
            ),
            Self::ObservationId(_) => {
                formatter.write_str("could not allocate remote observation ID")
            }
            Self::ClockUnavailable => {
                formatter.write_str("clock reading cannot be recorded as a timestamp")
            }
            Self::InitialObservation(error) => {
                write!(formatter, "could not observe remote before push: {error}")
            }
            Self::InitialObservationPersistence(_) => {
                formatter.write_str("pre-push remote observation could not be persisted")
            }
            Self::Reconciliation(error) => error.fmt(formatter),
            Self::LocalCommitInspection(error) => {
                write!(
                    formatter,
                    "could not inspect the local commit object: {error}"
                )
            }
            Self::LegacyCommitNotReconstructible { desired_commit } => write!(
                formatter,
                "the desired commit {} is gone and this run froze no specification to rebuild it",
                desired_commit.as_str()
            ),
            Self::CommitCreation(error) => {
                write!(
                    formatter,
                    "could not rebuild the desired commit object: {error}"
                )
            }
            Self::ReconstructedCommitMismatch { expected, actual } => write!(
                formatter,
                "the rebuilt commit {} is not the desired commit {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::ReconstructedCommitAbsent { commit } => write!(
                formatter,
                "the rebuilt commit {} is not present immediately after creation",
                commit.as_str()
            ),
            Self::CommitIdentityMismatch { expected, actual } => write!(
                formatter,
                "the local object is {} and not the desired commit {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::CommitParentMismatch { expected, actual } => write!(
                formatter,
                "the desired commit builds on {actual} instead of the observed base {expected}"
            ),
            Self::CommitTreeMismatch { expected, actual } => write!(
                formatter,
                "the desired commit holds tree {actual} instead of the reviewed tree {expected}"
            ),
            Self::PostObservationIdReused => formatter
                .write_str("the post-push observation reused an existing observation identity"),
            Self::CompareAndSwap(error) => {
                write!(
                    formatter,
                    "could not compare-and-swap the remote ref: {error}"
                )
            }
            Self::PostObservation(error) => {
                write!(formatter, "could not observe remote after push: {error}")
            }
            Self::PostObservationPersistence(_) => {
                formatter.write_str("post-push remote observation could not be persisted")
            }
            Self::UnexpectedNoopReconciliation => {
                formatter.write_str("a Noop reconciliation followed a compare-and-swap attempt")
            }
        }
    }
}

impl<
    P: Error + 'static,
    D: Error + 'static,
    O: Error + 'static,
    I: Error + 'static,
    R: Error + 'static,
    M: Error + 'static,
> Error for GitPublicationExecuteError<P, D, O, I, R, M>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PublishRunLoad(error) => Some(error),
            Self::DeliveryProjectionLoad(error) => Some(error),
            Self::Rematerialization(error) => Some(error),
            Self::ObservationId(error) => Some(error),
            Self::InitialObservation(error) => Some(error),
            Self::InitialObservationPersistence(error) => Some(error),
            Self::Reconciliation(error) => Some(error),
            Self::LocalCommitInspection(error) => Some(error),
            Self::CommitCreation(error) => Some(error),
            Self::CompareAndSwap(error) => Some(error),
            Self::PostObservation(error) => Some(error),
            Self::PostObservationPersistence(error) => Some(error),
            Self::PublishRunNotFound(_)
            | Self::ClockUnavailable
            | Self::DeliveryProjectionNotFound { .. }
            | Self::DeliveryProjectionIdentityMismatch { .. }
            | Self::DeliveryTextProjectionMismatch { .. }
            | Self::RematerializedBaseMismatch { .. }
            | Self::RematerializedTreeMismatch { .. }
            | Self::LegacyCommitNotReconstructible { .. }
            | Self::ReconstructedCommitMismatch { .. }
            | Self::ReconstructedCommitAbsent { .. }
            | Self::CommitIdentityMismatch { .. }
            | Self::CommitParentMismatch { .. }
            | Self::CommitTreeMismatch { .. }
            | Self::PostObservationIdReused
            | Self::UnexpectedNoopReconciliation => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        convert::Infallible,
        error::Error,
        fmt,
    };

    use crate::{
        domain::{ContentPath, Sha256, SnapshotId, TimestampMillis},
        publication::git::{
            GitCommitSpec, GitCurrentTarget, GitRefTarget, GitTreeOid, RemoteRefState,
            ReviewedGitTree,
        },
        publish::{
            PublishTargetId, RemoteObservationId, RemoteObservationIdError, RepositoryLocator,
        },
        workflow::{AssetProjection, DeliveryProjection, ManagedRoot, TextProjection},
    };

    use super::*;

    const BASE: char = 'a';
    const REVIEWED_TREE: char = 'b';
    const DESIRED: char = 'c';
    const OTHER_TREE: char = 'd';
    const TIME: u64 = 1_500;

    fn oid(value: char) -> GitCommitOid {
        GitCommitOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn tree(value: char) -> GitTreeOid {
        GitTreeOid::new(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn spec() -> GitCommitSpec {
        GitCommitSpec::new(
            oid(BASE),
            tree(REVIEWED_TREE),
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(TIME),
            "Mineral Publisher",
            "publisher@example.invalid",
            TimestampMillis::from_unix_millis(TIME),
            "Publish Mineral content",
        )
        .unwrap()
    }

    /// The intent under execution. `commit_spec` may be dropped to model an intent
    /// persisted before specifications were stored.
    fn intent(
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
    ) -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            Some(Sha256::new([1; 32])),
            None,
            None,
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            oid(BASE).as_str().to_owned(),
            oid(REVIEWED_TREE).as_str().to_owned(),
            desired_commit,
            commit_spec,
            TIME,
        )
        .unwrap()
    }

    fn ready_intent() -> PublishRun {
        intent(Some(oid(DESIRED)), Some(spec()))
    }

    /// A real delivery projection whose text side is one document.
    fn delivery_projection(seed: u8) -> DeliveryProjection {
        let source_projection_sha256 = Sha256::new([seed; 32]);
        let text = TextProjection::from_parts_for_test(
            SnapshotId::new(1).unwrap(),
            ManagedRoot::new("content").unwrap(),
            source_projection_sha256,
            vec![(
                ContentPath::new("note.md").unwrap(),
                Sha256::new([seed.wrapping_add(1); 32]),
            )],
        );
        DeliveryProjection::from_parts(
            source_projection_sha256,
            SnapshotId::new(1).unwrap(),
            ManagedRoot::new("content").unwrap(),
            text,
            AssetProjection::from_assets(Vec::new()),
        )
    }

    /// A persisted intent bound to one delivery projection, with an optional
    /// override for the recorded text identity.
    fn bound_intent(
        projection: &DeliveryProjection,
        recorded_text: Option<Sha256>,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
    ) -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            None,
            Some(recorded_text.unwrap_or_else(|| projection.text().projection_sha256())),
            Some(projection.delivery_sha256()),
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            oid(BASE).as_str().to_owned(),
            oid(REVIEWED_TREE).as_str().to_owned(),
            desired_commit,
            commit_spec,
            TIME,
        )
        .unwrap()
    }

    fn present(value: char) -> RemoteRefState {
        RemoteRefState::Present {
            commit_oid: oid(value),
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PortFailure(&'static str);

    impl fmt::Display for PortFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for PortFailure {}

    struct FakePublishRuns(BTreeMap<PublishRunId, PublishRun>);

    impl FakePublishRuns {
        fn with(run: PublishRun) -> Self {
            Self(BTreeMap::from([(run.id(), run)]))
        }

        fn empty() -> Self {
            Self(BTreeMap::new())
        }
    }

    impl PublishRunStore for FakePublishRuns {
        type Error = Infallible;

        fn save(&self, _: &PublishRun) -> Result<(), Self::Error> {
            unreachable!("execution never writes an intent")
        }
        fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error> {
            Ok(self.0.get(&id).cloned())
        }
        fn list(&self) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(self.0.values().cloned().collect())
        }
        fn list_for_target(&self, _: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error> {
            Ok(Vec::new())
        }
    }

    /// An append-only observation store that can be made to fail after N saves.
    #[derive(Default)]
    struct FakeObservations {
        saved: RefCell<Vec<RemoteRefObservation>>,
        fail_after: Cell<Option<usize>>,
    }

    impl FakeObservations {
        fn rejecting_after(count: usize) -> Self {
            Self {
                saved: RefCell::new(Vec::new()),
                fail_after: Cell::new(Some(count)),
            }
        }

        fn accept_everything(&mut self) {
            self.fail_after.set(None);
        }

        fn saved(&self) -> Vec<RemoteRefObservation> {
            self.saved.borrow().clone()
        }
    }

    impl RemoteObservationStore for FakeObservations {
        type Error = PortFailure;

        fn save(&self, observation: &RemoteRefObservation) -> Result<(), Self::Error> {
            if let Some(limit) = self.fail_after.get()
                && self.saved.borrow().len() >= limit
            {
                return Err(PortFailure("observation persistence failed"));
            }
            self.saved.borrow_mut().push(observation.clone());
            Ok(())
        }
        fn get(&self, _: RemoteObservationId) -> Result<Option<RemoteRefObservation>, Self::Error> {
            Ok(None)
        }
        fn list_for_publish_run(
            &self,
            _: PublishRunId,
        ) -> Result<Vec<RemoteRefObservation>, Self::Error> {
            Ok(self.saved())
        }
    }

    #[derive(Clone, Copy)]
    enum TestIds {
        Sequential(u64),
        Repeating,
    }

    impl RemoteObservationIdGenerator for TestIds {
        type Error = RemoteObservationIdError;

        fn next_id(&mut self) -> Result<RemoteObservationId, Self::Error> {
            match self {
                Self::Sequential(next) => {
                    let id = RemoteObservationId::new(*next)?;
                    *next += 1;
                    Ok(id)
                }
                Self::Repeating => RemoteObservationId::new(7),
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now(&self) -> Option<TimestampMillis> {
            Some(TimestampMillis::from_unix_millis(self.0))
        }
    }

    /// A remote that counts every call, so "no remote effects" is asserted instead
    /// of assumed. `observe_ref` walks the state list and repeats the last entry.
    struct CountingGitRemote {
        states: RefCell<Vec<RemoteRefState>>,
        cas_outcome: CasOutcome,
        fail_observations_after: Cell<Option<u32>>,
        observe_ref_calls: Cell<u32>,
        compare_and_swap_calls: Cell<u32>,
        updates: RefCell<Vec<RefUpdate>>,
    }

    impl CountingGitRemote {
        fn new(states: Vec<RemoteRefState>) -> Self {
            Self {
                states: RefCell::new(states),
                cas_outcome: CasOutcome::Updated,
                fail_observations_after: Cell::new(None),
                observe_ref_calls: Cell::new(0),
                compare_and_swap_calls: Cell::new(0),
                updates: RefCell::new(Vec::new()),
            }
        }

        fn with_cas_outcome(mut self, outcome: CasOutcome) -> Self {
            self.cas_outcome = outcome;
            self
        }

        fn failing_observations_after(self, calls: u32) -> Self {
            self.fail_observations_after.set(Some(calls));
            self
        }

        fn observe_ref_calls(&self) -> u32 {
            self.observe_ref_calls.get()
        }

        fn compare_and_swap_calls(&self) -> u32 {
            self.compare_and_swap_calls.get()
        }

        fn updates(&self) -> Vec<RefUpdate> {
            self.updates.borrow().clone()
        }
    }

    impl GitRemote for CountingGitRemote {
        type Error = PortFailure;

        fn observe_ref(&self, _: &GitRefTarget) -> Result<RemoteRefState, Self::Error> {
            let calls = self.observe_ref_calls.get() + 1;
            self.observe_ref_calls.set(calls);
            if let Some(limit) = self.fail_observations_after.get()
                && calls > limit
            {
                return Err(PortFailure("remote observation failed"));
            }
            let mut states = self.states.borrow_mut();
            if states.len() > 1 {
                Ok(states.remove(0))
            } else {
                Ok(states.first().cloned().unwrap_or(RemoteRefState::Missing))
            }
        }

        fn compare_and_swap(&self, update: &RefUpdate) -> Result<CasOutcome, Self::Error> {
            self.compare_and_swap_calls
                .set(self.compare_and_swap_calls.get() + 1);
            self.updates.borrow_mut().push(update.clone());
            Ok(self.cas_outcome)
        }
    }

    /// A repository whose local commit facts the test chooses.
    struct FakeGitRepository {
        observed: RefCell<Vec<Result<LocalCommitState, PortFailure>>>,
        create_result: RefCell<Result<GitCommitOid, PortFailure>>,
        materialized: RefCell<Result<ReviewedGitTree, PortFailure>>,
        inspect_commit_calls: Cell<u32>,
        create_commit_calls: Cell<u32>,
        materialize_calls: Cell<u32>,
        created_specs: RefCell<Vec<GitCommitSpec>>,
        materialized_texts: RefCell<Vec<TextProjection>>,
    }

    impl FakeGitRepository {
        fn with_observations(observations: Vec<Result<LocalCommitState, PortFailure>>) -> Self {
            Self {
                observed: RefCell::new(observations),
                create_result: RefCell::new(Ok(oid(DESIRED))),
                // Recovery only materializes when the intent binds a durable
                // projection, so an unexpected call must be loud.
                materialized: RefCell::new(Err(PortFailure(
                    "execution materialized without a delivery projection",
                ))),
                inspect_commit_calls: Cell::new(0),
                create_commit_calls: Cell::new(0),
                materialize_calls: Cell::new(0),
                created_specs: RefCell::new(Vec::new()),
                materialized_texts: RefCell::new(Vec::new()),
            }
        }

        fn with_materialized_tree(mut self, base: char, reviewed_tree: char) -> Self {
            self.materialized = RefCell::new(Ok(ReviewedGitTree::from_parts(
                oid(base).as_str(),
                oid('9').as_str(),
                Sha256::new([1; 32]),
                SnapshotId::new(1).unwrap(),
                oid(reviewed_tree).as_str(),
                ManagedRoot::new("content").unwrap(),
            )));
            self
        }

        fn with_materialization_error(mut self) -> Self {
            self.materialized = RefCell::new(Err(PortFailure(
                "reviewed tree could not be rebuilt from the delivery projection",
            )));
            self
        }

        fn materialize_calls(&self) -> u32 {
            self.materialize_calls.get()
        }

        fn materialized_texts(&self) -> Vec<TextProjection> {
            self.materialized_texts.borrow().clone()
        }

        fn present(parent: char, reviewed_tree: char) -> Self {
            Self::with_observations(vec![Ok(LocalCommitState::Present(
                GitCommitFacts::from_parts(oid(DESIRED), oid(parent), tree(reviewed_tree)),
            ))])
        }

        fn missing() -> Self {
            Self::with_observations(vec![Ok(LocalCommitState::Missing)])
        }

        fn missing_then_rebuilt() -> Self {
            Self::with_observations(vec![
                Ok(LocalCommitState::Missing),
                Ok(LocalCommitState::Present(GitCommitFacts::from_parts(
                    oid(DESIRED),
                    oid(BASE),
                    tree(REVIEWED_TREE),
                ))),
            ])
        }

        fn with_create_result(mut self, result: Result<GitCommitOid, PortFailure>) -> Self {
            self.create_result = RefCell::new(result);
            self
        }

        fn with_recreated(mut self, recreated: char) -> Self {
            self.create_result = RefCell::new(Ok(oid(recreated)));
            self
        }

        fn inspect_commit_calls(&self) -> u32 {
            self.inspect_commit_calls.get()
        }

        fn create_commit_calls(&self) -> u32 {
            self.create_commit_calls.get()
        }

        fn created_specs(&self) -> Vec<GitCommitSpec> {
            self.created_specs.borrow().clone()
        }

        fn next_observation(&self) -> Result<LocalCommitState, PortFailure> {
            let mut observed = self.observed.borrow_mut();
            if observed.len() > 1 {
                observed.remove(0)
            } else {
                observed
                    .first()
                    .cloned()
                    .unwrap_or(Ok(LocalCommitState::Missing))
            }
        }
    }

    impl GitRepository for FakeGitRepository {
        type Error = PortFailure;

        fn read_current(
            &self,
            _: &GitCommitOid,
            _: &ManagedRoot,
        ) -> Result<GitCurrentTarget, Self::Error> {
            unreachable!("execution never reads the current target")
        }

        fn materialize(
            &self,
            _: &GitCommitOid,
            text: &TextProjection,
        ) -> Result<ReviewedGitTree, Self::Error> {
            self.materialize_calls.set(self.materialize_calls.get() + 1);
            self.materialized_texts.borrow_mut().push(text.clone());
            self.materialized.borrow().clone()
        }

        fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
            self.create_commit_calls
                .set(self.create_commit_calls.get() + 1);
            self.created_specs.borrow_mut().push(spec.clone());
            self.create_result.borrow().clone()
        }

        fn inspect_commit(&self, _: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
            self.inspect_commit_calls
                .set(self.inspect_commit_calls.get() + 1);
            self.next_observation()
        }
    }

    type Failure = GitPublicationExecuteError<
        Infallible,
        PortFailure,
        PortFailure,
        RemoteObservationIdError,
        PortFailure,
        PortFailure,
    >;

    /// A delivery projection store whose answer the test chooses, so the recovery
    /// sequence can be shown to depend on durable facts and nothing else.
    #[derive(Default)]
    struct FakeDeliveryProjections {
        stored: RefCell<std::collections::HashMap<Sha256, DeliveryProjection>>,
        substituted: RefCell<Option<DeliveryProjection>>,
        fail_load: Cell<bool>,
        get_calls: Cell<u32>,
    }

    impl FakeDeliveryProjections {
        fn with(self, projection: DeliveryProjection) -> Self {
            self.stored
                .borrow_mut()
                .insert(projection.delivery_sha256(), projection);
            self
        }

        /// Answers every request with this projection, whatever identity was asked
        /// for: a store that violates its own contract.
        fn substituting(projection: DeliveryProjection) -> Self {
            Self {
                substituted: RefCell::new(Some(projection)),
                ..Self::default()
            }
        }

        fn rejecting() -> Self {
            Self {
                fail_load: Cell::new(true),
                ..Self::default()
            }
        }

        fn get_calls(&self) -> u32 {
            self.get_calls.get()
        }
    }

    impl DeliveryProjectionStore for FakeDeliveryProjections {
        type Error = PortFailure;

        fn save(&self, _: &DeliveryProjection) -> Result<(), Self::Error> {
            unreachable!("execution never stores a delivery projection")
        }

        fn get(&self, id: Sha256) -> Result<Option<DeliveryProjection>, Self::Error> {
            self.get_calls.set(self.get_calls.get() + 1);
            if self.fail_load.get() {
                return Err(PortFailure("delivery projection load failed"));
            }
            if let Some(substituted) = self.substituted.borrow().clone() {
                return Ok(Some(substituted));
            }
            Ok(self.stored.borrow().get(&id).cloned())
        }
    }

    struct Harness {
        runs: FakePublishRuns,
        deliveries: FakeDeliveryProjections,
        observations: FakeObservations,
        ids: TestIds,
        repository: FakeGitRepository,
        remote: CountingGitRemote,
        clock: FixedClock,
    }

    impl Harness {
        fn new(
            run: Option<PublishRun>,
            repository: FakeGitRepository,
            states: Vec<RemoteRefState>,
        ) -> Self {
            Self {
                runs: match run {
                    Some(run) => FakePublishRuns::with(run),
                    None => FakePublishRuns::empty(),
                },
                deliveries: FakeDeliveryProjections::default(),
                observations: FakeObservations::default(),
                ids: TestIds::Sequential(1),
                repository,
                remote: CountingGitRemote::new(states),
                clock: FixedClock(TIME),
            }
        }

        fn execute(&mut self) -> Result<GitPublicationExecution, Failure> {
            let Harness {
                runs,
                deliveries,
                observations,
                ids,
                repository,
                remote,
                clock,
            } = self;
            GitPublicationExecutor::execute(
                PublishRunId::new(1).unwrap(),
                runs,
                deliveries,
                observations,
                ids,
                repository,
                remote,
                clock,
            )
        }
    }

    #[test]
    fn an_unknown_publish_run_id_reaches_no_remote() {
        let mut harness = Harness::new(
            None,
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::PublishRunNotFound(_))
        ));
        assert_eq!(harness.remote.observe_ref_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
        assert_eq!(harness.repository.inspect_commit_calls(), 0);
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert!(harness.observations.saved().is_empty());
    }

    #[test]
    fn an_observation_that_cannot_be_persisted_stops_before_any_decision() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.observations = FakeObservations::rejecting_after(0);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::InitialObservationPersistence(_))
        ));
        assert_eq!(harness.remote.observe_ref_calls(), 1);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
        assert_eq!(harness.repository.inspect_commit_calls(), 0);
    }

    #[test]
    fn a_remote_that_already_holds_the_desired_commit_is_never_pushed() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(DESIRED)],
        );

        let execution = harness.execute().unwrap();
        assert!(matches!(
            execution,
            GitPublicationExecution::AlreadyPublished { .. }
        ));
        assert!(execution.is_satisfied());
        assert_eq!(harness.remote.observe_ref_calls(), 1);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
        assert_eq!(harness.repository.inspect_commit_calls(), 0);
        assert_eq!(harness.observations.saved().len(), 1);
    }

    #[test]
    fn a_remote_that_moved_away_from_the_expected_base_is_a_conflict_without_a_push() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present('e')],
        );

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::RemoteChanged {
                cas_outcome: None,
                ..
            }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
        assert_eq!(harness.repository.inspect_commit_calls(), 0);
    }

    #[test]
    fn a_noop_intent_is_satisfied_without_a_push() {
        let mut harness = Harness::new(
            Some(intent(None, None)),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::NoopSatisfied { .. }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_missing_target_is_reported_without_a_push() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![RemoteRefState::Missing],
        );

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::TargetMissing {
                cas_outcome: None,
                ..
            }
        ));
        // A missing target is not a durable completion, whatever it costs.
        assert!(!harness.execute().unwrap().is_satisfied());
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_desired_commit_that_holds_another_tree_fails_closed_before_any_push() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, OTHER_TREE),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::CommitTreeMismatch { .. })
        ));
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_desired_commit_that_builds_on_another_parent_fails_closed_before_any_push() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present('e', REVIEWED_TREE),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::CommitParentMismatch { .. })
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_runtime_that_cannot_read_its_object_database_fails_closed() {
        let mut repository = FakeGitRepository::present(BASE, REVIEWED_TREE);
        repository.observed = RefCell::new(vec![Err(PortFailure("cat-file failed"))]);
        let mut harness = Harness::new(Some(ready_intent()), repository, vec![present(BASE)]);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::LocalCommitInspection(_))
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_legacy_intent_whose_commit_is_gone_cannot_be_rebuilt() {
        let mut harness = Harness::new(
            Some(intent(Some(oid(DESIRED)), None)),
            FakeGitRepository::missing(),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::LegacyCommitNotReconstructible { .. })
        ));
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_reconstructible_intent_rebuilds_the_exact_commit_and_pushes_it() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::missing_then_rebuilt(),
            vec![present(BASE), present(DESIRED)],
        );

        let execution = harness.execute().unwrap();
        let GitPublicationExecution::Published { cas_outcome, .. } = execution else {
            panic!("expected a published execution, got {execution:?}");
        };
        assert_eq!(cas_outcome, CasOutcome::Updated);
        assert_eq!(harness.repository.create_commit_calls(), 1);
        assert_eq!(harness.repository.created_specs(), vec![spec()]);
        assert_eq!(harness.repository.inspect_commit_calls(), 2);
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
        assert_eq!(harness.observations.saved().len(), 2);

        // The exact compare-and-swap: the expected previous value is stated.
        let updates = harness.remote.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].target().destination_ref(), "refs/heads/main");
        assert_eq!(updates[0].expected_old(), &oid(BASE));
        assert_eq!(updates[0].new_commit(), &oid(DESIRED));
    }

    #[test]
    fn a_rebuilt_commit_that_is_not_the_frozen_one_fails_closed() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::missing_then_rebuilt().with_recreated('e'),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::ReconstructedCommitMismatch { .. })
        ));
        assert_eq!(harness.repository.create_commit_calls(), 1);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_runtime_that_cannot_rebuild_the_commit_fails_closed() {
        // This is also the path a lost reviewed-tree object takes: a runtime cannot
        // recreate a commit whose tree it no longer holds.
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::missing()
                .with_create_result(Err(PortFailure("reviewed tree object is missing"))),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::CommitCreation(_))
        ));
        // Nothing was invented: this intent bound no delivery projection, so the
        // store is never asked for one.
        assert_eq!(harness.deliveries.get_calls(), 0);
        assert_eq!(harness.repository.materialize_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    /// §10: the object database is gone, so the reviewed tree must be rebuilt from
    /// the run's own durable delivery projection before the commit can be.
    #[test]
    fn a_lost_commit_and_tree_are_rebuilt_from_the_persisted_delivery_projection() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            // `missing_then_rebuilt` reports the commit absent, then present with
            // the reviewed tree after the recreation.
            FakeGitRepository::missing_then_rebuilt().with_materialized_tree(BASE, REVIEWED_TREE),
            vec![present(BASE), present(DESIRED)],
        );
        harness.deliveries = FakeDeliveryProjections::default().with(projection.clone());

        let execution = harness.execute().unwrap();
        let GitPublicationExecution::Published { cas_outcome, .. } = execution else {
            panic!("expected a published execution, got {execution:?}");
        };

        assert_eq!(cas_outcome, CasOutcome::Updated);
        assert_eq!(harness.deliveries.get_calls(), 1);
        assert_eq!(harness.repository.materialize_calls(), 1);
        assert_eq!(
            harness.repository.materialized_texts(),
            vec![projection.text().clone()],
            "recovery materializes exactly the stored text side"
        );
        assert_eq!(harness.repository.create_commit_calls(), 1);
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
    }

    #[test]
    fn a_delivery_projection_that_was_never_captured_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::missing().with_materialized_tree(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::DeliveryProjectionNotFound { .. })
        ));
        assert_eq!(harness.deliveries.get_calls(), 1);
        assert_eq!(harness.repository.materialize_calls(), 0);
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_delivery_projection_store_failure_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::missing().with_materialized_tree(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.deliveries = FakeDeliveryProjections::rejecting();

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::DeliveryProjectionLoad(_))
        ));
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_store_that_answers_with_another_projection_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::missing().with_materialized_tree(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        // The store hands back a projection whose own identity is another one.
        harness.deliveries = FakeDeliveryProjections::substituting(delivery_projection(5));

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::DeliveryProjectionIdentityMismatch { .. })
        ));
        assert_eq!(harness.repository.materialize_calls(), 0);
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_projection_whose_text_side_is_not_the_reviewed_tree_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                // The run recorded another text identity than the stored one.
                Some(Sha256::new([0xee; 32])),
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::missing().with_materialized_tree(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.deliveries = FakeDeliveryProjections::default().with(projection);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::DeliveryTextProjectionMismatch { .. })
        ));
        assert_eq!(harness.repository.materialize_calls(), 0);
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_tree_that_does_not_reproduce_the_reviewed_tree_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            // The projection rebuilds, but not into the tree the intent recorded.
            FakeGitRepository::missing().with_materialized_tree(BASE, 'f'),
            vec![present(BASE)],
        );
        harness.deliveries = FakeDeliveryProjections::default().with(projection);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::RematerializedTreeMismatch { .. })
        ));
        assert_eq!(harness.repository.materialize_calls(), 1);
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_tree_rebuilt_on_another_base_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::missing().with_materialized_tree('d', REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.deliveries = FakeDeliveryProjections::default().with(projection);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::RematerializedBaseMismatch { .. })
        ));
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    #[test]
    fn a_runtime_that_cannot_rematerialize_fails_closed() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::missing().with_materialization_error(),
            vec![present(BASE)],
        );
        harness.deliveries = FakeDeliveryProjections::default().with(projection);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::Rematerialization(_))
        ));
        assert_eq!(harness.repository.create_commit_calls(), 0);
        assert_eq!(harness.remote.compare_and_swap_calls(), 0);
    }

    /// A run written before delivery projections were captured has nothing to
    /// rebuild from, so it must not be forced through the store at all.
    #[test]
    fn a_legacy_intent_never_consults_the_delivery_projection_store() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::missing_then_rebuilt(),
            vec![present(BASE), present(DESIRED)],
        );

        let execution = harness.execute().unwrap();

        assert!(execution.is_satisfied());
        assert_eq!(harness.deliveries.get_calls(), 0);
        assert_eq!(harness.repository.materialize_calls(), 0);
        assert_eq!(harness.repository.create_commit_calls(), 1);
    }

    /// An intent whose commit is still present needs no recovery at all.
    #[test]
    fn a_present_commit_never_consults_the_delivery_projection_store() {
        let projection = delivery_projection(4);
        let mut harness = Harness::new(
            Some(bound_intent(
                &projection,
                None,
                Some(oid(DESIRED)),
                Some(spec()),
            )),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE), present(DESIRED)],
        );

        let execution = harness.execute().unwrap();

        assert!(execution.is_satisfied());
        assert_eq!(harness.deliveries.get_calls(), 0);
        assert_eq!(harness.repository.materialize_calls(), 0);
        assert_eq!(harness.repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_rejected_compare_and_swap_is_a_lost_race_not_an_execution_failure() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.remote =
            CountingGitRemote::new(vec![present(BASE)]).with_cas_outcome(CasOutcome::Rejected);

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::PushFailedButRemoteUnchanged { .. }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
    }

    #[test]
    fn a_push_after_which_the_remote_holds_something_else_is_a_conflict() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE), present('e')],
        );

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::RemoteChanged {
                cas_outcome: Some(CasOutcome::Updated),
                ..
            }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
    }

    #[test]
    fn a_successful_push_that_did_not_move_the_remote_is_not_published() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE), present(BASE)],
        );

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::RemoteUnchangedAfterSuccessfulPush { .. }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
    }

    #[test]
    fn a_post_attempt_observation_failure_is_indeterminate() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.remote = CountingGitRemote::new(vec![present(BASE)]).failing_observations_after(1);

        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::Indeterminate {
                cas_outcome: Some(CasOutcome::Updated)
            }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
        assert_eq!(harness.observations.saved().len(), 1);
    }

    #[test]
    fn a_post_observation_that_cannot_be_persisted_is_indeterminate() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE), present(DESIRED)],
        );
        harness.observations = FakeObservations::rejecting_after(1);

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::PostObservationPersistence(_))
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
        assert_eq!(harness.observations.saved().len(), 1);
    }

    #[test]
    fn a_runtime_that_reuses_an_observation_identity_is_rejected() {
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE)],
        );
        harness.ids = TestIds::Repeating;

        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::PostObservationIdReused)
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
    }

    #[test]
    fn a_crash_after_the_push_is_recovered_without_a_second_push() {
        // The push lands but its audit fact cannot be stored, so the attempt is
        // reported as a failure to record rather than as a publication.
        let mut harness = Harness::new(
            Some(ready_intent()),
            FakeGitRepository::present(BASE, REVIEWED_TREE),
            vec![present(BASE), present(DESIRED)],
        );
        harness.observations = FakeObservations::rejecting_after(1);
        assert!(matches!(
            harness.execute(),
            Err(GitPublicationExecuteError::PostObservationPersistence(_))
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);

        // The next attempt reloads the durable intent, observes the remote holding
        // the desired commit, and pushes nothing.
        harness.observations.accept_everything();
        assert!(matches!(
            harness.execute().unwrap(),
            GitPublicationExecution::AlreadyPublished { .. }
        ));
        assert_eq!(harness.remote.compare_and_swap_calls(), 1);
        assert_eq!(harness.remote.observe_ref_calls(), 3);
        assert_eq!(harness.repository.create_commit_calls(), 0);
    }
}
