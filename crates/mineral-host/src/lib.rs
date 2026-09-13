//! Native host for Mineral Publisher: adapters plus the `mineral` CLI.
//!
//! During the core/host split this crate keeps its historical package name and
//! module paths: portable modules now live in `mineral-core` and are re-exported
//! here, so existing callers (`mineral_publisher::domain::…`, the CLI, examples
//! and the inline tests) keep working unchanged.

pub use mineral_core::{content, domain, policy, ports, publication, publish};

#[cfg(test)]
mod conformance_delivery_projection;
#[cfg(test)]
mod conformance_effective_review_set;
#[cfg(test)]
mod conformance_git_remote;
#[cfg(test)]
mod conformance_human_review;
#[cfg(test)]
mod conformance_markdown_analysis;
#[cfg(test)]
mod conformance_public_policy_run;
#[cfg(test)]
mod conformance_publish_reconciliation;

pub mod asset;
pub mod publisher;
pub mod reviewer;
pub mod runtime;
pub mod source;
pub mod storage;
pub mod workflow;
