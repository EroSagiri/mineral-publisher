//! Starting and reading operations.
//!
//! `POST` on an operation endpoint does exactly one thing: hand the request to
//! the supervisor and return its identity. It never waits, so a publication does
//! not hold an HTTP connection open for a minute. Everything a caller then wants
//! to know is either streamed from `/events` or read from `/operations/:id`.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

use crate::operations::{OperationId, OperationRequest};

use super::{
    WebState,
    dto::{WebAcceptedOperation, WebOperationResponse, WebOperationSummary},
    error::WebApiError,
};

/// `GET /api/v1/operations`
pub async fn list(State(state): State<Arc<WebState>>) -> Json<Vec<WebOperationSummary>> {
    let summaries = state
        .supervisor()
        .snapshots()
        .iter()
        .map(WebOperationSummary::from_snapshot)
        .collect();
    Json(summaries)
}

/// `POST /api/v1/operations/publish`
pub async fn start_publish(
    State(state): State<Arc<WebState>>,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    accept(
        state,
        OperationRequest::Publish(crate::application::publish::PublishRequest::now()),
    )
}

/// `POST /api/v1/operations/backup`
pub async fn start_backup(
    State(state): State<Arc<WebState>>,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    let request = crate::application::backup::BackupRequest::now()
        .map_err(|error| WebApiError::internal(error.to_string()))?;
    accept(state, OperationRequest::Backup(request))
}

/// `POST /api/v1/operations/backup/init`
pub async fn start_backup_init(
    State(state): State<Arc<WebState>>,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    accept(state, OperationRequest::BackupInit)
}

/// `POST /api/v1/operations/backup/verify`
pub async fn start_backup_verify(
    State(state): State<Arc<WebState>>,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    accept(state, OperationRequest::VerifyBackup)
}

/// `GET /api/v1/operations/:id`
pub async fn snapshot(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<WebOperationResponse>, WebApiError> {
    let id = parse_id(&id)?;
    let snapshot = state
        .supervisor()
        .snapshot(id)
        .ok_or_else(|| WebApiError::operation_not_found(id))?;
    Ok(Json(WebOperationResponse::from_snapshot(&snapshot)))
}

/// Accepts one operation and returns its identity, without waiting for it.
fn accept(
    state: Arc<WebState>,
    request: OperationRequest,
) -> Result<(StatusCode, Json<WebAcceptedOperation>), WebApiError> {
    let supervisor = Arc::clone(state.supervisor());
    let id = supervisor.start(request).map_err(|error| {
        // The refusal names the operation that holds the workspace, and what it
        // is doing, so a UI can say "a backup is running" rather than "busy".
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

/// Parses an operation identity from a path segment.
///
/// The wire form is `op-7`, which is what [`OperationId`] renders. Anything else
/// is a request the caller can fix, so it is a `400` rather than a `404`:
/// `operation_not_found` means "this identity is not known", not "that is not an
/// identity".
pub fn parse_id(value: &str) -> Result<OperationId, WebApiError> {
    let digits = value
        .strip_prefix("op-")
        .ok_or_else(|| WebApiError::invalid_path("operation id must look like op-<number>"))?;
    let number: u64 = digits
        .parse()
        .map_err(|_| WebApiError::invalid_path("operation id must look like op-<number>"))?;
    // Identities are allocated from 1 upward, so zero was never handed out.
    if number == 0 {
        return Err(WebApiError::invalid_path(
            "operation id must look like op-<number>",
        ));
    }
    // Rebuilding one from its number is safe because the supervisor only knows
    // the identities it allocated: an unknown number resolves to nothing.
    Ok(OperationId::from_number(number))
}
