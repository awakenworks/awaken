//! Shared time-conversion helpers.

/// Wall-clock time in milliseconds since the UNIX epoch.
///
/// Saturates to 0 when the system clock is set before 1970 — callers in
/// wire-format paths must never panic on a misconfigured host.
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
