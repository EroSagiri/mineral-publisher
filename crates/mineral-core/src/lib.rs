//! Portable Mineral Publisher engine.
//!
//! This crate must stay free of platform capabilities: no filesystem, process,
//! thread, environment, clock or HTTP access, and no host adapter types. Native
//! and Cloudflare runtimes compose it with their own adapters.

#![forbid(unsafe_code)]

pub mod content;
pub mod domain;
pub mod policy;
pub mod ports;
pub mod publication;
pub mod publish;
pub mod source;
pub mod workflow;
