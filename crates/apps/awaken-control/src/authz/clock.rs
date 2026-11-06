//! Canonical wall-clock projection shared by local and remote IAM adapters.
//!
//! Both adapters read the process's one injected wall clock; these helpers only
//! spell a reading in the two shapes the IAM contract consumes.

use awaken_time::{WallClock, WallClockExt};

/// A reading of `clock` as the contract's canonical fixed-width RFC 3339 UTC
/// timestamp, spelled by the wall-clock port's one codec.
pub(super) fn rfc3339_at(clock: &dyn WallClock) -> String {
    clock.now().to_rfc3339_secs()
}

/// A reading of `clock` as whole Unix seconds, the JWT `exp`/`nbf` unit.
pub(super) fn unix_seconds_at(clock: &dyn WallClock) -> i64 {
    i64::try_from(clock.now_ms() / 1_000).unwrap_or(i64::MAX)
}

/// Whether `value` has the contract's canonical RFC 3339 UTC shape
/// (`YYYY-MM-DDTHH:MM:SSZ`), so lexical comparison is chronological. The mint
/// engine additionally enforces `expires_at` strictly after `created_at`.
pub(super) fn canonical_timestamp_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes[19] == b'Z'
        && [4usize, 7].iter().all(|&i| bytes[i] == b'-')
        && bytes[10] == b'T'
        && [13usize, 16].iter().all(|&i| bytes[i] == b':')
        && (0..19).all(|i| [4, 7, 10, 13, 16].contains(&i) || bytes[i].is_ascii_digit())
}
