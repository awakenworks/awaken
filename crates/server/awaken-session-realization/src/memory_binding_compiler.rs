// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: One stateless binding law retains original content and verification roles without installed Sessions or a staging authority.
//! Compile resolved and frozen Memory inputs through the same binding law.
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_types::RunError;
use std::sync::Arc;

/// Memory-specific compilation through selected content and verification roles.
#[derive(Clone)]
pub struct MemoryBindingCompiler {
    memory: awaken_resource_contract::MemoryExecutionContent,
    resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
}
impl MemoryBindingCompiler {
    pub fn new(
        memory: awaken_resource_contract::MemoryExecutionContent,
        resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    ) -> Self {
        Self {
            memory,
            resource_validator,
        }
    }
    pub async fn compile(
        &self,
        session_thread: &str,
        workspace: &str,
        input: &session_resource::ResolvedInput,
        materialization_reference: Option<String>,
    ) -> Result<Option<(String, Arc<crate::memory_binding::MemoryBinding>)>, RunError> {
        let session_resource::ResolvedInputSource::MemoryStore {
            memory_store_id,
            config,
        } = &input.source
        else {
            return Ok(None);
        };
        let writable = input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
        if let Some(reference) = &materialization_reference {
            // Remote Memory claim decision table: active + exact config =>
            // snapshot preflight succeeds; archived/config-changed/stale claim
            // fails before the mounter reuses a prior projection.
            self.memory
                .snapshots()
                .snapshot_heads(reference)
                .await
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        let handle = Arc::new(crate::memory_binding::PlatformMemoryHandle::from_roles(
            self.memory.clone(),
            materialization_reference
                .clone()
                .unwrap_or_else(|| memory_store_id.to_string()),
            writable,
        ));
        let resource_validator = if materialization_reference.is_some() {
            None
        } else {
            Some(
                self.resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "Memory extraction requires a configured resource binding validator",
                        )
                    })?
                    .clone(),
            )
        };
        Ok(Some((
            input.binding_id.to_string(),
            Arc::new(crate::memory_binding::MemoryBinding::new(
                session_thread,
                workspace,
                handle,
                resource_validator,
                config,
                writable,
            )),
        )))
    }
    pub async fn compile_frozen(
        &self,
        session_thread: &str,
        manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
        binding_id: &str,
    ) -> Result<Arc<crate::memory_binding::MemoryBinding>, RunError> {
        let input = manifest
            .resources
            .inputs()
            .iter()
            .find(|input| input.binding_id.as_str() == binding_id)
            .ok_or_else(|| {
                RunError::internal(format!(
                    "frozen Memory binding `{binding_id}` is absent from the Session manifest"
                ))
            })?;
        if !matches!(
            &input.source,
            awaken_session_configuration_contract::resource::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(RunError::internal(format!(
                "frozen binding `{binding_id}` is not a MemoryStore"
            )));
        }
        self.compile(session_thread, &manifest.workspace_id, input, None)
            .await?
            .map(|(_, memory)| memory)
            .ok_or_else(|| RunError::internal("frozen Memory input did not compile"))
    }
}
