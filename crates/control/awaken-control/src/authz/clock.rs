//! Canonical wall-clock projection shared by local and remote IAM adapters.

use std::time::{SystemTime, UNIX_EPOCH};

/// Now as the contract's canonical fixed-width RFC 3339 UTC timestamp, spelled
/// by the wall-clock port's one codec.
pub(super) fn now_rfc3339() -> String {
    awaken_lease_contract::system_clock()
        .now()
        .to_rfc3339_secs()
}

pub(super) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
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
