//! Host adapters for the private backup pipeline.
//!
//! Core owns the facts and the order (projection, pointer identity, LFS-first
//! ordering, the exact compare-and-swap last). These modules own the planet:
//! the local Git object database, the Git LFS Batch API over HTTP, and the
//! composition that turns one Snapshot into a durable backup intent.

pub mod application;
pub mod git_backup;
pub mod lfs_http;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod tests;
