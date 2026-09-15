//! The local HTTP API: the second entry point over the same application layer.
//!
//! ```text
//!            CLI            Web (this module)
//!             │                    │
//!             └────────┬───────────┘
//!                      ▼
//!                 operations/          start, watch, read
//!                      ▼
//!                 application/         one use case per module
//!                      ▼
//!              runtime/ ──> core + adapters
//! ```
//!
//! A terminal and a browser want the same thing for different reasons: the
//! terminal prints progress, the browser streams it. Both start a publication by
//! identity and read a typed outcome, so neither duplicates a decision the
//! application layer already makes.
//!
//! ## What this layer does not do
//!
//! * It does not serialise internal types. Every response is a `Web*` DTO
//!   written out in [`dto`], so an internal rename is not a breaking API change.
//! * It does not classify failures. Codes come from the operation vocabulary,
//!   which the CLI also uses.
//! * It does not carry secrets. There is no field for one on any response.
//! * It does not persist operations. An operation is in-process work; the
//!   durable facts are the runs the engine records.
//!
//! ## Security model
//!
//! This is a **local** administration interface. It can publish, back up and
//! approve content, so it binds [`DEFAULT_BIND`] — loopback only — and there is
//! no authentication, no TLS and no multi-user model. Nothing here is meant to
//! be reached from another machine; a caller that overrides the bind address is
//! deliberately taking on that decision, and is warned about it.

mod dto;
mod error;
mod operations;
mod reviews;
mod router;
mod sse;
mod status;
#[cfg(test)]
mod tests;

use std::{future::Future, net::SocketAddr, path::Path, path::PathBuf, sync::Arc};

use crate::{
    operations::{ApplicationExecutor, OperationSupervisor},
    runtime::WorkspaceRuntime,
};

pub use dto::{
    WebAcceptedOperation, WebBackupInitResult, WebBackupResult, WebBackupStatus, WebDecision,
    WebDoctorCheck, WebDoctorResponse, WebNoBaseCommit, WebOperationFailure, WebOperationResponse,
    WebOperationResult, WebOperationSummary, WebPolicy, WebProgressEvent, WebPublicationTarget,
    WebPublishResult, WebReviewCounts, WebReviewDetailResponse, WebReviewListResponse,
    WebReviewResolution, WebReviewSummary, WebSource, WebStatusResponse, WebVerifyResult,
};
pub use error::WebApiError;
pub use router::router;

/// Where a local admin API listens when nothing says otherwise.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

/// Where a built Web UI is looked for when nothing says otherwise.
///
/// `cargo` never builds it: a Rust build that shelled out to `npm` would make
/// every Rust CI run depend on Node. The UI is built once by hand, and the
/// server finds it here — or is told where it is.
pub const DEFAULT_ASSETS: &str = "web-ui/dist";

/// Everything a request handler needs.
///
/// One workspace, one supervisor. The supervisor's single-flight rule is per
/// workspace, which is exactly the scope of the resources it protects.
pub struct WebState {
    runtime: Arc<WorkspaceRuntime>,
    supervisor: Arc<OperationSupervisor>,
    assets: Option<PathBuf>,
}

impl WebState {
    /// Builds the state for one workspace, serving the default asset directory
    /// when it has been built.
    pub fn new(runtime: Arc<WorkspaceRuntime>) -> Self {
        Self::with_assets(runtime, Some(PathBuf::from(DEFAULT_ASSETS)))
    }

    /// Builds the state with an explicit asset directory.
    pub fn with_assets(runtime: Arc<WorkspaceRuntime>, assets: Option<PathBuf>) -> Self {
        let supervisor = Arc::new(OperationSupervisor::new(Arc::new(
            ApplicationExecutor::new(Arc::clone(&runtime)),
        )));
        Self {
            runtime,
            supervisor,
            assets: assets.filter(|directory| router::is_built_ui(directory)),
        }
    }

    /// Builds the state with an explicit supervisor, which is what a test uses
    /// to reach in and observe the gate.
    pub fn with_supervisor(
        runtime: Arc<WorkspaceRuntime>,
        supervisor: Arc<OperationSupervisor>,
    ) -> Self {
        Self {
            runtime,
            supervisor,
            assets: None,
        }
    }

    /// The built UI this server serves, if one was found.
    pub fn assets(&self) -> Option<&Path> {
        self.assets.as_deref()
    }

    /// The workspace every read is answered from.
    pub fn runtime(&self) -> &Arc<WorkspaceRuntime> {
        &self.runtime
    }

    /// The supervisor every mutation goes through.
    pub fn supervisor(&self) -> &Arc<OperationSupervisor> {
        &self.supervisor
    }
}

/// Serves the API until the future ends.
pub async fn serve(state: Arc<WebState>, address: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router(state)).await
}

/// Serves the API until `shutdown` resolves.
pub async fn serve_with_shutdown(
    state: Arc<WebState>,
    address: SocketAddr,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Serves the API on an already-bound listener.
///
/// A caller that wants the operating system to choose a port binds first and
/// hands the listener here; the tests use it to drive a real socket.
pub async fn serve_listener(
    state: Arc<WebState>,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

/// Whether an address stays on this machine.
pub fn is_loopback(address: &SocketAddr) -> bool {
    address.ip().is_loopback()
}

/// Runs one blocking use case off the async runtime.
///
/// Reads touch SQLite and the file system. Running them on a runtime worker
/// would stall every other request for the duration of a query, and a panic
/// inside one would take the connection with it; here it becomes a typed 500.
async fn off_runtime<T, F>(work: F) -> Result<T, WebApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, WebApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| WebApiError::internal("the request handler did not finish"))?
}
