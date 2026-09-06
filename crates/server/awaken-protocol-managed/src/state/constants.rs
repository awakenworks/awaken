/// The seeded owner scope a bare/self-hosted session is created under when the
/// edge resolved no workspace (ADR-0051 / ADR-0048 D2 "seeded, not absent"). It
/// matches the request scope the ownership guard derives for an unscoped request,
/// so a single-tenant deployment never 404s itself.
pub(crate) const DEFAULT_SCOPE: &str = "default";

/// Wire compatibility sentinel for historical roots without a recorded time.
/// New facts never use this value; optional Event timestamps remain absent.
pub(crate) const LEGACY_UNRECORDED_AT: &str = "2026-01-01T00:00:00Z";

pub(crate) fn persisted_time(epoch_ms: Option<u64>) -> String {
    epoch_ms
        .map(awaken_session_contract::epoch_millis_to_rfc3339)
        .unwrap_or_else(|| LEGACY_UNRECORDED_AT.to_string())
}

/// The Managed Agents contract error for a `memory_store` add/remove on a running
/// session — memory stores bind at session creation only.
pub(crate) const MEMORY_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; adding or removing one from a \
     running session is not supported";
