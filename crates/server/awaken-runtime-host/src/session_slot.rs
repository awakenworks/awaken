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
    /// The one canonical, secret-free effect input. Keep it intact so durable
    /// dispatch can replay the exact generation on the execution Worker without
    /// reconstructing a parallel MCP configuration from live transport state.
    pub request: awaken_session_contract::StageMcpAttachment,
    /// Exact immutable realization identity with only renewable expiry/key
    /// excluded. It prevents a lease extension from changing target,
    /// credential, holder, owner incarnation, epoch, or logical generation.
    pub receipt: awaken_session_contract::McpRealizationReceipt,
    pub server: Option<crate::mcp::McpTransportMaterial>,
    pub native_wiring: Option<crate::mcp::McpWiring>,
    /// Session-owned process for a Native sandbox-stdio MCP generation. ACP
    /// starts its stdio child itself and HTTP generations have no sandbox
    /// process, so both leave this empty.
    pub mcp_process: Option<Arc<dyn awaken_provisioning_contract::ProcessHandle>>,
    pub state: McpProjectionState,
}

/// Process-local projection of the Control-frozen baseline. This is realization
/// input only; the durable Session aggregate remains the authority.
#[derive(Clone)]
pub(crate) struct FrozenBaselineRuntimeProjection {
    pub fingerprint: awaken_session_contract::SessionBaselineFingerprint,
    /// Exact published Agent identity that owns this Session. Runtime effects
    /// that must materialize before the first turn (for example sandbox stdio
    /// MCP) use it to resolve the same immutable publication.
    pub agent_id: String,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
}

/// The sole process-local projection of the frozen Environment snapshot.
/// Native and ACP provisioning both consume this value; no late network or
/// Sandbox override is stored beside it.
#[derive(Clone)]
pub(crate) struct FrozenEnvironmentRuntimeProjection {
    pub fingerprint: awaken_session_contract::EnvironmentFingerprint,
    pub network: awaken_provisioning_contract::NetworkPolicy,
    pub packages: awaken_provisioning_contract::PackageRequirements,
    pub sandbox: Option<awaken_provisioning_contract::SandboxOverride>,
    pub provisioning: awaken_session_contract::SandboxProvisioning,
    pub credential_realization: awaken_runtime_contract::CredentialRealizationProfile,
}

#[derive(Default)]
pub(crate) struct SessionRuntimeSlot {
    /// Serializes the canonical realization phase driver for this Session.
    /// Lease renewal may advance authority while this lock is held, but it must
    /// not start a second Stage/Publish driver. This is deliberately separate
    /// from `lifecycle`, which is re-entered by Environment materialization.
    pub realization: Arc<tokio::sync::Mutex<()>>,
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    pub runtime: Option<Arc<crate::host::SessionCtx>>,
    /// Rebuildable model-only context materialized from the Session baseline's
    /// immutable transcript-prefix reference. Never committed to this Thread.
    pub request_context: Vec<awaken_agent_contract::agent::message::Message>,
    pub environment: Option<Arc<crate::session_environment::SessionEnvironment>>,
    /// Durable binding that recovery must adopt. If it is present while no
    /// environment is resident, cold context creation fails closed instead of
    /// manufacturing an unrelated replacement.
    pub expected_environment_binding: Option<String>,
    pub deferred_executor: Option<Arc<dyn awaken_runtime_contract::tool::ToolExecutor>>,
    /// Current durable dispatch claim used by claim-fenced Resource effects.
    /// This process-local projection is replaced at every claimed resolve; the
    /// dispatch store remains the lease/epoch authority.
    pub dispatch_claim: Option<awaken_run_ingress::RunClaim>,
    pub workspace: Option<String>,
    /// Exact Agent identity copied from the frozen Session baseline. Internal
    /// history/recovery calls do not carry a wire Agent parameter, so they must
    /// resolve the publication through this projection instead of defaulting to
    /// the built-in assistant.
    pub agent_id: Option<String>,
    /// Exact immutable Agent publication supplied by the claimed activation.
    /// Dynamic application publications may not be present in the host catalog,
    /// so lease-only realization must retain this authority instead of trying
    /// to reconstruct it from the backend projection.
    pub published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    /// Immutable non-parent publication closure delivered by the current Run
    /// claim. It is rebuildable execution input, not a Worker-owned catalog.
    pub agent_publications: Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
    pub model_ref: Option<String>,
    /// Process-local copy of the backend frozen in the Session baseline. It is
    /// validated against the immutable publication before runtime construction.
    pub backend_ref: Option<String>,
    /// Exact Environment projection shared by Native and ACP realization.
    pub environment_projection: Option<FrozenEnvironmentRuntimeProjection>,
    /// Original secret-free Control snapshot retained solely for durable
    /// dispatch to another Worker. Runtime provisioning consumes the decoded
    /// projection above; this value is never a second configuration authority.
    pub environment_snapshot: Option<awaken_session_contract::EnvironmentSnapshot>,
    /// Every standard MemoryStore resource frozen for this Session, keyed by
    /// binding id. These exist independently of the optional Awaken automatic
    /// memory extension selected into `memory` below.
    pub memory_bindings: HashMap<String, Arc<BoundMemory>>,
    /// The one explicitly selected automatic recall/extraction binding. This is
    /// never the authority for which standard resources are mounted.
    pub memory: Option<Arc<BoundMemory>>,
    pub mcp: Vec<McpGenerationProjection>,
    /// Current Control-issued projection authority. It is a live cache used to
    /// request renewal; durable ownership remains in the Session aggregate.
    pub realization_lease: Option<awaken_session_contract::SessionRealizationLease>,
    /// Wakes a claim resolver waiting for the Session application's initial
    /// restart reassignment. The lease above remains the only projected fact;
    /// this notification carries no authority or parallel state.
    pub realization_changed: Arc<tokio::sync::Notify>,
    /// Whether the last Control-frozen projection contains a nonterminal MCP
    /// attachment. This supports lease supervision when effects live behind an
    /// injected realizer and therefore are not stored in `mcp` locally.
    pub has_mcp_projection: bool,
    /// The thread entered through the Session application API and its durable
    /// dispatch must therefore cross the claimed Session realization boundary.
    /// It carries no desired-state data of its own.
    pub session_dispatch: bool,
    /// Exact Control-frozen baseline projected for realization. It is never
    /// authored or mutated locally.
    pub baseline: Option<FrozenBaselineRuntimeProjection>,
    /// Exact published delegation targets projected by a managed Session.
    pub delegates: Vec<String>,
    /// Session-local exact toolset replacement; `None` inherits publication.
    pub toolsets: Option<Vec<awaken_agent_contract::ToolsetPolicy>>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_resource_contract::SkillVersion>>,
    pub resources: StagedResources,
    pub manifest: Option<awaken_session_contract::SessionResourceManifest>,
}

#[derive(Clone, Default)]
pub(crate) struct SessionRuntimeSlots(Arc<Mutex<HashMap<String, SessionRuntimeSlot>>>);

impl SessionRuntimeSlots {
    /// Read the one current process-local projection of all frozen prompt
    /// inputs. The durable baseline/resource aggregates remain authoritative;
    /// executors call this at attempt time so late realization cannot leave a
    /// construction-time prompt snapshot behind.
    pub fn prompts(&self, session: &str) -> Vec<String> {
        self.read(session, |slot| {
            let mut prompts = slot.resources.prompts.clone();
            if let Some(baseline) = &slot.baseline {
                prompts.extend(baseline.prompts.clone());
            }
            prompts
        })
        .unwrap_or_default()
    }

    pub fn realization_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.update(session, |slot| slot.realization.clone())
    }

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
    ) -> Vec<(String, awaken_session_contract::SessionRealizationLease)> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .iter()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_only_session_lease_remains_supervised() {
        // FMECA/causal graph: C1 Environment startup outlives the initial lease;
        // C2 the Session has no MCP projection; C3 supervision filters on MCP.
        // E1 C1+C2 must still expose the lease for renewal, otherwise C3 lets
        // the durable fence expire before the Environment receipt can commit.
        let slots = SessionRuntimeSlots::default();
        slots.update("environment-only", |slot| {
            assert!(!slot.has_mcp_projection, "C2");
            slot.realization_lease = Some(awaken_session_contract::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: 30,
            });
        });

        let leases = slots.realization_leases();
        assert_eq!(leases.len(), 1, "E1");
        assert_eq!(leases[0].0, "environment-only", "E1");
    }
}
