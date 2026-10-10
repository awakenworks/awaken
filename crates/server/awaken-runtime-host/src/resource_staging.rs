//! Resource staging of the realization executor: the frozen Session Resources
//! as the Environment receives them. It projects the provider-effective sandbox
//! spec, registers the staged Resources and the active manifest, and validates
//! the managed mount layout against a provider.

/// Stages one installed Session's Resources into its Environment spec; its only
/// state is the installed Sessions' realization layer it reads and records into.
#[derive(Clone)]
pub(crate) struct ResourceStaging {
    pub(crate) sessions: crate::session_realizations::SessionRealizations,
}
