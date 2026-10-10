//! The Session realizer (ADR-0089): the component that executes the
//! Environment realization effects the Session aggregate projects, through
//! ports chosen once when the Host is built. It keeps no durable state.

use crate::environment_providers::EnvironmentProviders;

/// Executes Session Environment realization; warm capacity is in
/// `environment_capacity`, the other effects join as they leave `SharedHost`.
pub struct RealizationExecutor {
    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    pub(crate) env: EnvironmentProviders,
    /// Warm capacity is kept only where a local dispatch pool realizes claims.
    pub(crate) runs_local_dispatch_pool: bool,
    /// The installed Sessions, as far as realization may reach them.
    pub(crate) sessions: crate::session_realizations::SessionRealizations,
    /// Resource staging: spec projection, registration and layout validation.
    pub(crate) staging: crate::resource_staging::ResourceStaging,
}

impl RealizationExecutor {
    /// Build the executor over the installed Sessions with the sandbox
    /// providers it realizes through.
    pub(crate) fn new(
        env: EnvironmentProviders,
        runs_local_dispatch_pool: bool,
        sessions: crate::session_realizations::SessionRealizations,
    ) -> Self {
        Self {
            env,
            runs_local_dispatch_pool,
            staging: crate::resource_staging::ResourceStaging {
                sessions: sessions.clone(),
            },
            sessions,
        }
    }
}
