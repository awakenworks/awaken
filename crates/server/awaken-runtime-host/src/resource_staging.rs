// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Resource staging owns the original projection mutex and cancellation/cache fields under the canonical installed-slot lock; publication still checks the existing realization proof.
//! Resource staging of the realization executor: the frozen Session Resources
//! as the Environment receives them. It projects the provider-effective sandbox
//! spec, registers the staged Resources and the active manifest, and validates
//! the managed mount layout against a provider.

use crate::memory_binding::MemoryBinding;
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
    pub(crate) file_content_source:
        Arc<dyn awaken_resource_contract::FileContentSource<ResourceOperationFence>>,
    pub(crate) memory: awaken_resource_contract::MemoryExecutionContent,
    pub(crate) resource_validator:
        Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    pub(crate) repository_binding_verifier: Option<
        Arc<dyn awaken_resource_contract::RepositoryBindingVerifier<ResourceOperationFence>>,
    >,
    pub(crate) fetches_resources_upstream: bool,
    /// The same exact materializer selected for the Host, used only at a Git effect.
    pub(crate) credential_materializer:
        Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    pub(crate) memory_reference_encoder:
        Option<Arc<dyn crate::memory_binding::RuntimeMemoryReferenceEncoder>>,
}

impl ResourceStaging {
    pub(crate) fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::MountRequirement> {
        self.sessions
            .modify_session(thread, |slot| {
                crate::provisioning::project_session_mounts(
                    slot.realization.staging.resources.mounts.clone(),
                    slot.inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice()),
                    slot.realization.staging.content_delivery,
                )
            })
            .unwrap_or_default()
    }

    pub(crate) fn register_thread_memory(&self, thread: &str, memory: Option<Arc<MemoryBinding>>) {
        self.sessions.update_realization(thread, |r| {
            r.staging.memory_bindings.clear();
            r.staging.memory = memory;
        });
    }

    /// Select the exact Awaken extension binding authored by the published
    /// Agent. A missing binding clears any prior selection before failing, so a
    /// rebuilt context cannot retain stale automatic-memory authority.
    pub(crate) fn select_thread_memory_binding(
        &self,
        thread: &str,
        binding_id: Option<&str>,
    ) -> Result<Option<Arc<MemoryBinding>>, String> {
        let selected = self.sessions.update_realization(thread, |r| {
            let selected = binding_id.and_then(|id| r.staging.memory_bindings.get(id).cloned());
            r.staging.memory = selected.clone();
            selected
        });
        match (binding_id, selected) {
            (_, Some(selected)) => Ok(Some(selected)),
            (None, None) => Ok(None),
            (Some(binding_id), None) => Err(format!(
                "automatic memory binding `{binding_id}` is not mounted for this Session"
            )),
        }
    }
}

#[cfg(test)]
mod tests;

/// Per-Session state held under the canonical installed-slot mutex.
#[derive(Default)]
pub(crate) struct ResourceStagingState {
    /// Serializes one complete Resource-generation compare/realize/publish
    /// transition. The global lock order is `realization -> lifecycle ->
    /// resource_projection`; callers may acquire any suffix but never reverse
    /// it. Keeping this separate lets projection installation remain atomic to
    /// Runtime observers without re-entering `lifecycle` during Resource sync.
    pub projection: Arc<tokio::sync::Mutex<()>>,
    /// Derived once while the Session context is built. Prompt projection,
    /// mount realization, Native tools, and ACP export all consume this value;
    /// none may independently choose another delivery path.
    pub content_delivery: Option<crate::frozen_inputs::ManagedContentDelivery>,
    /// Every standard MemoryStore resource frozen for this Session, keyed by
    /// binding id. These exist independently of the optional Awaken automatic
    /// memory extension selected into `memory` below.
    pub memory_bindings: std::collections::HashMap<String, Arc<MemoryBinding>>,
    /// The one explicitly selected automatic recall/extraction binding. This is
    /// never the authority for which standard resources are mounted.
    pub memory: Option<Arc<MemoryBinding>>,
    /// The Session's MCP generation owners, quiescence fence, and frozen
    /// attachment marker. Other Host modules use only its methods.
    pub mcp: awaken_session_mcp::SessionMcpState,
    /// Cancellation of the current Resource preparation only. This carries no
    /// ownership: `authority` and Control remain the sole authorities.
    pub preparation: Option<Arc<awaken_runtime_contract::CancellationToken>>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_resource_contract::SkillVersion>>,
    /// Managed-filesystem Skill discovery metadata. Full instructions remain
    /// in the exact frozen bundle (and its later filesystem projection) and
    /// are read on demand by the ordinary `read` tool.
    pub skill_prompt: Option<crate::skill_delivery::FilesystemSkillPrompt>,
    pub resources: crate::staged_resources::StagedResources,
    pub manifest: Option<awaken_session_configuration_contract::resource::SessionResourceManifest>,
    /// Exact aggregate-owned physical replacement currently being replayed.
    /// This is a rebuildable command projection, not completion state: the
    /// durable Resource aggregate remains the only active/pending authority.
    pub transition:
        Option<awaken_session_configuration_contract::resource::SessionResourceTransition>,
    /// Fingerprint of the exact transition + optional Run claim whose immutable
    /// inputs were compiled into `resources`, `memory_bindings`, and `skills`.
    /// This avoids duplicate File/Vault/catalog reads between deferred staging
    /// and the same process's later physical Environment convergence.
    pub effect_key: Option<String>,
    /// How the resident Environment entered this process. Fresh substrates and
    /// adopted providers require different idempotent mount completion, but the
    /// aggregate transition remains their single desired-state authority.
    pub reconciliation: crate::staged_resources::EnvironmentResourceReconciliation,
}
