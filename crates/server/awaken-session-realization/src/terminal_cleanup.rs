//! Terminal cleanup of the realization executor: the authorization every
//! terminal cleanup effect runs under. It admits a cleanup lease against the
//! Session's realization authority, derives the effect fence and the typed
//! Sandbox disposal authorization, and reads the retained terminal owner and
//! Memory reconciliation plan the cleanup executes.

/// Executes terminal effects over its role-owned state in the canonical slots.
/// Run and cleanup effects share the one selected Artifact publication operation.
#[derive(Clone)]
pub struct TerminalCleanup {
    pub artifacts: std::sync::Arc<crate::artifact_harvest::ArtifactHarvester>,
    pub sessions: crate::session_realizations::SessionRealizations,
}
