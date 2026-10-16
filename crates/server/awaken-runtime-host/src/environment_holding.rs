// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Environment component owns the original per-Session lifecycle mutex and physical owner projection; no second registry or durable authority is introduced.
//! Environment holding of the realization executor: the Environment each
//! installed Session holds, through the sandbox providers it realizes with.
//! It selects the provider, creates, adopts, restores and retires the exact
//! Environment owner under the Session's lifecycle lock, and keeps the warm
//! capacity those providers prepare.

/// Holds each installed Session's Environment. It owns the sandbox providers;
/// staging and the execution authority are the ports its effects go through.
pub struct EnvironmentHolding {
    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    pub(crate) env: crate::environment_providers::EnvironmentProviders,
    /// Warm capacity is kept only where a local dispatch pool realizes claims.
    pub(crate) runs_local_dispatch_pool: bool,
    /// The installed Sessions, as far as realization may reach them.
    pub(crate) sessions: crate::session_realizations::SessionRealizations,
    /// Resource staging: spec projection, registration and layout validation.
    pub(crate) staging: crate::resource_staging::ResourceStaging,
    /// The authority physical Environment effects run under.
    pub(crate) execution: crate::session_realizations::ExecutionAuthority,
}

impl EnvironmentHolding {
    /// Hold Environments over the installed Sessions with the sandbox providers
    /// they realize through and the Worker's execution proof, when registered.
    pub(crate) fn new(
        env: crate::environment_providers::EnvironmentProviders,
        runs_local_dispatch_pool: bool,
        staging: crate::resource_staging::ResourceStaging,
        worker: Option<std::sync::Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>>,
    ) -> Self {
        let sessions = staging.sessions.clone();
        Self {
            env,
            runs_local_dispatch_pool,
            staging,
            execution: crate::session_realizations::ExecutionAuthority {
                sessions: sessions.clone(),
                worker,
            },
            sessions,
        }
    }
}

/// Per-Session state held under the canonical installed-slot mutex.
#[derive(Default)]
pub(crate) struct EnvironmentHoldingState {
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Sole live Environment owner. Every state transition is driven through
    /// `host/session/environment_lifecycle.rs`; other modules may only inspect
    /// Resident/occupancy projections through its read helpers.
    pub(crate) owner: crate::environment_owner::SessionEnvironmentOwner,
    /// Exact unavailable aggregate binding/generation authorized for one
    /// RebuildFromCommittedTruth replacement. This is a process-local command
    /// projection; the Session aggregate receipt remains the only transition
    /// authority and clears it after durable publication.
    pub rebuild_source: Option<(String, Option<String>)>,
}
