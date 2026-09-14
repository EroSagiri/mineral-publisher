//! Native runtime concerns: how review work is executed, and host-side clocks
//! and identity allocation.
//!
//! Nothing here is part of the portable engine. The engine exposes per-item
//! operations plus an execution seam; this module supplies the strategies that
//! use host capabilities (currently bounded worker threads and the wall clock).

mod bounded;
mod clock;
pub mod composition;
mod evaluators;
#[cfg(test)]
mod tests;
pub mod workspace;

pub(crate) use bounded::bounded_map;
pub use clock::SystemClock;
pub use evaluators::{HostAssetReviews, HostMarkdownReviews};
pub use workspace::{RuntimeError, WorkspaceRuntime};
