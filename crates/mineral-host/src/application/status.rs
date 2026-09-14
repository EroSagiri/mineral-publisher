//! What this workspace currently holds.
//!
//! `status` is a read-only report: which source is configured, where state
//! lives, which ref publications go to, and how much work is waiting for a
//! human. It never writes and never touches the network.

use std::path::PathBuf;

use crate::{
    publisher::PublishRunStore, runtime::WorkspaceRuntime, workflow::HumanReviewResolution,
};

use super::ApplicationError;

/// Everything `status` reports, as data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusOutcome {
    /// The configured source identity.
    pub source_id: String,
    /// A human-readable description of where the source comes from.
    pub source: String,
    /// Where durable engine state lives.
    pub state_path: PathBuf,
    /// The publication remote's name.
    pub target_remote: String,
    /// The fully qualified publication ref.
    pub target_reference: String,
    /// The newest durable publication run, if this workspace has published.
    pub last_publication: Option<String>,
    /// How many Markdown attempts await a human decision.
    pub pending_documents: usize,
    /// How many asset attempts await a human decision.
    pub pending_assets: usize,
}

/// Reports the state of one workspace.
pub fn status(runtime: &WorkspaceRuntime) -> Result<StatusOutcome, ApplicationError> {
    let documents = runtime.document_reviews()?;
    let assets = runtime.asset_reviews()?;
    let human = runtime.human_reviews()?;
    let runs = runtime.publish_runs()?;
    // Touching the materialization store keeps `status` honest about which
    // database files a workspace is supposed to have.
    runtime.source_materializations()?;

    let pending_documents = HumanReviewResolution::list_pending_documents(&documents, &human)
        .map_err(|error| ApplicationError::operation("list pending Markdown reviews", error))?
        .len();
    let pending_assets = HumanReviewResolution::list_pending_assets(&assets, &human)
        .map_err(|error| ApplicationError::operation("list pending asset reviews", error))?
        .len();
    let all_runs = runs
        .list()
        .map_err(|error| ApplicationError::operation("list publication runs", error))?;

    Ok(StatusOutcome {
        source_id: runtime.config.source.id.clone(),
        source: runtime.source_description(),
        state_path: runtime.config.state.path.clone(),
        target_remote: runtime.config.git.remote.clone(),
        target_reference: runtime.config.git.reference.clone(),
        last_publication: all_runs.last().map(|run| run.id().get().to_string()),
        pending_documents,
        pending_assets,
    })
}
