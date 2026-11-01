/// Wire compatibility sentinel for historical roots without a recorded time.
/// New facts never use this value; optional Event timestamps remain absent.
pub(crate) const LEGACY_UNRECORDED_AT: &str = "2026-01-01T00:00:00Z";

/// A read could not assemble one version-consistent committed prefix within
/// its bounded seqlock attempts. Clients may retry the unchanged GET, but must
/// not replay the command that created the Session events.
pub const COMMITTED_PREFIX_READ_CONFLICT: &str =
    "Managed projection sources changed while reading one committed prefix";

pub(crate) fn persisted_time(epoch_ms: Option<u64>) -> String {
    epoch_ms
        .map(awaken_time::utc::epoch_millis_to_rfc3339)
        .unwrap_or_else(|| LEGACY_UNRECORDED_AT.to_string())
}
