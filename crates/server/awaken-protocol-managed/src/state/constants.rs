/// The seeded owner scope a bare/self-hosted session is created under when the
/// edge resolved no workspace (ADR-0051 / ADR-0048 D2 "seeded, not absent"). It
/// matches the request scope the ownership guard derives for an unscoped request,
/// so a single-tenant deployment never 404s itself.
pub(crate) const DEFAULT_SCOPE: &str = "default";

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
pub(crate) const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The Managed Agents contract error for a `memory_store` add/remove on a running
/// session — memory stores bind at session creation only.
pub(crate) const MEMORY_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; adding or removing one from a \
     running session is not supported";
