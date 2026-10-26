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
    ) -> Result<Option<(String, Arc<crate::MemoryBinding>)>, RunError> {
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
        let handle = Arc::new(crate::PlatformMemoryHandle::from_roles(
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
            Arc::new(crate::MemoryBinding::new(
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
    ) -> Result<Arc<crate::MemoryBinding>, RunError> {
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

#[cfg(test)]
mod tests {
    use super::{Arc, MemoryBindingCompiler};
    use awaken_ext_memory::MemoryStoreHandle as _;
    use awaken_resource_contract::{
        ConfigVersion, LiveResourceBindingVerifier, MemoryStoreConfigVersion, ResourceAccess,
        ResourceRegistryError,
    };
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, SessionResourceManifest,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct LiveBinding {
        revoked: AtomicBool,
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LiveResourceBindingVerifier for LiveBinding {
        async fn verify_memory_binding(
            &self,
            workspace: &str,
            store: &str,
            version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            assert_eq!(
                (workspace, store, version),
                ("workspace", "memory", ConfigVersion(4))
            );
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.revoked.load(Ordering::SeqCst) {
                Err(ResourceRegistryError::NotFound(store.into()))
            } else {
                Ok(())
            }
        }

        async fn verify_repository_binding(
            &self,
            _workspace: &str,
            _store: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            unreachable!("a Memory binding never asks the Repository authority")
        }
    }

    #[tokio::test]
    async fn resolved_and_frozen_memory_share_one_compiler_and_live_authority() {
        // Cause/effect matrix: C1 resolved/frozen input; C2 read-only/writable;
        // C3 configured validator live/revoked/missing. M1 C1+C2+live => exact
        // scope/store/version and maximum access retained; M2 revoked => a later
        // write refuses at the original validator before content access; M3 no
        // validator => compilation refuses. M4 frozen binding absent => refuse.
        // Nothing is staged, and compiling either form attaches no commit.
        for writable in [false, true] {
            let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                awaken_time::system_clock(),
            ));
            let content = awaken_resource_contract::MemoryExecutionContent::new(repository.clone());
            let live = Arc::new(LiveBinding::default());
            let compiler = MemoryBindingCompiler::new(content.clone(), Some(live.clone()));
            let input = ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: "notes".into(),
                source: ResolvedInputSource::MemoryStore {
                    memory_store_id: "memory".into(),
                    config: MemoryStoreConfigVersion {
                        memory_store_id: "memory".into(),
                        version: ConfigVersion(4),
                        retention_policy: Default::default(),
                    },
                },
                mount_path: "/mnt/memory".into(),
                access: if writable {
                    ResourceAccess::ReadWrite
                } else {
                    ResourceAccess::ReadOnly
                },
                instructions: None,
            };
            let manifest = SessionResourceManifest::new(
                "workspace",
                ResolvedSessionResources::try_new(vec![input.clone()], Vec::new()).unwrap(),
            );
            let (id, resolved) = compiler
                .compile("session", "workspace", &input, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(id, "notes", "M1");
            let frozen = compiler
                .compile_frozen("session", &manifest, "notes")
                .await
                .unwrap();
            for binding in [resolved, frozen] {
                assert_eq!(
                    (
                        binding.session_thread_id.as_str(),
                        binding.workspace_id.as_str(),
                        binding.memory_store_id.as_str(),
                        binding.memory_config_version
                    ),
                    ("session", "workspace", "memory", 4),
                    "M1",
                );
                assert_eq!(binding.extraction_enabled, writable, "M1");
                assert!(
                    binding.recall_enabled && binding.extraction_commit().unwrap().is_none(),
                    "M1"
                );
                assert!(binding.entries().await.unwrap().is_empty(), "M1");
                let observed = live.reads.load(Ordering::SeqCst);
                live.revoked.store(true, Ordering::SeqCst);
                assert!(binding.write("pref", "prefers tea").await.is_err(), "M2");
                assert_eq!(live.reads.load(Ordering::SeqCst), observed + 1, "M2");
                live.revoked.store(false, Ordering::SeqCst);
            }
            assert!(
                MemoryBindingCompiler::new(content, None)
                    .compile("session", "workspace", &input, None)
                    .await
                    .is_err(),
                "M3"
            );
            assert!(
                compiler
                    .compile_frozen("session", &manifest, "absent")
                    .await
                    .is_err(),
                "M4"
            );
            assert!(
                awaken_resource_contract::MemoryVersionQueries::list_versions(
                    repository.as_ref(),
                    "memory"
                )
                .await
                .unwrap()
                .is_empty(),
                "M2 no writes"
            );
        }
    }
}
