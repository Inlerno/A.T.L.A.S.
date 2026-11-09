//! Wall-clock time shared by the event bus (#8) and the module manager (#9).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in Unix milliseconds; 0 if the clock is set before
/// 1970. Informational only: nothing orders events or decisions by it.
pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
