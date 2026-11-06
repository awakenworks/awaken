//! The provider-effective layout the Session environment owner validates.

use awaken_provisioning_contract as pc;

/// The exact layout a provider will reopen from a durable handle: the
/// effective spec with the handle's historical output, environment and
/// network coordinates, and the paths the handle already owns. Consumer: the
/// Host's environment lifecycle, which rejects a stale conflicting layout
/// before any provider I/O.
pub struct EnvironmentAdoptionLayout {
    pub spec: pc::SandboxSpec,
    pub historical_owned_paths: Option<Vec<String>>,
}
