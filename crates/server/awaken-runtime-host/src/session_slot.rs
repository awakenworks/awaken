//! Process-local realization state for one Session.
//!
//! The frozen manifest remains the durable authority. This private slot owns every
//! projection materialized from it so registration and terminal cleanup share one
//! lifecycle boundary instead of coordinating parallel maps.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::memory::BoundMemory;
use crate::provisioning::StagedResources;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpProjectionState {
    Staged,
    Active,
    Draining,
    Removed,
}

/// Process-local effect for one exact durable MCP generation. This is live
/// material/connection state only; desired state remains in the Session
/// aggregate and is never reconstructed from this slot.
#[derive(Clone)]
pub(crate) struct McpGenerationProjection {
    pub generation: awaken_protocol_managed::McpGenerationRef,
    pub realization_id: String,
    pub stage_idempotency_key: String,
    /// Exact immutable realization identity with only renewable expiry/key
    /// excluded. It prevents a lease extension from changing target,
    /// credential, holder, owner incarnation, epoch, or logical generation.
    pub renewal_binding_fingerprint: String,
    pub receipt: awaken_protocol_managed::McpRealizationReceipt,
    pub server: Option<crate::mcp::McpTransportMaterial>,
    pub native_wiring: Option<crate::mcp::McpWiring>,
    pub state: McpProjectionState,
}

/// Process-local projection of the Control-frozen baseline. This is realization
/// input only; the durable Session aggregate remains the authority.
#[derive(Clone)]
pub(crate) struct FrozenBaselineRuntimeProjection {
    pub fingerprint: awaken_protocol_managed::SessionBaselineFingerprint,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
}

/// The sole process-local projection of the frozen Environment snapshot.
/// Native and ACP provisioning both consume this value; no late network or
/// Sandbox override is stored beside it.
#[derive(Clone)]
pub(crate) struct FrozenEnvironmentRuntimeProjection {
    pub fingerprint: awaken_protocol_managed::EnvironmentFingerprint,
    pub network: awaken_provisioning_contract::NetworkPolicy,
    pub packages: awaken_provisioning_contract::PackageRequirements,
    pub sandbox: Option<awaken_provisioning_contract::SandboxOverride>,
    pub provisioning: awaken_provisioning_contract::SandboxProvisioning,
    pub credential_realization: awaken_runtime_contract::CredentialRealizationProfile,
}

#[derive(Default)]
pub(crate) struct SessionRuntimeSlot {
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    pub runtime: Option<Arc<crate::host::SessionCtx>>,
    pub environment: Option<Arc<crate::session_environment::SessionEnvironment>>,
    pub deferred_executor: Option<Arc<dyn awaken_runtime_contract::tool::ToolExecutor>>,
    /// Current durable claim allowed to publish a deferred Sandbox binding.
    /// Replaced at every resolve; the dispatch store remains the lease fence.
    pub deferred_claim: Option<awaken_run_ingress::RunClaim>,
    pub workspace: Option<String>,
    pub model_ref: Option<String>,
    /// Process-local copy of the backend frozen in the Session baseline. It is
    /// validated against the immutable publication before runtime construction.
    pub backend_ref: Option<String>,
    /// Exact Environment projection shared by Native and ACP realization.
    pub environment_projection: Option<FrozenEnvironmentRuntimeProjection>,
    /// Original secret-free Control snapshot retained solely for durable
    /// dispatch to another Worker. Runtime provisioning consumes the decoded
    /// projection above; this value is never a second configuration authority.
    pub environment_snapshot: Option<awaken_protocol_managed::EnvironmentSnapshot>,
    pub memory: Option<Arc<BoundMemory>>,
    pub mcp: Vec<McpGenerationProjection>,
    /// Current Control-issued projection authority. It is a live cache used to
    /// request renewal; durable ownership remains in the Session aggregate.
    pub realization_lease: Option<awaken_protocol_managed::SessionRealizationLease>,
    /// Whether the last Control-frozen projection contains a nonterminal MCP
    /// attachment. This supports lease supervision when effects live behind an
    /// injected realizer and therefore are not stored in `mcp` locally.
    pub has_mcp_projection: bool,
    /// Exact Control-frozen baseline projected for realization. It is never
    /// authored or mutated locally.
    pub baseline: Option<FrozenBaselineRuntimeProjection>,
    /// Exact published delegation targets projected by a managed Session.
    pub delegates: Vec<String>,
    /// Session-local exact toolset replacement; `None` inherits publication.
    pub toolsets: Option<Vec<awaken_agent_contract::ToolsetPolicy>>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_skill_store::SkillVersion>>,
    pub resources: StagedResources,
    pub manifest: Option<awaken_protocol_managed::SessionResourceManifest>,
}

#[derive(Clone, Default)]
pub(crate) struct SessionRuntimeSlots(Arc<Mutex<HashMap<String, SessionRuntimeSlot>>>);

impl SessionRuntimeSlots {
    pub fn read<R>(&self, session: &str, f: impl FnOnce(&SessionRuntimeSlot) -> R) -> Option<R> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get(session)
            .map(f)
    }

    pub fn update<R>(&self, session: &str, f: impl FnOnce(&mut SessionRuntimeSlot) -> R) -> R {
        let mut slots = self.0.lock().expect("session runtime slots mutex poisoned");
        f(slots.entry(session.to_string()).or_default())
    }

    pub fn modify<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRuntimeSlot) -> R,
    ) -> Option<R> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get_mut(session)
            .map(f)
    }

    pub fn remove(&self, session: &str) -> Option<SessionRuntimeSlot> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .remove(session)
    }

    pub fn realization_leases(
        &self,
    ) -> Vec<(String, awaken_protocol_managed::SessionRealizationLease)> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .iter()
            .filter(|(_, slot)| slot.has_mcp_projection)
            .filter_map(|(session_id, slot)| {
                slot.realization_lease
                    .clone()
                    .map(|lease| (session_id.clone(), lease))
            })
            .collect()
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn contains(&self, session: &str) -> bool {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .contains_key(session)
    }
}
