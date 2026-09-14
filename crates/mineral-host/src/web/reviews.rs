//! The review queue: list it, inspect one attempt, decide it.
//!
//! A decision mutates the workspace, so it goes through the supervisor like any
//! other mutation — a Web approval and a CLI approval are then the same
//! operation, subject to the same gate. A handler that called the use case
//! directly would quietly give the browser a way past the single-flight rule.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

use crate::{
    application,
    operations::{OperationErrorCode, OperationRequest, ReviewOperation},
};

use super::{
    WebState,
    dto::{WebAcceptedOperation, WebReviewDetailResponse, WebReviewListResponse},
    error::WebApiError,
    off_runtime,
};

/// `GET /api/v1/reviews`
pub async fn list(
    State(state): State<Arc<WebState>>,
) -> Result<Json<WebReviewListResponse>, WebApiError> {
    let runtime = Arc::clone(state.runtime());
    let response = off_runtime(move || {
        application::review::review(&runtime, &application::review::ReviewRequest::List)
            .map(|outcome| WebReviewListResponse::from_outcome(&outcome))
            .map_err(|error| {
                WebApiError::from_application(OperationErrorCode::InvalidRequest, &error)
            })
    })
    .await?;
    Ok(Json(response))
}

/// `GET /api/v1/reviews/:attempt`
pub async fn show(
    State(state): State<Arc<WebState>>,
    Path(attempt): Path<String>,
) -> Result<Json<WebReviewDetailResponse>, WebApiError> {
    let runtime = Arc::clone(state.runtime());
    let response = off_runtime(move || {
        application::review::review(&runtime, &application::review::ReviewRequest::Show(attempt))
            .map_err(|error| {
                WebApiError::from_application(OperationErrorCode::InvalidRequest, &error)
            })
            .and_then(|outcome| match outcome {
                application::review::ReviewOutcome::Shown(detail) => {
                    Ok(WebReviewDetailResponse::from(detail.as_ref()))
                }
                _ => Err(WebApiError::internal(
                    "a show request did not produce a review",
                )),
            })
    })
    .await?;
    Ok(Json(response))
}

/// `POST /api/v1/reviews/:attempt/approve`
pub async fn approve(
    State(state): State<Arc<WebState>>,
    Path(attempt): Path<String>,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    start(Arc::clone(&state), ReviewOperation::Approve(attempt))
}

/// `POST /api/v1/reviews/:attempt/reject`
pub async fn reject(
    State(state): State<Arc<WebState>>,
    Path(attempt): Path<String>,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    start(Arc::clone(&state), ReviewOperation::Reject(attempt))
}

fn start(
    state: Arc<WebState>,
    decision: ReviewOperation,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    let supervisor = Arc::clone(state.supervisor());
    let id = supervisor
        .start(OperationRequest::ReviewDecision(decision))
        .map_err(|error| {
            let active = supervisor
                .active_mutation()
                .and_then(|id| supervisor.snapshot(id))
                .map(|snapshot| snapshot.kind.to_string());
            WebApiError::from_start(error, active.as_deref())
        })?;
    let snapshot = supervisor
        .snapshot(id)
        .ok_or_else(|| WebApiError::internal("the accepted operation vanished"))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(WebAcceptedOperation::new(
            id,
            &snapshot.kind,
            snapshot.state,
        )),
    ))
}
