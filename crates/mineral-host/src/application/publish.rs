//! One publication: read the source, judge it, deliver it, record the run.
//!
//! This is the use case the whole engine exists for. It takes a request, asks
//! the runtime for the capabilities it needs, and returns a trace of what
//! happened. Whether the result is a publication, a queue of work for a human,
//! or a remote conflict is a value in the outcome, not a printed line.

use std::{sync::Arc, time::SystemTime};

use crate::{
    policy::PolicyIdentity,
    publisher::GitPublicationExecution,
    reviewer::{ASSET_REVIEWER_PROMPT_VERSION, MARKDOWN_REVIEWER_PROMPT_VERSION},
    runtime::{HostAssetReviews, HostMarkdownReviews, Progress, WorkspaceRuntime, composition},
    workflow::{
        ExplicitHumanReviewSelection, PublicExclusionRules, PublicationApplication,
        PublicationApplicationOutcome, PublicationApplicationRequest,
    },
};

use super::ApplicationError;

/// What one publication is asked for.
#[derive(Clone, Debug)]
pub struct PublishRequest {
    /// The instant the run is recorded as beginning.
    ///
    /// Time is an input, not something a use case reads: a caller that replays
    /// or tests a publication supplies its own.
    pub created_at: SystemTime,
    /// Human decisions the caller has already made, if any.
    pub human_reviews: ExplicitHumanReviewSelection,
}

impl PublishRequest {
    /// The request a caller makes right now.
    pub fn now() -> Self {
        Self {
            created_at: SystemTime::now(),
            human_reviews: ExplicitHumanReviewSelection::default(),
        }
    }
}

/// What one publication produced, plus what a renderer needs to describe it.
#[derive(Debug)]
pub struct PublishOutcome {
    /// The engine's own outcome: either work is waiting for a human, or a
    /// publication was attempted with the trace that describes it.
    pub outcome: PublicationApplicationOutcome,
    /// Where delivered assets were written, as the target describes itself.
    pub asset_location: String,
    /// The validated public scope this run applied.
    pub public_scope: PublicExclusionRules,
}

impl PublishOutcome {
    /// The overall result, as a stable code a client matches on.
    ///
    /// The fact lives here rather than in a renderer because both the terminal
    /// and the wire protocol need the same answer, and deriving it twice is how
    /// two adapters start disagreeing.
    pub fn status_code(&self) -> &'static str {
        match &self.outcome {
            PublicationApplicationOutcome::NeedsHumanReview { .. } => "waiting_for_human_review",
            PublicationApplicationOutcome::Completed { completed, .. } => {
                // A Git target holding the right Markdown is not a finished
                // delivery: its documents point at objects, and those have to be
                // verified too.
                if !completed.publication().workflow().is_satisfied() {
                    return "incomplete";
                }
                self.git_code().unwrap_or("not_published")
            }
        }
    }

    /// What happened to the publication ref, when a publication was attempted.
    pub fn git_code(&self) -> Option<&'static str> {
        let PublicationApplicationOutcome::Completed { completed, .. } = &self.outcome else {
            return None;
        };
        Some(match completed.publication().workflow().git() {
            GitPublicationExecution::NoopSatisfied { .. } => "noop",
            GitPublicationExecution::Published { .. }
            | GitPublicationExecution::AlreadyPublished { .. } => "published",
            GitPublicationExecution::RemoteChanged { .. } => "conflict",
            GitPublicationExecution::Indeterminate { .. } => "indeterminate",
            GitPublicationExecution::TargetMissing { .. } => "target_missing",
            GitPublicationExecution::PushFailedButRemoteUnchanged { .. }
            | GitPublicationExecution::RemoteUnchangedAfterSuccessfulPush { .. } => "not_published",
        })
    }

    /// Whether the publication changed anything.
    pub fn is_noop(&self) -> bool {
        self.status_code() == "noop"
    }
}

/// Runs one publication.
pub fn publish(
    runtime: &WorkspaceRuntime,
    request: PublishRequest,
    progress: &Arc<dyn Progress>,
) -> Result<PublishOutcome, ApplicationError> {
    runtime.prepare()?;
    let content_store = runtime.content_store();

    progress.stage("[1/4] Creating immutable Snapshot...");
    let snapshot = runtime.snapshot(progress.as_ref())?;
    progress.stage(&format!(
        "[1/4] Snapshot {} contains {} files.",
        snapshot.id().get(),
        snapshot.files().len()
    ));

    let document_runs = runtime.document_reviews()?;
    let asset_runs = runtime.asset_reviews()?;
    let human = runtime.human_reviews()?;
    let publish_runs = runtime.publish_runs()?;
    let delivery_projections = runtime.delivery_projections()?;
    let asset_target = runtime.asset_target()?;
    let public_scope = runtime.public_scope()?;
    let asset_location = asset_target.description();
    let asset_observations = runtime.asset_observations()?;
    let observations = runtime.remote_observations()?;
    let markdown_reviewer = runtime.markdown_reviewer(Arc::clone(progress))?;
    let asset_reviewer = runtime.asset_reviewer(Arc::clone(progress))?;
    let markdown_policy = PolicyIdentity::new(
        format!("deepseek:{}", runtime.config.review.markdown_model),
        MARKDOWN_REVIEWER_PROMPT_VERSION,
        runtime.markdown_contract_hash()?,
    )
    .map_err(|error| ApplicationError::operation("build the Markdown policy identity", error))?;
    let asset_policy = PolicyIdentity::new(
        format!("deepseek:{}", runtime.config.review.asset_model),
        ASSET_REVIEWER_PROMPT_VERSION,
        runtime.asset_contract_hash()?,
    )
    .map_err(|error| ApplicationError::operation("build the asset policy identity", error))?;
    let target = runtime.publish_target()?;
    let commit_metadata = runtime.publish_commit_metadata()?;
    let asset_delivery = runtime.asset_delivery()?;
    let publication_request = PublicationApplicationRequest {
        snapshot: &snapshot,
        markdown_policy: &markdown_policy,
        asset_policy: &asset_policy,
        repository: &runtime.config.git.repository,
        target_id: runtime.publish_target_id()?,
        target,
        commit_metadata: &commit_metadata,
        human_reviews: request.human_reviews,
        public_scope: &public_scope,
        asset_delivery: &asset_delivery,
    };

    // The review execution strategy is a runtime capability, like the clock:
    // the engine is handed one rather than choosing how to run work.
    let markdown_evaluator =
        HostMarkdownReviews::for_concurrency(runtime.config.review.markdown_concurrency);
    let asset_evaluator =
        HostAssetReviews::for_concurrency(runtime.config.review.asset_concurrency);
    let mut document_ids = composition::RandomDocumentIds;
    let mut asset_ids = composition::RandomAssetIds;
    let mut publish_ids = crate::publisher::UuidPublishRunIdGenerator;
    let mut observation_ids = crate::publisher::UuidRemoteObservationIdGenerator;
    let mut asset_observation_ids = crate::asset::UuidAssetObservationIdGenerator;

    progress.stage("[2/4] Running privacy, program checks, and semantic review...");
    let outcome = PublicationApplication::run(
        publication_request,
        progress.as_ref(),
        &content_store,
        &markdown_reviewer,
        &asset_reviewer,
        &document_runs,
        &asset_runs,
        &human,
        &publish_runs,
        &delivery_projections,
        &asset_target,
        &asset_observations,
        &mut asset_observation_ids,
        &observations,
        &mut document_ids,
        &mut asset_ids,
        &mut publish_ids,
        &mut observation_ids,
        request.created_at,
        &markdown_evaluator,
        &asset_evaluator,
    )
    .map_err(|error| ApplicationError::operation("publication", error))?;
    progress.stage("[4/4] Publication workflow finished.");

    Ok(PublishOutcome {
        outcome,
        asset_location,
        public_scope,
    })
}
