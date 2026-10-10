//! The realization executor (ADR-0089): executes the Environment realization
//! effects the Session aggregate projects, through ports chosen once when the
//! Host is built. It keeps no durable state.

use std::sync::Arc;

use awaken_provisioning_contract::CurrentExecutionAuthority;

use crate::environment_providers::EnvironmentProviders;
use crate::resource_staging::ResourceStaging;
use crate::session_realizations::{ExecutionAuthority, SessionRealizations};

/// Executes Session Environment realization through its components; warm
/// capacity is in `environment_capacity`.
pub struct RealizationExecutor {
    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    pub(crate) env: EnvironmentProviders,
    /// Warm capacity is kept only where a local dispatch pool realizes claims.
    pub(crate) runs_local_dispatch_pool: bool,
    /// The installed Sessions, as far as realization may reach them.
    pub(crate) sessions: SessionRealizations,
    /// Resource staging: spec projection, registration and layout validation.
    pub(crate) staging: ResourceStaging,
    /// The authority physical Environment effects run under.
    pub(crate) execution: ExecutionAuthority,
}

impl RealizationExecutor {
    /// Build the executor over the installed Sessions, the sandbox providers it
    /// realizes through and the Worker's execution proof, when registered.
    pub(crate) fn new(
        env: EnvironmentProviders,
        runs_local_dispatch_pool: bool,
        sessions: SessionRealizations,
        worker: Option<Arc<dyn CurrentExecutionAuthority>>,
    ) -> Self {
        Self {
            env,
            runs_local_dispatch_pool,
            staging: ResourceStaging {
                sessions: sessions.clone(),
            },
            execution: ExecutionAuthority {
                sessions: sessions.clone(),
                worker,
            },
            sessions,
        }
    }
}
