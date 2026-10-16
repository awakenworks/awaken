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
                    slot.realization.resources.mounts.clone(),
                    slot.inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice()),
                    slot.realization.content_delivery,
                )
            })
            .unwrap_or_default()
    }

    pub(crate) fn register_thread_memory(&self, thread: &str, memory: Option<Arc<MemoryBinding>>) {
        self.sessions.update_realization(thread, |r| {
            r.memory_bindings.clear();
            r.memory = memory;
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
            let selected = binding_id.and_then(|id| r.memory_bindings.get(id).cloned());
            r.memory = selected.clone();
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
