//! Time as heli reports it: whole seconds since 1970-01-01 UTC.

use std::time::{SystemTime, UNIX_EPOCH};

/// `time` in seconds since 1970; times before 1970 count as 0.
pub fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Now, in seconds since 1970.
pub fn now() -> u64 {
    unix_seconds(SystemTime::now())
}
