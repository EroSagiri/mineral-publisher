use std::{error::Error, fmt};

use crate::{
    domain::{Sha256, TimestampMillis},
    publish::{
        DeliveryProjectionBinding, PublishRun, PublishRunError, PublishRunId, PublishTargetId,
        RepositoryLocator,
    },
    workflow::{PublishPlan, PublishPlanError, TextProjection},
};

use super::{
    GitCommitOid, GitCommitSpec, GitCommitSpecError, GitRefTarget, GitRepository, GitTreeOid,
    GitTreeOidError, ReviewedGitTree,
};

/// Everything one prepare attempt needs, with the platform facts already frozen.
///
/// The runtime supplies the identity it resolved, the remote base it observed, the
/// complete desired projection, the inputs that determine the commit identity, and
/// the single attempt instant. The engine decides whether those facts agree with
/// each other and what the resulting intent is; it reads no clock, no
/// configuration, and no path of its own.
#[derive(Clone, Copy, Debug)]
pub struct GitPublicationPrepareRequest<'a> {
    pub id: PublishRunId,
    /// Stable audit identity of the logical publication target.
    pub target_id: &'a PublishTargetId,
    /// Opaque locator the runtime resolves; the engine only stores it.
    pub repository: &'a RepositoryLocator,
    pub target: &'a GitRefTarget,
    /// The complete delivery text side the target must end up holding.
    ///
    /// The engine hands over the already-built [`TextProjection`], so the runtime
    /// never sees the delivery rules: which assets exist, where they live, and
    /// which URLs replaced which references were all decided upstream.
    pub text_projection: &'a TextProjection,
    /// The exact durable delivery projection this attempt is delivering.
    ///
    /// The runtime carried it from the delivery stage; the engine checks that the
    /// binding and the text side it is about to materialize describe the same
    /// tree before anything is read or written.
    pub delivery: DeliveryProjectionBinding,
    /// The base the target currently holds, as the runtime observed it.
    pub observed_base: &'a GitCommitOid,
    /// V1 uses one identity for both the author and the committer.
    pub author_name: &'a str,
    pub author_email: &'a str,
    pub message: &'a str,
    /// The frozen instant of this attempt. Every commit-identity time is this one.
    pub created_at: TimestampMillis,
}

/// What one prepare attempt decided: the intent, plus the identity of the plan it
/// was derived from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPublicationPreparation {
    publish_run: PublishRun,
    publish_plan_sha256: Sha256,
}

impl GitPublicationPreparation {
    pub fn publish_run(&self) -> &PublishRun {
        &self.publish_run
    }

    pub fn into_publish_run(self) -> PublishRun {
        self.publish_run
    }

    pub fn publish_plan_sha256(&self) -> Sha256 {
        self.publish_plan_sha256
    }
}

/// Builds the immutable publication intent for one reviewed projection.
///
/// This is the whole prepare stage and nothing more: it reads the current target
/// through the [`GitRepository`] port, derives the plan, materializes the reviewed
/// tree, checks that the facts agree, decides whether the attempt is a Noop, freezes
/// the commit identity, creates the commit object, and returns the intent. It never
/// persists anything, never touches a remote, and never observes or reconciles.
///
/// The runtime reports facts; this engine decides whether to trust them. A base the
/// runtime resolved differently from the base it observed, a reviewed tree that does
/// not belong to that base, a plan that disagrees with the materialized tree, or an
/// unusable commit identity all fail closed, and a Noop never reaches
/// `create_commit` at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitPublicationPreparer;

impl GitPublicationPreparer {
    pub fn prepare<R: GitRepository>(
        repository: &R,
        request: &GitPublicationPrepareRequest<'_>,
    ) -> Result<GitPublicationPreparation, GitPublicationPrepareError<R::Error>> {
        // The binding and the text side must agree before any repository fact is
        // read: a run that pointed at another projection would otherwise be
        // discovered only after a materialization had already happened.
        if request.delivery.text_projection_sha256() != request.text_projection.projection_sha256()
        {
            return Err(GitPublicationPrepareError::DeliveryTextProjectionMismatch {
                text_projection_sha256: request.text_projection.projection_sha256(),
                bound_text_projection_sha256: request.delivery.text_projection_sha256(),
            });
        }
        let current = repository
            .read_current(
                request.observed_base,
                request.text_projection.managed_root(),
            )
            .map_err(GitPublicationPrepareError::CurrentTarget)?;
        let resolved_base = GitCommitOid::new(current.base_commit())
            .map_err(|_| GitPublicationPrepareError::ResolvedBaseInvalid)?;
        if &resolved_base != request.observed_base {
            return Err(GitPublicationPrepareError::ResolvedBaseMismatch {
                expected: request.observed_base.clone(),
                resolved: current.base_commit().to_owned(),
            });
        }

        let plan = PublishPlan::build(current.state(), request.text_projection)
            .map_err(GitPublicationPrepareError::PublishPlan)?;
        let reviewed = repository
            .materialize(&resolved_base, request.text_projection)
            .map_err(GitPublicationPrepareError::Materialization)?;
        if reviewed.base_commit() != resolved_base.as_str() {
            return Err(GitPublicationPrepareError::MaterializedBaseMismatch {
                expected: resolved_base,
                actual: reviewed.base_commit().to_owned(),
            });
        }
        if plan.operations().is_empty() != reviewed.is_noop() {
            return Err(GitPublicationPrepareError::PlanReviewedTreeMismatch);
        }

        if reviewed.is_noop() {
            // The target already holds the complete desired state: this attempt
            // creates no commit object and freezes no commit identity.
            return Self::build(request, &reviewed, None, None, plan.plan_sha256());
        }

        let tree = GitTreeOid::new(reviewed.tree_oid())
            .map_err(GitPublicationPrepareError::ReviewedTreeInvalid)?;
        let spec = GitCommitSpec::new(
            resolved_base,
            tree,
            request.author_name,
            request.author_email,
            request.created_at,
            request.author_name,
            request.author_email,
            request.created_at,
            request.message,
        )
        .map_err(GitPublicationPrepareError::CommitSpec)?;
        // The very specification that produced the commit object is the one the
        // intent freezes, so the persisted identity can rebuild this commit later.
        let desired_commit = repository
            .create_commit(&spec)
            .map_err(GitPublicationPrepareError::CreateCommit)?;
        Self::build(
            request,
            &reviewed,
            Some(desired_commit),
            Some(spec),
            plan.plan_sha256(),
        )
    }

    fn build<E: Error>(
        request: &GitPublicationPrepareRequest<'_>,
        reviewed: &ReviewedGitTree,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
        publish_plan_sha256: Sha256,
    ) -> Result<GitPublicationPreparation, GitPublicationPrepareError<E>> {
        let publish_run = PublishRun::from_reviewed_tree(
            request.id,
            request.target_id.clone(),
            request.repository.clone(),
            request.target.clone(),
            reviewed,
            &request.delivery,
            desired_commit,
            commit_spec,
            request.created_at,
        )
        .map_err(GitPublicationPrepareError::PublishRun)?;
        Ok(GitPublicationPreparation {
            publish_run,
            publish_plan_sha256,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum GitPublicationPrepareError<PortError: Error> {
    /// The runtime could not read the current target through its port.
    CurrentTarget(PortError),
    /// The commit the runtime resolved is not a usable object identity.
    ResolvedBaseInvalid,
    /// The delivery binding and the text side do not describe the same tree.
    DeliveryTextProjectionMismatch {
        text_projection_sha256: Sha256,
        bound_text_projection_sha256: Sha256,
    },
    /// The runtime resolved a different base than the one it observed.
    ResolvedBaseMismatch {
        expected: GitCommitOid,
        resolved: String,
    },
    PublishPlan(PublishPlanError),
    /// The runtime could not materialize the reviewed tree.
    Materialization(PortError),
    /// The reviewed tree was materialized from another base than the trusted one.
    MaterializedBaseMismatch {
        expected: GitCommitOid,
        actual: String,
    },
    /// The plan and the materialized tree disagree about whether there is work.
    PlanReviewedTreeMismatch,
    /// The materialized tree is not a usable object identity.
    ReviewedTreeInvalid(GitTreeOidError),
    /// The frozen commit-identity inputs are unusable.
    CommitSpec(GitCommitSpecError),
    /// The runtime could not create the commit object.
    CreateCommit(PortError),
    PublishRun(PublishRunError),
}

impl<PortError: Error> fmt::Display for GitPublicationPrepareError<PortError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentTarget(error) => {
                write!(formatter, "could not read the current Git target: {error}")
            }
            Self::ResolvedBaseInvalid => {
                formatter.write_str("the resolved Git base is not a usable commit identity")
            }
            Self::DeliveryTextProjectionMismatch { .. } => formatter.write_str(
                "the delivery binding names another text projection than the one being prepared",
            ),
            Self::ResolvedBaseMismatch { expected, .. } => write!(
                formatter,
                "the resolved Git base is not the observed base {}",
                expected.as_str()
            ),
            Self::PublishPlan(error) => write!(formatter, "could not derive publish plan: {error}"),
            Self::Materialization(error) => write!(
                formatter,
                "could not materialize the reviewed Git tree: {error}"
            ),
            Self::MaterializedBaseMismatch { expected, .. } => write!(
                formatter,
                "the reviewed Git tree was materialized from another base than {}",
                expected.as_str()
            ),
            Self::PlanReviewedTreeMismatch => {
                formatter.write_str("publish plan and reviewed Git tree disagree about noop state")
            }
            Self::ReviewedTreeInvalid(error) => {
                write!(formatter, "the reviewed Git tree is unusable: {error}")
            }
            Self::CommitSpec(error) => {
                write!(formatter, "could not freeze Git commit identity: {error}")
            }
            Self::CreateCommit(error) => {
                write!(formatter, "could not create the Git commit object: {error}")
            }
            Self::PublishRun(error) => {
                write!(formatter, "could not construct the publish run: {error}")
            }
        }
    }
}

impl<PortError: Error + 'static> Error for GitPublicationPrepareError<PortError> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentTarget(error) => Some(error),
            Self::Materialization(error) => Some(error),
            Self::CreateCommit(error) => Some(error),
            Self::PublishPlan(error) => Some(error),
            Self::ReviewedTreeInvalid(error) => Some(error),
            Self::CommitSpec(error) => Some(error),
            Self::PublishRun(error) => Some(error),
            Self::ResolvedBaseInvalid
            | Self::DeliveryTextProjectionMismatch { .. }
            | Self::ResolvedBaseMismatch { .. }
            | Self::MaterializedBaseMismatch { .. }
            | Self::PlanReviewedTreeMismatch => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, error::Error, fmt};

    use crate::{
        domain::{ContentPath, Sha256, SnapshotId},
        publication::git::{GitCurrentTarget, LocalCommitState},
        workflow::{
            CurrentTargetEntry, CurrentTargetState, ManagedRoot, ProjectionTargetPath,
            TextProjection,
        },
    };

    use super::*;

    const BASE: char = 'a';
    const BASE_TREE: char = '9';
    const REVIEWED_TREE: char = 'b';
    const CREATED_COMMIT: char = 'c';
    const TIME: u64 = 1_500;

    fn oid(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }

    fn managed_root() -> ManagedRoot {
        ManagedRoot::new("content").unwrap()
    }

    /// A text projection whose single document is the [1; 32] blob, so a current
    /// state can be made to match it or to differ from it.
    fn projection() -> TextProjection {
        TextProjection::from_parts_for_test(
            SnapshotId::new(7).unwrap(),
            managed_root(),
            Sha256::new([1; 32]),
            vec![(ContentPath::new("note.md").unwrap(), Sha256::new([1; 32]))],
        )
    }

    fn current_state(blob_sha256: [u8; 32]) -> CurrentTargetState {
        CurrentTargetState::new(
            managed_root(),
            vec![CurrentTargetEntry::new(
                ProjectionTargetPath::new("content/note.md").unwrap(),
                Sha256::new(blob_sha256),
            )],
        )
        .unwrap()
    }

    fn differing_state() -> CurrentTargetState {
        current_state([2; 32])
    }

    fn matching_state() -> CurrentTargetState {
        current_state([1; 32])
    }

    /// The fact a runtime reports after materializing [`projection`]: its
    /// projection identity is that projection's text identity.
    fn reviewed(base: char, base_tree: char, tree: char) -> ReviewedGitTree {
        ReviewedGitTree::from_parts(
            oid(base),
            oid(base_tree),
            projection().projection_sha256(),
            SnapshotId::new(7).unwrap(),
            oid(tree),
            managed_root(),
        )
    }

    fn observed_base() -> GitCommitOid {
        GitCommitOid::new(oid(BASE)).unwrap()
    }

    struct Fixture {
        target_id: PublishTargetId,
        repository: RepositoryLocator,
        target: GitRefTarget,
        author_name: String,
        author_email: String,
        message: String,
    }

    fn fixture() -> Fixture {
        Fixture {
            target_id: PublishTargetId::new("origin:refs/heads/main").unwrap(),
            repository: RepositoryLocator::new("/srv/public-repo").unwrap(),
            target: GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            author_name: "Mineral Publisher".to_owned(),
            author_email: "publisher@example.invalid".to_owned(),
            message: "Publish Mineral content".to_owned(),
        }
    }

    impl Fixture {
        fn request<'a>(
            &'a self,
            text_projection: &'a TextProjection,
            observed_base: &'a GitCommitOid,
        ) -> GitPublicationPrepareRequest<'a> {
            GitPublicationPrepareRequest {
                id: PublishRunId::new(1).unwrap(),
                target_id: &self.target_id,
                repository: &self.repository,
                target: &self.target,
                text_projection,
                delivery: DeliveryProjectionBinding::new(
                    Sha256::new([9; 32]),
                    text_projection.projection_sha256(),
                ),
                observed_base,
                author_name: &self.author_name,
                author_email: &self.author_email,
                message: &self.message,
                created_at: TimestampMillis::from_unix_millis(TIME),
            }
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

    /// A `GitRepository` whose every answer is chosen by the test, so the prepare
    /// stage can be shown to depend on port behaviour and on nothing else.
    struct FakeRepository {
        current_base: Result<String, PortFailure>,
        state: CurrentTargetState,
        reviewed: Result<ReviewedGitTree, PortFailure>,
        created: Result<String, PortFailure>,
        read_current_calls: Cell<u32>,
        materialize_calls: Cell<u32>,
        create_commit_calls: Cell<u32>,
        created_specs: std::cell::RefCell<Vec<GitCommitSpec>>,
    }

    impl FakeRepository {
        fn new(state: CurrentTargetState, reviewed: ReviewedGitTree) -> Self {
            Self {
                current_base: Ok(oid(BASE)),
                state,
                reviewed: Ok(reviewed),
                created: Ok(oid(CREATED_COMMIT)),
                read_current_calls: Cell::new(0),
                materialize_calls: Cell::new(0),
                create_commit_calls: Cell::new(0),
                created_specs: std::cell::RefCell::new(Vec::new()),
            }
        }

        /// A real change: the plan has work and the materialized tree differs from
        /// the base tree.
        fn real_change() -> Self {
            Self::new(differing_state(), reviewed(BASE, BASE_TREE, REVIEWED_TREE))
        }

        /// Nothing to do: the plan is empty and the materialized tree is the base
        /// tree.
        fn noop() -> Self {
            Self::new(matching_state(), reviewed(BASE, BASE_TREE, BASE_TREE))
        }

        fn with_current_base(mut self, base: &str) -> Self {
            self.current_base = Ok(base.to_owned());
            self
        }

        fn with_unusable_current_base(mut self) -> Self {
            self.current_base = Ok("not-a-commit".to_owned());
            self
        }

        fn with_current_error(mut self) -> Self {
            self.current_base = Err(PortFailure("read_current failed"));
            self
        }

        fn with_materialization_error(mut self) -> Self {
            self.reviewed = Err(PortFailure("materialize failed"));
            self
        }

        fn with_creation_error(mut self) -> Self {
            self.created = Err(PortFailure("create_commit failed"));
            self
        }

        fn read_current_calls(&self) -> u32 {
            self.read_current_calls.get()
        }

        fn materialize_calls(&self) -> u32 {
            self.materialize_calls.get()
        }

        fn create_commit_calls(&self) -> u32 {
            self.create_commit_calls.get()
        }

        fn created_specs(&self) -> Vec<GitCommitSpec> {
            self.created_specs.borrow().clone()
        }
    }

    impl GitRepository for FakeRepository {
        type Error = PortFailure;

        fn read_current(
            &self,
            _: &GitCommitOid,
            _: &ManagedRoot,
        ) -> Result<GitCurrentTarget, Self::Error> {
            self.read_current_calls
                .set(self.read_current_calls.get() + 1);
            self.current_base
                .clone()
                .map(|base| GitCurrentTarget::from_parts(base, self.state.clone()))
        }

        fn materialize(
            &self,
            _: &GitCommitOid,
            _: &TextProjection,
        ) -> Result<ReviewedGitTree, Self::Error> {
            self.materialize_calls.set(self.materialize_calls.get() + 1);
            self.reviewed.clone()
        }

        fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error> {
            self.create_commit_calls
                .set(self.create_commit_calls.get() + 1);
            self.created_specs.borrow_mut().push(spec.clone());
            self.created
                .clone()
                .map(|value| GitCommitOid::new(value).unwrap())
        }

        fn inspect_commit(&self, _: &GitCommitOid) -> Result<LocalCommitState, Self::Error> {
            unreachable!("preparation never inspects a commit object")
        }
    }

    #[test]
    fn a_real_change_creates_exactly_one_commit_and_freezes_the_identity_it_used() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change();
        let request = fixture.request(&projection, &base);

        let preparation = GitPublicationPreparer::prepare(&repository, &request).unwrap();
        let run = preparation.publish_run();

        assert_eq!(repository.read_current_calls(), 1);
        assert_eq!(repository.materialize_calls(), 1);
        assert_eq!(repository.create_commit_calls(), 1);
        assert_eq!(
            run.desired_commit(),
            Some(&GitCommitOid::new(oid(CREATED_COMMIT)).unwrap())
        );
        assert_eq!(run.base_commit(), oid(BASE));
        assert_eq!(run.reviewed_tree(), oid(REVIEWED_TREE));
        assert_eq!(run.created_at_unix_ms(), TIME);

        let spec = run
            .commit_spec()
            .expect("a new non-Noop run freezes the identity of its commit");
        assert_eq!(spec.parent().as_str(), oid(BASE));
        assert_eq!(spec.tree().as_str(), oid(REVIEWED_TREE));
        assert_eq!(spec.author_time().as_unix_millis(), TIME);
        assert_eq!(spec.committer_time().as_unix_millis(), TIME);
        assert_eq!(spec.author_name(), "Mineral Publisher");
        assert_eq!(spec.message(), "Publish Mineral content");
        // The identity the port actually used is the one that was persisted.
        assert_eq!(repository.created_specs(), vec![spec.clone()]);
        assert_eq!(
            preparation.publish_plan_sha256(),
            PublishPlan::build(&differing_state(), &projection)
                .unwrap()
                .plan_sha256()
        );
    }

    #[test]
    fn a_noop_never_creates_a_commit_and_freezes_nothing() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::noop();
        let request = fixture.request(&projection, &base);

        let preparation = GitPublicationPreparer::prepare(&repository, &request).unwrap();
        let run = preparation.publish_run();

        assert_eq!(repository.read_current_calls(), 1);
        assert_eq!(repository.materialize_calls(), 1);
        assert_eq!(repository.create_commit_calls(), 0);
        assert!(repository.created_specs().is_empty());
        assert_eq!(run.desired_commit(), None);
        assert_eq!(run.commit_spec(), None);
        assert_eq!(
            preparation.publish_plan_sha256(),
            PublishPlan::build(&matching_state(), &projection)
                .unwrap()
                .plan_sha256()
        );
    }

    #[test]
    fn a_base_the_runtime_resolved_differently_fails_closed_before_materializing() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change().with_current_base(&oid('d'));
        let request = fixture.request(&projection, &base);

        assert_eq!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::ResolvedBaseMismatch {
                expected: base,
                resolved: oid('d'),
            })
        );
        assert_eq!(repository.materialize_calls(), 0);
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_binding_that_names_another_text_projection_fails_closed_before_reading_anything() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change();
        let mut request = fixture.request(&projection, &base);
        request.delivery =
            DeliveryProjectionBinding::new(Sha256::new([2; 32]), Sha256::new([3; 32]));

        assert_eq!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::DeliveryTextProjectionMismatch {
                text_projection_sha256: projection.projection_sha256(),
                bound_text_projection_sha256: Sha256::new([3; 32]),
            })
        );
        assert_eq!(repository.read_current_calls(), 0);
        assert_eq!(repository.materialize_calls(), 0);
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn the_intent_is_bound_to_the_delivery_projection_that_was_prepared() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change();
        let request = fixture.request(&projection, &base);

        let preparation = GitPublicationPreparer::prepare(&repository, &request).unwrap();
        let run = preparation.publish_run();

        assert_eq!(
            run.delivery_projection_binding(),
            Some(DeliveryProjectionBinding::new(
                request.delivery.delivery_sha256(),
                projection.projection_sha256()
            ))
        );
        assert_eq!(
            run.reviewed_text_projection_sha256(),
            Some(projection.projection_sha256())
        );
    }

    #[test]
    fn an_unusable_resolved_base_fails_closed() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change().with_unusable_current_base();
        let request = fixture.request(&projection, &base);

        assert_eq!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::ResolvedBaseInvalid)
        );
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_reviewed_tree_from_another_base_fails_closed() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository =
            FakeRepository::new(differing_state(), reviewed('d', BASE_TREE, REVIEWED_TREE));
        let request = fixture.request(&projection, &base);

        assert_eq!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::MaterializedBaseMismatch {
                expected: base,
                actual: oid('d'),
            })
        );
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_plan_and_a_reviewed_tree_that_disagree_fail_closed() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        // The plan is empty but the materialized tree claims a real change.
        let repository =
            FakeRepository::new(matching_state(), reviewed(BASE, BASE_TREE, REVIEWED_TREE));
        let request = fixture.request(&projection, &base);

        assert_eq!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::PlanReviewedTreeMismatch)
        );
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_read_current_failure_is_reported_without_materializing() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change().with_current_error();
        let request = fixture.request(&projection, &base);

        assert!(matches!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::CurrentTarget(_))
        ));
        assert_eq!(repository.materialize_calls(), 0);
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_materialization_failure_is_reported_without_creating_a_commit() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change().with_materialization_error();
        let request = fixture.request(&projection, &base);

        assert!(matches!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::Materialization(_))
        ));
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn an_unusable_commit_identity_fails_closed_before_creating_a_commit() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change();
        let mut request = fixture.request(&projection, &base);
        request.message = "";

        assert!(matches!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::CommitSpec(_))
        ));
        assert_eq!(repository.create_commit_calls(), 0);
    }

    #[test]
    fn a_commit_creation_failure_produces_no_intent() {
        let fixture = fixture();
        let projection = projection();
        let base = observed_base();
        let repository = FakeRepository::real_change().with_creation_error();
        let request = fixture.request(&projection, &base);

        assert!(matches!(
            GitPublicationPreparer::prepare(&repository, &request),
            Err(GitPublicationPrepareError::CreateCommit(_))
        ));
        assert_eq!(repository.create_commit_calls(), 1);
        assert_eq!(repository.created_specs().len(), 1);
    }
}
