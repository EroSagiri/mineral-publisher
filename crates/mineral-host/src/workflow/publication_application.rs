use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    path::Path,
    time::SystemTime,
};

use crate::{
    domain::Snapshot,
    policy::{PolicyIdentity, ReviewRunStore, Reviewer},
    ports::BlobStore,
    publish::PublishTargetId,
    publisher::{
        GitCommitMetadata, GitPublicationApplication, GitPublicationApplicationResult,
        GitRefTarget, PublishRunIdGenerator, PublishRunStore, RemoteObservationIdGenerator,
        RemoteObservationStore,
    },
};

use crate::asset::{AssetObservationIdGenerator, AssetObservationStore, AssetTarget};

use super::{
    AssetDeliveryConfig, AssetProgramCheck, AssetReviewEvaluator, AssetReviewRunIdGenerator,
    AssetReviewRunStore, AssetReviewWorkflow, AssetReviewWorkflowInput, AssetReviewWorkflowResult,
    AssetReviewer, AssetSanitizer, CandidateAssetSet, DeliveryProjectionStore, EffectiveReviewSet,
    FinalPublicationSet, HumanReviewAttempt, HumanReviewBinding, HumanReviewKind,
    HumanReviewRecord, HumanReviewStore, HumanReviewSubject, ManagedRoot, MarkdownReviewEvaluator,
    PublicPolicyRun, PublicPolicyRunResult, PublicProjection, ReviewRunIdGenerator,
};

/// Caller-selected, durable human decisions.  The application never discovers
/// a decision by listing reviews or by asking for the latest record.
#[derive(Clone, Debug, Default)]
pub struct ExplicitHumanReviewSelection {
    records: Vec<HumanReviewRecord>,
}

impl ExplicitHumanReviewSelection {
    pub fn new(records: Vec<HumanReviewRecord>) -> Self {
        Self { records }
    }
    pub fn records(&self) -> &[HumanReviewRecord] {
        &self.records
    }
}

/// Immutable configuration and explicit identities for one publication run.
///
/// This carries data only. Runtime concerns — how review work is executed, what
/// the clock says, and which publisher applies the result — are supplied to
/// [`PublicationApplication::run`] as separate dependencies.
pub struct PublicationApplicationRequest<'a> {
    pub snapshot: &'a Snapshot,
    pub markdown_policy: &'a PolicyIdentity,
    pub asset_policy: &'a PolicyIdentity,
    pub repository: &'a Path,
    /// Stable audit identity of the logical target, independent of which remote
    /// and ref currently implement it.
    pub target_id: PublishTargetId,
    pub target: GitRefTarget,
    pub commit_metadata: &'a GitCommitMetadata,
    pub human_reviews: ExplicitHumanReviewSelection,
    /// Where delivered binary assets will be served from. This is a frozen
    /// input: the application never reads configuration or the environment.
    pub asset_delivery: &'a AssetDeliveryConfig,
}

#[derive(Clone, Debug)]
pub struct PublicationTrace {
    snapshot: Snapshot,
    markdown_reviews: PublicPolicyRunResult,
    asset_reviews: AssetReviewWorkflowResult,
    effective_reviews: EffectiveReviewSet,
}
impl PublicationTrace {
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub fn markdown_reviews(&self) -> &PublicPolicyRunResult {
        &self.markdown_reviews
    }
    pub fn asset_reviews(&self) -> &AssetReviewWorkflowResult {
        &self.asset_reviews
    }
    pub fn effective_reviews(&self) -> &EffectiveReviewSet {
        &self.effective_reviews
    }
}

#[derive(Clone, Debug)]
pub enum PublicationApplicationOutcome {
    NeedsHumanReview {
        trace: PublicationTrace,
    },
    Completed {
        trace: PublicationTrace,
        completed: Box<CompletedPublication>,
    },
}

#[derive(Clone, Debug)]
pub struct CompletedPublication {
    publication_set: FinalPublicationSet,
    projection: PublicProjection,
    publication: GitPublicationApplicationResult,
}
impl CompletedPublication {
    pub fn publication_set(&self) -> &FinalPublicationSet {
        &self.publication_set
    }
    pub fn projection(&self) -> &PublicProjection {
        &self.projection
    }
    pub fn publication(&self) -> &GitPublicationApplicationResult {
        &self.publication
    }
}

#[derive(Debug)]
pub enum PublicationApplicationError {
    Stage {
        stage: &'static str,
        source: Box<dyn Error>,
    },
    /// Boxed: a binding carries the whole reviewed subject, and this error is
    /// returned by every publication entry point.
    DuplicateHumanReviewSelection(Box<HumanReviewBinding>),
    HumanReviewNotPersisted(Box<HumanReviewBinding>),
    IrrelevantHumanReviewSelection(Box<HumanReviewBinding>),
    MissingDependencyGraph,
}
impl fmt::Display for PublicationApplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stage { stage, source } => write!(f, "{stage} failed: {source}"),
            Self::DuplicateHumanReviewSelection(subject) => {
                write!(f, "human review selection repeats {subject:?}")
            }
            Self::HumanReviewNotPersisted(subject) => write!(
                f,
                "selected human review for {subject:?} is not the durable record"
            ),
            Self::IrrelevantHumanReviewSelection(subject) => write!(
                f,
                "selected human review for {subject:?} is not part of this run"
            ),
            Self::MissingDependencyGraph => {
                f.write_str("completed Markdown policy run did not retain its dependency graph")
            }
        }
    }
}
impl Error for PublicationApplicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Stage { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Thin application orchestration: it composes existing stages without adding
/// policy authority or publication behavior.
pub struct PublicationApplication;
impl PublicationApplication {
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn run<R, AR, D, A, H, P, DP, T, S, G, O, DI, AI, PI, OI, B>(
        request: PublicationApplicationRequest<'_>,
        content_store: &B,
        markdown_reviewer: &R,
        asset_reviewer: &AR,
        document_runs: &D,
        asset_runs: &A,
        human_store: &H,
        publish_runs: &P,
        delivery_projections: &DP,
        asset_target: &T,
        asset_observations: &S,
        asset_observation_ids: &mut G,
        observations: &O,
        document_ids: &mut DI,
        asset_ids: &mut AI,
        publish_ids: &mut PI,
        observation_ids: &mut OI,
        created_at: SystemTime,
        markdown_evaluator: &dyn MarkdownReviewEvaluator<R>,
        asset_evaluator: &dyn AssetReviewEvaluator<AR>,
    ) -> Result<PublicationApplicationOutcome, PublicationApplicationError>
    where
        R: Reviewer + Sync + ?Sized,
        AR: AssetReviewer + Sync + ?Sized,
        D: ReviewRunStore + ?Sized,
        A: AssetReviewRunStore + ?Sized,
        H: HumanReviewStore + ?Sized,
        P: PublishRunStore,
        DP: DeliveryProjectionStore,
        T: AssetTarget,
        S: AssetObservationStore,
        G: AssetObservationIdGenerator,
        O: RemoteObservationStore,
        DI: ReviewRunIdGenerator + ?Sized,
        AI: AssetReviewRunIdGenerator + ?Sized,
        PI: PublishRunIdGenerator,
        OI: RemoteObservationIdGenerator,
        B: BlobStore + Clone,
        D::Error: 'static,
        A::Error: 'static,
        H::Error: 'static,
        P::Error: 'static,
        DP::Error: 'static,
        T::Error: 'static,
        S::Error: 'static,
        G::Error: 'static,
        O::Error: 'static,
        DI::Error: 'static,
        AI::Error: 'static,
        PI::Error: 'static,
        OI::Error: 'static,
    {
        let documents = PublicPolicyRun::execute_at(
            request.snapshot,
            content_store,
            markdown_reviewer,
            document_runs,
            human_store,
            request.markdown_policy,
            document_ids,
            created_at,
            markdown_evaluator,
        )
        .map_err(|e| stage("markdown review", e))?;
        validate_explicit_selection(
            &request.human_reviews,
            human_store,
            &documents,
            None,
            request.snapshot,
            request.asset_policy,
        )?;
        let no_assets = AssetReviewWorkflowResult::empty(request.snapshot.id());
        let document_effective = EffectiveReviewSet::build(
            &documents,
            &no_assets,
            document_runs,
            asset_runs,
            human_store,
        )
        .map_err(|e| stage("Markdown human resolution", e))?;
        if document_effective.has_pending_review() {
            return Ok(PublicationApplicationOutcome::NeedsHumanReview {
                trace: PublicationTrace {
                    snapshot: request.snapshot.clone(),
                    markdown_reviews: documents,
                    asset_reviews: no_assets,
                    effective_reviews: document_effective,
                },
            });
        }
        let graph = documents
            .dependency_graph()
            .cloned()
            .ok_or(PublicationApplicationError::MissingDependencyGraph)?;
        let candidates = CandidateAssetSet::select_effective(&document_effective, &graph)
            .map_err(|e| stage("candidate asset selection", e))?;
        let assets = AssetReviewWorkflow::execute_at(
            AssetReviewWorkflowInput::new(
                &candidates,
                request.snapshot,
                &AssetProgramCheck::new(content_store.clone()),
                request.asset_policy,
            ),
            asset_reviewer,
            asset_runs,
            human_store,
            asset_ids,
            created_at,
            asset_evaluator,
        )
        .map_err(|e| stage("asset review", e))?;
        validate_explicit_selection(
            &request.human_reviews,
            human_store,
            &documents,
            Some(&assets),
            request.snapshot,
            request.asset_policy,
        )?;
        let effective =
            EffectiveReviewSet::build(&documents, &assets, document_runs, asset_runs, human_store)
                .map_err(|e| stage("effective review selection", e))?;
        let trace = PublicationTrace {
            snapshot: request.snapshot.clone(),
            markdown_reviews: documents,
            asset_reviews: assets,
            effective_reviews: effective,
        };
        if trace.effective_reviews.has_pending_review() {
            return Ok(PublicationApplicationOutcome::NeedsHumanReview { trace });
        }
        let sanitized = AssetSanitizer::new(content_store.clone())
            .sanitize(
                trace.effective_reviews(),
                trace.asset_reviews().checks(),
                request.snapshot,
            )
            .map_err(|e| stage("asset sanitization", e))?;
        let publication_set = FinalPublicationSet::close(
            trace.effective_reviews(),
            &graph,
            &sanitized,
            request.snapshot,
        )
        .map_err(|e| stage("final dependency closure", e))?;
        let projection = PublicProjection::build(
            &publication_set,
            request.snapshot,
            ManagedRoot::repository_root(),
        )
        .map_err(|e| stage("public projection", e))?;
        let publication = GitPublicationApplication::prepare_and_publish(
            &projection,
            request.snapshot,
            request.asset_delivery,
            request.repository,
            request.target_id,
            request.target,
            request.commit_metadata,
            content_store,
            publish_runs,
            delivery_projections,
            asset_target,
            asset_observations,
            asset_observation_ids,
            observations,
            publish_ids,
            observation_ids,
        )
        .map_err(|e| stage("Git publication", e))?;
        Ok(PublicationApplicationOutcome::Completed {
            trace,
            completed: Box::new(CompletedPublication {
                publication_set,
                projection,
                publication,
            }),
        })
    }
}

fn stage(stage: &'static str, source: impl Error + 'static) -> PublicationApplicationError {
    PublicationApplicationError::Stage {
        stage,
        source: Box::new(source),
    }
}

/// Validates an explicit human-review selection against the runs of this attempt.
///
/// Durable decisions are found by the subject they decided about, so this is only
/// an assertion for callers that hand in records explicitly: every selected record
/// must answer a review this attempt actually produced, must not repeat another
/// selection, and must already be persisted exactly as given. A record written
/// before subjects were bound answers the attempt it named, and is matched that way.
fn validate_explicit_selection<H: HumanReviewStore + ?Sized>(
    selection: &ExplicitHumanReviewSelection,
    store: &H,
    documents: &PublicPolicyRunResult,
    assets: Option<&AssetReviewWorkflowResult>,
    snapshot: &Snapshot,
    asset_policy: &PolicyIdentity,
) -> Result<(), PublicationApplicationError> {
    let valid_subjects = documents
        .document_outcomes()
        .iter()
        .map(HumanReviewSubject::document)
        .chain(assets.into_iter().flat_map(|items| {
            items.entries().iter().filter_map(|entry| {
                let file = snapshot
                    .files()
                    .iter()
                    .find(|file| file.path() == entry.content_path())?;
                Some(HumanReviewSubject::for_path(
                    HumanReviewKind::Asset,
                    entry.content_path().clone(),
                    file.sha256(),
                    asset_policy.clone(),
                ))
            })
        }))
        .collect::<BTreeSet<_>>();
    let valid_attempts = documents
        .document_outcomes()
        .iter()
        .map(|run| HumanReviewAttempt::Document(run.id()))
        .chain(assets.into_iter().flat_map(|items| {
            items
                .entries()
                .iter()
                .map(|entry| HumanReviewAttempt::Asset(entry.review_run_id()))
        }))
        .collect::<BTreeSet<_>>();

    let mut seen: BTreeMap<HumanReviewBinding, u64> = BTreeMap::new();
    for record in selection.records() {
        let binding = record.binding().clone();
        let relevant = match &binding {
            HumanReviewBinding::Subject { subject, .. } => valid_subjects.contains(subject),
            HumanReviewBinding::AttemptOnly(attempt) => valid_attempts.contains(attempt),
        };
        if !relevant {
            return Err(PublicationApplicationError::IrrelevantHumanReviewSelection(
                Box::new(binding),
            ));
        }
        if let Some(existing) = seen.get(&binding)
            && *existing != record.id().get()
        {
            return Err(PublicationApplicationError::DuplicateHumanReviewSelection(
                Box::new(binding),
            ));
        }
        seen.insert(binding.clone(), record.id().get());
        match store
            .get(record.id())
            .map_err(|e| stage("human review lookup", e))?
        {
            Some(saved) if saved == *record => {}
            _ => {
                return Err(PublicationApplicationError::HumanReviewNotPersisted(
                    Box::new(binding),
                ));
            }
        }
    }
    Ok(())
}
