//! Portable publication identities and the Git publication boundary.
//!
//! Only Git is modeled today, and deliberately so: commits, trees, refs, and
//! compare-and-swap updates are Git concepts. When another target family
//! arrives it gets its own sibling module (`publication/filesystem`,
//! `publication/r2`, ...) with its own application workflow, instead of this
//! module pretending that every publisher shares a universal "target".

pub mod git;
