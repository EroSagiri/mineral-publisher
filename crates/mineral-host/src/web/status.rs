//! The read-only endpoints: what the workspace holds, and whether it is healthy.
//!
//! Both answer a question, so neither is an operation and neither is gated. They
//! also both touch SQLite and the file system, so they run off the async runtime
//! rather than blocking a worker on a database read.

use std::sync::Arc;

use axum::{Json, extract::State};

use crate::{application, operations::OperationErrorCode};

use super::{
    WebState,
    dto::{WebDoctorResponse, WebStatusResponse},
    error::WebApiError,
    off_runtime,
};

/// `GET /api/v1/status`
pub async fn status(
    State(state): State<Arc<WebState>>,
) -> Result<Json<WebStatusResponse>, WebApiError> {
    let runtime = Arc::clone(state.runtime());
    let response = off_runtime(move || {
        let outcome = application::status::status(&runtime).map_err(|error| {
            WebApiError::from_application(OperationErrorCode::DiagnosisFailed, &error)
        })?;
        let kind = source_kind(&runtime);
        Ok(WebStatusResponse::from_status(
            &outcome,
            kind,
            runtime.backup_enabled(),
        ))
    })
    .await?;
    Ok(Json(response))
}

/// `GET /api/v1/doctor`
pub async fn doctor(
    State(state): State<Arc<WebState>>,
) -> Result<Json<WebDoctorResponse>, WebApiError> {
    let runtime = Arc::clone(state.runtime());
    let response = off_runtime(move || {
        application::doctor::doctor(&runtime)
            .map(|outcome| WebDoctorResponse::from(&outcome))
            .map_err(|error| {
                WebApiError::from_application(OperationErrorCode::DiagnosisFailed, &error)
            })
    })
    .await?;
    Ok(Json(response))
}

/// The source kind, as a stable wire name.
fn source_kind(runtime: &crate::runtime::WorkspaceRuntime) -> &'static str {
    match runtime.source_kind() {
        crate::config::SourceType::Local => "local",
        crate::config::SourceType::R2 => "r2",
    }
}
