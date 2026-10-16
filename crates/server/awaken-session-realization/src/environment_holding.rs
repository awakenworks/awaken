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
    /// The selected aggregate publication authority; the lock permits fixture installation only.
    pub environment_binding_sink: std::sync::RwLock<
        Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    >,
    /// The same dispatch instance used by this process, narrowed to execution binding.
    pub dispatch_execution:
        Option<std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>>,
    /// Frozen publication selection over the installed Session slots.
    pub publication: crate::session_publication::SessionPublicationSelection,

    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    pub env: crate::environment_providers::EnvironmentProviders,
    /// The installed Sessions, as far as realization may reach them.
    pub sessions: crate::session_realizations::SessionRealizations,
    /// Resource staging: spec projection, registration and layout validation.
    pub staging: crate::resource_staging::ResourceStaging,
    /// The authority physical Environment effects run under.
    pub execution: crate::session_realizations::ExecutionAuthority,
}

/// Per-Session state held under the canonical installed-slot mutex.
#[derive(Default)]
pub struct EnvironmentHoldingState {
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Sole live Environment owner. Every state transition is driven through
    /// `environment_lifecycle.rs`; other modules may only inspect
    /// Resident/occupancy projections through its read helpers.
    pub owner: crate::environment_owner::SessionEnvironmentOwner,
    /// Exact unavailable aggregate binding/generation authorized for one
    /// RebuildFromCommittedTruth replacement. This is a process-local command
    /// projection; the Session aggregate receipt remains the only transition
    /// authority and clears it after durable publication.
    pub rebuild_source: Option<(String, Option<String>)>,
}
