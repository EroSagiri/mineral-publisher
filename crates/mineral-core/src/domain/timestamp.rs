use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A frozen point in time, in milliseconds since the Unix epoch.
///
/// Reading a wall clock is a runtime concern: the engine never calls
/// `SystemTime::now()`. A runtime freezes one clock reading into this value, and
/// every intent, observation, and commit-identity input derived from it stays
/// reproducible afterwards — the same reviewed tree can never acquire two
/// different identities because two attempts read two different clocks.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TimestampMillis(u64);

impl TimestampMillis {
    pub const UNIX_EPOCH: Self = Self(0);

    pub const fn from_unix_millis(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }

    /// Freezes a clock reading. Returns `None` for instants before the Unix
    /// epoch or beyond the representable millisecond range, so an unusable
    /// clock reading can never be silently clamped.
    pub fn from_system_time(value: SystemTime) -> Option<Self> {
        Self::from_duration(value.duration_since(UNIX_EPOCH).ok()?)
    }

    /// Freezes an elapsed duration since the Unix epoch.
    pub fn from_duration(duration: Duration) -> Option<Self> {
        duration.as_millis().try_into().ok().map(Self)
    }

    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0)
    }

    /// Whole seconds since the Unix epoch, truncating sub-second precision.
    pub const fn as_unix_seconds(self) -> u64 {
        self.0 / 1_000
    }
}

impl fmt::Display for TimestampMillis {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freezes_and_restores_a_clock_reading() {
        let value = TimestampMillis::from_system_time(UNIX_EPOCH + Duration::from_millis(1_500))
            .expect("representable instant");

        assert_eq!(value.as_unix_millis(), 1_500);
        assert_eq!(value.as_unix_seconds(), 1);
        assert_eq!(
            value.to_system_time(),
            UNIX_EPOCH + Duration::from_millis(1_500)
        );
        assert_eq!(TimestampMillis::UNIX_EPOCH.as_unix_millis(), 0);
    }

    #[test]
    fn rejects_instants_before_the_unix_epoch() {
        let before = UNIX_EPOCH - Duration::from_millis(1);

        assert_eq!(TimestampMillis::from_system_time(before), None);
    }

    #[test]
    fn ordering_follows_wall_clock_order() {
        let earlier = TimestampMillis::from_unix_millis(1);
        let later = TimestampMillis::from_unix_millis(2);

        assert!(earlier < later);
    }
}
