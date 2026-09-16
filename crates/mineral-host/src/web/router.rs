//! The route table.
//!
//! Two trees, and the split between them is the whole point:
//!
//! ```text
//! /api/*        -> the JSON API       (unknown paths here stay JSON 404s)
//! anything else -> the built Web UI   (unknown paths get index.html)
//! ```
//!
//! A single-page application needs the second rule: a browser that reloads
//! `/operations/op-17` is asking for a client-side route, not for a file, and
//! answering `404` would break the reload. It must never swallow the first rule,
//! though — `/api/v1/nonsense` is a broken API call, and a client that receives
//! an HTML page instead of an error code cannot tell what went wrong. That
//! guarantee is asserted by a test.

use std::{path::Path, sync::Arc};

use axum::{
    Router,
    routing::{get, post},
};
use tower_http::services::{ServeDir, ServeFile};

use super::{WebState, error::WebApiError, operations, reviews, sse, status};

/// Builds the router for one workspace.
///
/// The UI is served when a build exists; when it does not, the API still works
/// and every page answers with an explanation rather than a blank error.
pub fn router(state: Arc<WebState>) -> Router {
    let api = api_router()
        .route(
            "/v1/schedules",
            get(super::service::schedules).post(super::service::save),
        )
        .route("/v1/history", get(super::service::history))
        .route("/v1/history/:id/steps", get(super::service::steps))
        .route("/v1/auth/logout", post(super::auth::logout))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            super::auth::guard,
        ))
        .route("/v1/auth/login", post(super::auth::login))
        .route("/v1/auth/session", get(super::auth::status));
    let app = match state.assets() {
        Some(directory) => Router::new()
            .nest("/api", api)
            // `fallback`, not `not_found_service`: the latter overrides the
            // fallback's status with 404, which is right for a missing file and
            // wrong for a client-side route that the shell answers with 200.
            .fallback_service(
                ServeDir::new(directory).fallback(ServeFile::new(directory.join("index.html"))),
            )
            .with_state(state),
        None => Router::new()
            .nest("/api", api)
            .fallback(missing_ui)
            .with_state(state),
    };
    app.layer(axum::middleware::from_fn(security_headers))
}

async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("referrer-policy", "same-origin".parse().unwrap());
    if api {
        headers.insert("cache-control", "no-store".parse().unwrap());
    }
    response
}

/// The JSON API alone, with no UI behind it.
fn api_router() -> Router<Arc<WebState>> {
    Router::new()
        .route("/v1/status", get(status::status))
        .route("/v1/doctor", get(status::doctor))
        .route("/v1/reviews", get(reviews::list))
        .route("/v1/reviews/:attempt", get(reviews::show))
        .route("/v1/reviews/:attempt/approve", post(reviews::approve))
        .route("/v1/reviews/:attempt/reject", post(reviews::reject))
        .route("/v1/operations", get(operations::list))
        .route("/v1/operations/publish", post(operations::start_publish))
        .route("/v1/operations/backup", post(operations::start_backup))
        .route(
            "/v1/operations/backup/init",
            post(operations::start_backup_init),
        )
        .route(
            "/v1/operations/backup/verify",
            post(operations::start_backup_verify),
        )
        .route("/v1/operations/:id", get(operations::snapshot))
        .route("/v1/operations/:id/events", get(sse::events))
        // Anything else under /api is a broken API call, and says so in JSON.
        .fallback(no_such_endpoint)
}

/// Anything under `/api` that is not a route is a JSON 404.
async fn no_such_endpoint() -> WebApiError {
    WebApiError::no_such_endpoint()
}

/// The UI was never built.
///
/// A fresh checkout has no `web-ui/dist`, and `cargo` deliberately does not
/// build it. Saying so is more useful than a stack of 404s.
async fn missing_ui() -> (axum::http::StatusCode, &'static str) {
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "the Web UI is not built.\n\n\
         Build it once with:\n    npm --prefix web-ui install\n    npm --prefix web-ui run build\n\n\
         Then restart `mineral web`. The JSON API under /api/v1 works without it.\n",
    )
}

/// Whether a directory looks like a built UI.
pub(crate) fn is_built_ui(directory: &Path) -> bool {
    directory.join("index.html").is_file()
}
