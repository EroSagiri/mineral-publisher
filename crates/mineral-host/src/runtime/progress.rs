//! Progress reporting is a host capability, like the clock.
//!
//! A use case knows *what* it is doing and in what order; where that becomes
//! visible is the caller's decision. The CLI streams it to stderr, a web host
//! would forward it as server-sent events, and a test passes [`NoProgress`] so
//! that asserting on an outcome never depends on a terminal.
//!
//! This is also what keeps the application layer free of printing: a module that
//! wanted to report progress would have to take a sink, and taking a sink is
//! what makes it testable.

/// Where a long-running use case reports what it is doing.
pub trait Progress: Send + Sync {
    /// Announces a phase boundary, for example entering the review stage.
    fn stage(&self, message: &str);

    /// Announces one item of work inside the current phase.
    fn detail(&self, message: &str);
}

/// A sink that reports nothing.
///
/// Tests and non-interactive hosts use it, so a use case can run to completion
/// without a terminal to write to.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn stage(&self, _message: &str) {}

    fn detail(&self, _message: &str) {}
}

/// A sink that writes every line to the process's standard error.
///
/// Standard error rather than standard output: progress is not the result, and a
/// caller that pipes the report must not have to filter it out.
#[derive(Clone, Copy, Debug, Default)]
pub struct StderrProgress;

impl Progress for StderrProgress {
    fn stage(&self, message: &str) {
        eprintln!("{message}");
    }

    fn detail(&self, message: &str) {
        eprintln!("{message}");
    }
}
