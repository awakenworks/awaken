//! The Session realizer (ADR-0089): the component that executes the
//! Environment realization effects the Session aggregate projects, through
//! ports chosen once when the Host is built. It keeps no durable state.

use crate::environment_providers::EnvironmentProviders;

/// Executes Session Environment realization; warm capacity is in
/// `environment_capacity`, the other effects join as they leave `SharedHost`.
pub struct SessionRealizer {
    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    pub(crate) env: EnvironmentProviders,
    /// Warm capacity is kept only where a local dispatch pool realizes claims.
    pub(crate) runs_local_dispatch_pool: bool,
}
