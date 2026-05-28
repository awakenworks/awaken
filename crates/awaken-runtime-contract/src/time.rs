/// Returns the current time in milliseconds since the UNIX epoch.
///
/// Uses `unwrap_or_default()` so the call never panics (returns `0` if the
/// system clock is before the epoch) and clamps the `u128` millis value to
/// [`u64::MAX`].
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
