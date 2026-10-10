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
