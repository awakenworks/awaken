//! Managed Agent lifecycle identities and timestamps.

use awaken_time::{WallClock, WallClockExt};

/// A fresh Agent id. Only uniqueness is required of it, so it is drawn from a
/// random source rather than composed from a clock reading, the process id and
/// a counter.
pub(super) fn new_agent_id() -> String {
    format!("agent_{}", uuid::Uuid::new_v4().simple())
}

/// A reading of `clock` spelled as a Managed lifecycle timestamp.
pub(super) fn lifecycle_timestamp_at(clock: &dyn WallClock) -> String {
    awaken_time::utc::epoch_millis_to_rfc3339(clock.now_ms())
}
