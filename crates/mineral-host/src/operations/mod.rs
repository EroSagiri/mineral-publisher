//! The Operation API: long-running work that can be started, watched and read.
//!
//! ```text
//! CLI ──┐
//!       ├──> operations/ ──> application/ ──> runtime/ ──> core + adapters
//! Web ──┘        (S8.1)          (S8.1–)        (S8.2)
//! ```
//!
//! This module is the layer between an entry point and a use case. It exists
//! because a terminal and a browser need the same thing for different reasons:
//! the terminal prints progress and the browser streams it, but both need a
//! publication to be *startable without being waited on*, watchable while it
//! runs, and readable afterwards as a typed result or a typed failure.
//!
//! Nothing here knows what an entry point looks like. There is no HTTP status,
//! no JSON, no SSE and no `println!`: [`OperationSupervisor`] hands out
//! [`OperationId`]s, [`Subscription`]s and [`OperationSnapshot`]s, and a CLI or
//! a Web adapter decides what those become.

pub mod executor;
pub mod model;
pub mod supervisor;
#[cfg(test)]
mod tests;

pub use executor::{ApplicationExecutor, OperationExecutor};
pub use model::{
    OperationErrorCode, OperationEvent, OperationFailure, OperationFailureError, OperationId,
    OperationKind, OperationRequest, OperationResult, OperationSnapshot, OperationState,
    ProgressEvent, ProgressKind, ReviewOperation, StartError,
};
pub use supervisor::{OperationSupervisor, Subscription};
