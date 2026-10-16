//! Terminal cleanup of the realization executor: the authorization every
//! terminal cleanup effect runs under. It admits a cleanup lease against the
//! Session's realization authority, derives the effect fence and the typed
//! Sandbox disposal authorization, and reads the retained terminal owner and
//! Memory reconciliation plan the cleanup executes.

/// Authorizes one installed Session's terminal cleanup effects; it reads the
/// realization layer and keeps no state of its own.
#[derive(Clone)]
pub(crate) struct TerminalCleanup {
    pub(crate) sessions: crate::session_realizations::SessionRealizations,
}

/// Per-Session state held under the canonical installed-slot mutex.
#[derive(Default)]
pub(crate) struct TerminalCleanupState {
    /// One complete Control-issued terminal assignment. Its Environment state
    /// and no-Create witness are observed together; authority remains in `authority`.
    pub assignment: Option<awaken_session_realization_contract::SessionTerminalCleanupAssignment>,
    /// Process-local parent of a terminal cleanup target. This is routing
    /// metadata only: the aggregate's frozen command set remains the sole work
    /// and receipt authority. It lets an accepted root receipt (or a later
    /// aggregate Completed read after response loss) retire child projections
    /// without maintaining a second cleanup registry.
    pub root: Option<String>,
}
