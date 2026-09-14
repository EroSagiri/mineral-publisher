//! Failures, as the HTTP API reports them.
//!
//! One shape for every failure, and one vocabulary: `code` is stable and is what
//! a client switches on, `message` is for a human and may be reworded. The codes
//! come from [`OperationErrorCode`], which is also what a CLI failure and an
//! operation failure are classified into — so the terminal and the browser can
//! never disagree about what happened.
//!
//! Nothing here can carry a secret: the codes are a closed set, and the only
//! strings that reach a response are messages the layers below already wrote
//! without credential values.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{
    application::ApplicationError,
    operations::{OperationErrorCode, OperationFailure, OperationId, StartError},
};

/// A failure, as JSON plus an HTTP status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebApiError {
    /// The stable code. This is the interface.
    pub code: String,
    /// For a human.
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    /// Present when the request was refused because the workspace is held.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation_kind: Option<String>,
}

impl WebApiError {
    fn new(code: OperationErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_owned(),
            message: message.into(),
            causes: Vec::new(),
            active_operation_id: None,
            active_operation_kind: None,
        }
    }

    /// The identity does not name an operation this server knows.
    ///
    /// It means one of three things, and the client is told to treat all three
    /// the same way: the identity was never allocated, retention dropped it, or
    /// it belonged to a process that has since restarted. Guessing is wrong —
    /// read the durable state instead.
    pub fn operation_not_found(id: OperationId) -> Self {
        Self::new(
            OperationErrorCode::OperationNotFound,
            format!("{id} is not known to this server"),
        )
    }

    /// A route that does not exist.
    pub fn no_such_endpoint() -> Self {
        Self::new(
            OperationErrorCode::EndpointNotFound,
            "no such endpoint; see the API documentation for /api/v1",
        )
    }

    /// A path parameter that is not usable.
    pub fn invalid_path(message: impl Into<String>) -> Self {
        Self::new(OperationErrorCode::InvalidRequest, message)
    }

    /// A worker thread panicked or was cancelled.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(OperationErrorCode::OperationPanicked, message)
    }

    /// The workspace is already running a mutating operation.
    pub fn workspace_busy(id: OperationId, kind: Option<&str>) -> Self {
        let mut error = Self::new(
            OperationErrorCode::WorkspaceBusy,
            format!("workspace is busy with {id}"),
        );
        error.active_operation_id = Some(id.to_string());
        error.active_operation_kind = kind.map(str::to_owned);
        error
    }

    /// A failure from a use case the API called directly.
    pub fn from_application(default: OperationErrorCode, error: &ApplicationError) -> Self {
        let code = OperationErrorCode::classify(default, error);
        let mut reported = Self::new(code, error.to_string());
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            reported.causes.push(cause.to_string());
            source = cause.source();
        }
        reported
    }

    /// A failure an operation already recorded.
    pub fn from_operation(error: &OperationFailure) -> Self {
        let mut reported = Self::new(error.code(), error.message());
        reported.causes = error.causes().to_vec();
        reported
    }

    /// Why an operation could not be started.
    pub fn from_start(error: StartError, kind: Option<&str>) -> Self {
        match error {
            StartError::WorkspaceBusy {
                active_operation_id,
            } => Self::workspace_busy(active_operation_id, kind),
        }
    }

    /// The HTTP status this failure reports as.
    ///
    /// The mapping is by code, in one place, so a client can rely on
    /// `code` and `status` agreeing.
    pub fn status(&self) -> StatusCode {
        match self.code.as_str() {
            "invalid_request" => StatusCode::BAD_REQUEST,
            "review_not_found" | "operation_not_found" | "endpoint_not_found" => {
                StatusCode::NOT_FOUND
            }
            "workspace_busy"
            | "workspace_not_configured"
            | "review_conflict"
            | "backup_no_base_commit" => StatusCode::CONFLICT,
            "credential_missing" => StatusCode::SERVICE_UNAVAILABLE,
            "connection_failed" => StatusCode::BAD_GATEWAY,
            // Everything else is a failure inside this server or the engine it
            // drives: the request was fine and retrying it unchanged will not
            // help until something is fixed.
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for WebApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(self)).into_response()
    }
}
