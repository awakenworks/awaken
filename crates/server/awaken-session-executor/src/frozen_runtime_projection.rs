//! Frozen Session values every runtime projection reads: the content delivery
//! mode and the process-local projections of the Session baseline and
//! Environment snapshot. The Session slot holds them; provisioning, resource
//! layout and prompt rendering read them without reaching into the slot.

/// One Session-wide delivery decision for resources that can be represented
/// either as files or as semantic tools. The frozen Resource/Skill identities
/// remain authoritative; this value selects only their runtime projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ManagedContentDelivery {
    /// Anthropic Managed Agents compatible filesystem projection. Memory stores
    /// are mounted and Skills are discovered from their `SKILL.md` paths.
    #[default]
    ManagedFilesystem,
    /// Filesystem-free projection for Native agents whose published tool policy
    /// cannot invoke a filesystem tool. The same frozen bindings are exposed by
    /// bounded semantic tools instead.
    SemanticTools,
}

/// Process-local reference to the exact Control-frozen baseline. Keeping the
/// domain value intact avoids a second partial publication/layout projection:
/// context, provider selection, cold replay, and cleanup all consume the same
/// immutable coordinates.
pub(crate) type FrozenBaselineRuntimeProjection =
    awaken_session_configuration_contract::SessionBaseline;

/// The sole process-local projection of the frozen Environment snapshot.
/// Native and ACP provisioning both consume this value; no late network or
/// Sandbox override is stored beside it.
#[derive(Clone)]
pub(crate) struct FrozenEnvironmentRuntimeProjection {
    pub fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint,
    pub network: awaken_provisioning_contract::NetworkPolicy,
    pub packages: awaken_provisioning_contract::PackageRequirements,
    pub sandbox: Option<awaken_provisioning_contract::SandboxOverride>,
    pub provisioning: awaken_session_configuration_contract::SandboxProvisioning,
    pub idle_retention: awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
}
