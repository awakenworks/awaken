/// The seeded owner scope a bare/self-hosted session is created under when the
/// edge resolved no workspace (ADR-0051 / ADR-0048 D2 "seeded, not absent"). It
/// matches the request scope the ownership guard derives for an unscoped request,
/// so a single-tenant deployment never 404s itself.
pub(crate) const DEFAULT_SCOPE: &str = "default";

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
pub(crate) const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// Current wall-clock time for newly-created public resource projections.
///
/// `PROCESSED_AT` is retained for deterministic event projection until that
/// projection receives its clock port. It must not be used as the creation
/// time shown to users.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn session_created_at(epoch_millis: u64) -> String {
    if epoch_millis == 0 {
        PROCESSED_AT.to_string()
    } else {
        awaken_session_contract::epoch_millis_to_rfc3339(epoch_millis)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_projection_time_is_not_the_deterministic_placeholder() {
        let timestamp = super::session_created_at(super::now_unix_ms());
        assert_ne!(timestamp, super::PROCESSED_AT);
        assert!(timestamp.ends_with('Z'));
    }

    #[test]
    fn historical_session_without_creation_time_keeps_legacy_placeholder() {
        assert_eq!(super::session_created_at(0), super::PROCESSED_AT);
    }
}

/// The Managed Agents contract error for a `memory_store` add/remove on a running
/// session — memory stores bind at session creation only.
pub(crate) const MEMORY_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; adding or removing one from a \
     running session is not supported";
