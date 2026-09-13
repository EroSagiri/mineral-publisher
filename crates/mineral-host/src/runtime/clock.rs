use std::time::SystemTime;

use crate::{domain::TimestampMillis, ports::Clock};

/// The native wall clock.
///
/// The engine never reads a clock itself; this adapter freezes one reading per
/// call and reports `None` when the instant cannot be represented, so an unusable
/// reading fails closed instead of being silently clamped.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Option<TimestampMillis> {
        TimestampMillis::from_system_time(SystemTime::now())
    }
}
