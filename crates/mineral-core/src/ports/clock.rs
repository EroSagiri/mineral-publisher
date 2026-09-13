use crate::domain::TimestampMillis;

/// The runtime's wall clock, read only when the engine must date a fact.
///
/// The engine never reads a clock itself: a runtime freezes one reading per call
/// and reports `None` when the instant cannot be represented, so an unusable
/// reading fails closed instead of being silently clamped. Dating a fact is the
/// only use — nothing in the engine decides behaviour from a clock.
pub trait Clock {
    fn now(&self) -> Option<TimestampMillis>;
}
