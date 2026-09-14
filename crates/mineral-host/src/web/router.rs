//! The route table.
//!
//! Every route is under `/api/v1`, and every one either reads a structured value
//! or starts an operation. There is no route that renders text, because the API
//! is consumed by a program.

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use super::{WebState, error::WebApiError, operations, reviews, sse, status};

/// Builds the router for one workspace.
pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/api/v1/status", get(status::status))
        .route("/api/v1/doctor", get(status::doctor))
        .route("/api/v1/reviews", get(reviews::list))
        .route("/api/v1/reviews/:attempt", get(reviews::show))
        .route("/api/v1/reviews/:attempt/approve", post(reviews::approve))
        .route("/api/v1/reviews/:attempt/reject", post(reviews::reject))
        .route("/api/v1/operations", get(operations::list))
        .route(
            "/api/v1/operations/publish",
            post(operations::start_publish),
        )
        .route("/api/v1/operations/backup", post(operations::start_backup))
        .route(
            "/api/v1/operations/backup/init",
            post(operations::start_backup_init),
        )
        .route(
            "/api/v1/operations/backup/verify",
            post(operations::start_backup_verify),
        )
        .route("/api/v1/operations/:id", get(operations::snapshot))
        .route("/api/v1/operations/:id/events", get(sse::events))
        .fallback(no_such_endpoint)
        .with_state(state)
}

/// Anything else is a 404 with the standard error shape.
async fn no_such_endpoint() -> WebApiError {
    WebApiError::no_such_endpoint()
}
