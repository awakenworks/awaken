//! Resource staging of the realization executor: the frozen Session Resources
//! as the Environment receives them. It projects the provider-effective sandbox
//! spec, registers the staged Resources and the active manifest, and validates
//! the managed mount layout against a provider.

use awaken_session_configuration_contract::SkillBundleSource;
use awaken_session_realization_contract::ResourceOperationFence;
use std::sync::Arc;

mod skills;

/// Stages one installed Session's Resources into its Environment spec; its only
/// state is the installed Sessions' realization layer it reads and records into.
/// Its exact Skill byte source is selected before construction, without catalog writes.
#[derive(Clone)]
pub(crate) struct ResourceStaging {
    pub(crate) sessions: crate::session_realizations::SessionRealizations,
    /// Exact immutable bytes only; this grants no catalog/authoring access.
    pub(crate) bundle_source: Option<Arc<dyn SkillBundleSource<ResourceOperationFence>>>,
}
