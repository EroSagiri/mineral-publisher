//! The human review queue: list what is waiting, inspect one attempt, decide it.
//!
//! A decision is immutable and bound to the subject it decided about, so
//! approving an attempt answers every later attempt about the same content and
//! policy. This module reports those facts; it decides nothing on its own.

use crate::{
    policy::ReviewRunStore,
    runtime::WorkspaceRuntime,
    workflow::{
        AssetReviewRunStore, HumanReviewAttempt, HumanReviewDecision, HumanReviewResolution,
    },
};

use super::ApplicationError;

/// One attempt waiting for a human, named the way an operator names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReview {
    /// The attempt identity, for example `document:42`.
    pub subject: String,
    /// The content the attempt is about.
    pub content_path: String,
}

/// What a caller asks the review queue to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewRequest {
    /// Every attempt waiting for a human decision.
    List,
    /// One attempt, with the facts a decision needs.
    Show(String),
    /// Record an approval of one attempt.
    Approve(String),
    /// Record a rejection of one attempt.
    Reject(String),
}

/// What the automatic reviewer decided.
///
/// The `code` is what a client matches on and may never be reworded; the
/// `detail` is the domain's own rendering of the decision, which is what a
/// human reads and what a terminal prints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewDecisionSummary {
    /// A stable code: `program_issues`, `review_approved`, `review_rejected`
    /// or `needs_human_review`.
    pub code: &'static str,
    /// The decision as the domain renders it.
    pub detail: String,
}

/// The reviewer's own reasons for its decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewReasonSummary {
    /// Stable snake_case names a client matches on.
    pub codes: Vec<String>,
    /// The domain's own rendering, which is what a human reads.
    pub detail: String,
}

/// The facts about one review attempt.
///
/// It carries the *values* the domain recorded — the subject, its content
/// identity, the policy contract it was judged under, and the reviewer's own
/// words — so a renderer decides how to present them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewDetail {
    /// The attempt identity, for example `document:42`.
    pub subject: String,
    /// `Markdown` or `Asset`.
    pub kind: &'static str,
    /// The content the attempt is about.
    pub content_path: String,
    /// The content identity the attempt froze.
    pub content_sha256: String,
    /// The decision the automatic reviewer reached.
    pub decision: ReviewDecisionSummary,
    /// The policy contract's name.
    pub policy_name: String,
    /// The policy contract's version.
    pub policy_version: String,
    /// The policy contract's hash.
    pub policy_hash: String,
    /// The reviewer's reason codes, when it reported any.
    pub reason_codes: Option<ReviewReasonSummary>,
    /// The reviewer's summary, when it reported one.
    pub summary: Option<String>,
    /// Whether a human decision already answers this attempt.
    pub human_resolution: bool,
}

/// What one review request produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewOutcome {
    /// The pending queues.
    List {
        documents: Vec<PendingReview>,
        assets: Vec<PendingReview>,
    },
    /// One attempt, with its facts.
    Shown(Box<ReviewDetail>),
    /// A decision was recorded, or was already recorded identically.
    Resolved {
        subject: String,
        decision: HumanReviewDecision,
        /// True when the same decision was already durable, so nothing changed.
        already: bool,
    },
}

/// Runs one review request.
pub fn review(
    runtime: &WorkspaceRuntime,
    request: &ReviewRequest,
) -> Result<ReviewOutcome, ApplicationError> {
    let documents = runtime.document_reviews()?;
    let assets = runtime.asset_reviews()?;
    let human = runtime.human_reviews()?;
    match request {
        ReviewRequest::List => Ok(ReviewOutcome::List {
            documents: pending_documents(&documents, &human)?,
            assets: pending_assets(&assets, &human)?,
        }),
        ReviewRequest::Show(subject) => Ok(ReviewOutcome::Shown(Box::new(detail(
            subject, &documents, &assets, &human,
        )?))),
        ReviewRequest::Approve(subject) => resolve(
            subject,
            HumanReviewDecision::Approve,
            &documents,
            &assets,
            &human,
        ),
        ReviewRequest::Reject(subject) => resolve(
            subject,
            HumanReviewDecision::Reject,
            &documents,
            &assets,
            &human,
        ),
    }
}

/// Every Markdown attempt waiting for a human decision.
pub fn pending_documents(
    documents: &crate::storage::SqliteReviewRunStore,
    human: &crate::storage::SqliteHumanReviewStore,
) -> Result<Vec<PendingReview>, ApplicationError> {
    Ok(
        HumanReviewResolution::list_pending_documents(documents, human)
            .map_err(|error| ApplicationError::operation("list pending Markdown reviews", error))?
            .into_iter()
            .map(|run| PendingReview {
                subject: format!("document:{}", run.id().get()),
                content_path: run.content_path().to_string(),
            })
            .collect(),
    )
}

/// Every asset attempt waiting for a human decision.
pub fn pending_assets(
    assets: &crate::storage::SqliteAssetReviewRunStore,
    human: &crate::storage::SqliteHumanReviewStore,
) -> Result<Vec<PendingReview>, ApplicationError> {
    Ok(HumanReviewResolution::list_pending_assets(assets, human)
        .map_err(|error| ApplicationError::operation("list pending asset reviews", error))?
        .into_iter()
        .map(|run| PendingReview {
            subject: format!("asset:{}", run.id().get()),
            content_path: run.content_path().to_string(),
        })
        .collect())
}

/// Parses the operator-facing review ID, which names one automatic attempt.
pub fn parse_attempt(value: &str) -> Result<HumanReviewAttempt, ApplicationError> {
    let (kind, id) = value
        .split_once(':')
        .ok_or_else(|| ApplicationError::Unsupported {
            message: "review ID must be document:ID or asset:ID".to_owned(),
        })?;
    let id: u64 = id.parse().map_err(|error| ApplicationError::Unsupported {
        message: format!("review ID must be document:ID or asset:ID: {error}"),
    })?;
    match kind {
        "document" => Ok(HumanReviewAttempt::Document(
            crate::policy::ReviewRunId::new(id).map_err(|error| ApplicationError::Unsupported {
                message: format!("review ID is unusable: {error}"),
            })?,
        )),
        "asset" => Ok(HumanReviewAttempt::Asset(
            crate::workflow::AssetReviewRunId::new(id).map_err(|error| {
                ApplicationError::Unsupported {
                    message: format!("review ID is unusable: {error}"),
                }
            })?,
        )),
        _ => Err(ApplicationError::Unsupported {
            message: "review ID must be document:ID or asset:ID".to_owned(),
        }),
    }
}

/// The automatic decision about one document, as a stable code plus the
/// domain's own rendering.
fn document_decision(decision: &crate::policy::PublicPolicyDecision) -> ReviewDecisionSummary {
    use crate::policy::PublicPolicyDecision;
    let code = match decision {
        PublicPolicyDecision::ProgramIssues(_) => "program_issues",
        PublicPolicyDecision::ReviewApproved => "review_approved",
        PublicPolicyDecision::ReviewRejected => "review_rejected",
        PublicPolicyDecision::NeedsHumanReview(_) => "needs_human_review",
    };
    ReviewDecisionSummary {
        code,
        detail: format!("{decision:?}"),
    }
}

/// The automatic decision about one asset, as a stable code plus the domain's
/// own rendering.
fn asset_decision(disposition: &crate::workflow::AssetReviewDisposition) -> ReviewDecisionSummary {
    use crate::workflow::AssetReviewDisposition;
    let code = match disposition {
        AssetReviewDisposition::Reviewed(_) => "reviewed",
        AssetReviewDisposition::NeedsHumanReview(_) => "needs_human_review",
        AssetReviewDisposition::Blocked => "blocked",
    };
    ReviewDecisionSummary {
        code,
        detail: format!("{disposition:?}"),
    }
}

/// The stable snake_case name of a document reason code.
///
/// The name comes from the domain enum's own serialised form, so it cannot
/// drift from the value the engine records; the enum type itself never leaves
/// this layer.
fn reason_code_name(code: &crate::policy::ReviewReasonCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The stable snake_case name of an asset reason code.
fn asset_reason_code_name(code: &crate::workflow::AssetReviewReasonCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The facts about one attempt.
fn detail(
    subject: &str,
    documents: &crate::storage::SqliteReviewRunStore,
    assets: &crate::storage::SqliteAssetReviewRunStore,
    human: &crate::storage::SqliteHumanReviewStore,
) -> Result<ReviewDetail, ApplicationError> {
    let attempt = parse_attempt(subject)?;
    let (kind, content_path, content_sha256, decision, policy, report) = match attempt {
        HumanReviewAttempt::Document(id) => {
            let run = documents
                .get(id)
                .map_err(|error| ApplicationError::operation("read Markdown review", error))?
                .ok_or_else(|| ApplicationError::NotFound {
                    subject: subject.to_owned(),
                })?;
            let report = run.reviewer_report();
            (
                "Markdown",
                run.content_path().to_string(),
                run.content_sha256().to_string(),
                document_decision(run.decision()),
                run.policy().clone(),
                report.map(|report| {
                    (
                        ReviewReasonSummary {
                            codes: report.reason_codes().iter().map(reason_code_name).collect(),
                            detail: format!("{:?}", report.reason_codes()),
                        },
                        report.summary().to_owned(),
                    )
                }),
            )
        }
        HumanReviewAttempt::Asset(id) => {
            let run = assets
                .get(id)
                .map_err(|error| ApplicationError::operation("read asset review", error))?
                .ok_or_else(|| ApplicationError::NotFound {
                    subject: subject.to_owned(),
                })?;
            let report = run.outcome().reviewer_report();
            (
                "Asset",
                run.content_path().to_string(),
                run.content_sha256().to_string(),
                asset_decision(run.outcome().disposition()),
                run.policy().clone(),
                report.map(|report| {
                    (
                        ReviewReasonSummary {
                            codes: report
                                .reason_codes()
                                .iter()
                                .map(asset_reason_code_name)
                                .collect(),
                            detail: format!("{:?}", report.reason_codes()),
                        },
                        report.summary().to_owned(),
                    )
                }),
            )
        }
    };
    // A decision is recognised by the subject it decided about, so the question
    // this attempt is still asking is answered by that lookup first, and by the
    // attempt itself for a record written before subjects were bound.
    let resolution = match attempt {
        HumanReviewAttempt::Document(id) => {
            let run = documents
                .get(id)
                .map_err(|error| ApplicationError::operation("read Markdown review", error))?
                .ok_or_else(|| ApplicationError::NotFound {
                    subject: subject.to_owned(),
                })?;
            HumanReviewResolution::document_resolution(&run, human)
                .map_err(|error| ApplicationError::operation("read the human resolution", error))?
        }
        HumanReviewAttempt::Asset(id) => {
            let run = assets
                .get(id)
                .map_err(|error| ApplicationError::operation("read asset review", error))?
                .ok_or_else(|| ApplicationError::NotFound {
                    subject: subject.to_owned(),
                })?;
            HumanReviewResolution::asset_resolution(&run, human)
                .map_err(|error| ApplicationError::operation("read the human resolution", error))?
        }
    };
    Ok(ReviewDetail {
        subject: subject.to_owned(),
        kind,
        content_path,
        content_sha256,
        decision,
        policy_name: policy.name().to_owned(),
        policy_version: policy.version().to_owned(),
        policy_hash: policy.hash().to_string(),
        reason_codes: report.as_ref().map(|(reasons, _)| reasons.clone()),
        summary: report.map(|(_, summary)| summary),
        human_resolution: resolution.is_some(),
    })
}

/// Records one immutable human decision, or reports that it already holds.
fn resolve(
    subject: &str,
    decision: HumanReviewDecision,
    documents: &crate::storage::SqliteReviewRunStore,
    assets: &crate::storage::SqliteAssetReviewRunStore,
    human: &crate::storage::SqliteHumanReviewStore,
) -> Result<ReviewOutcome, ApplicationError> {
    let attempt = parse_attempt(subject)?;
    let existing = match attempt {
        HumanReviewAttempt::Document(id) => {
            let run = documents
                .get(id)
                .map_err(|error| ApplicationError::operation("read Markdown review", error))?
                .ok_or_else(|| ApplicationError::NotFound {
                    subject: subject.to_owned(),
                })?;
            HumanReviewResolution::document_resolution(&run, human)
                .map_err(|error| ApplicationError::operation("read the human resolution", error))?
        }
        HumanReviewAttempt::Asset(id) => {
            let run = assets
                .get(id)
                .map_err(|error| ApplicationError::operation("read asset review", error))?
                .ok_or_else(|| ApplicationError::NotFound {
                    subject: subject.to_owned(),
                })?;
            HumanReviewResolution::asset_resolution(&run, human)
                .map_err(|error| ApplicationError::operation("read the human resolution", error))?
        }
    };
    if let Some(existing) = existing {
        if existing.decision() == decision {
            return Ok(ReviewOutcome::Resolved {
                subject: subject.to_owned(),
                decision,
                already: true,
            });
        }
        return Err(ApplicationError::Conflict {
            message: "review already has the opposite immutable human resolution".to_owned(),
        });
    }
    // A decision is frozen against the wall clock, which is a runtime concern;
    // it is read here because a stored record must carry a time and no caller
    // can supply a more truthful one.
    let id = crate::runtime::composition::random_human_id().map_err(|error| {
        ApplicationError::operation("allocate a human decision identity", error)
    })?;
    let now = std::time::SystemTime::now();
    match attempt {
        HumanReviewAttempt::Document(run) => {
            HumanReviewResolution::resolve_document(
                documents, human, id, run, decision, now, None, None,
            )
            .map_err(|error| ApplicationError::operation("record the human decision", error))?;
        }
        HumanReviewAttempt::Asset(run) => {
            HumanReviewResolution::resolve_asset(assets, human, id, run, decision, now, None, None)
                .map_err(|error| ApplicationError::operation("record the human decision", error))?;
        }
    }
    Ok(ReviewOutcome::Resolved {
        subject: subject.to_owned(),
        decision,
        already: false,
    })
}
