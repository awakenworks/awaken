use awaken_runtime::registry::RegistrySet;
use awaken_server_contract::PreparedSkillSpecs;

use super::provider_cache::{ProviderExecutorCache, StagedCapabilityCache};
use super::{ConfigRuntimeError, ConfigRuntimeManager, PreparedMcpRegistry};

/// Staged, pre-validated catalog ready for atomic commit to the in-process
/// runtime.
///
/// Build this via `publish()`, then call `commit_to()` to atomically install
/// the catalog into the live runtime. If the config fingerprint has changed
/// since the catalog was prepared, `commit_to` fails closed and leaves the
/// active catalog untouched.
pub(super) struct RuntimeCatalogInstaller {
    snapshot_fingerprint: u64,
    candidate: RegistrySet,
    next_provider_cache: ProviderExecutorCache,
    staged_capabilities: StagedCapabilityCache,
    prepared_skills: Option<Box<dyn PreparedSkillSpecs>>,
    prepared_mcp: PreparedMcpRegistry,
}

impl RuntimeCatalogInstaller {
    pub(super) fn new(
        snapshot_fingerprint: u64,
        candidate: RegistrySet,
        next_provider_cache: ProviderExecutorCache,
        staged_capabilities: StagedCapabilityCache,
        prepared_skills: Option<Box<dyn PreparedSkillSpecs>>,
        prepared_mcp: PreparedMcpRegistry,
    ) -> Self {
        Self {
            snapshot_fingerprint,
            candidate,
            next_provider_cache,
            staged_capabilities,
            prepared_skills,
            prepared_mcp,
        }
    }

    /// Atomically install the prepared catalog into the live runtime.
    ///
    /// Before crossing the point-of-no-return (the runtime registry swap),
    /// re-reads the current config from the store and compares its fingerprint
    /// against the one captured when this installer was built. A mismatch means
    /// the catalog was compiled from stale config: `FingerprintChanged` is
    /// returned and the active catalog is left untouched.
    pub(super) async fn commit_to(
        self,
        manager: &ConfigRuntimeManager,
    ) -> Result<u64, ConfigRuntimeError> {
        let Self {
            snapshot_fingerprint,
            candidate,
            next_provider_cache,
            staged_capabilities,
            prepared_skills,
            prepared_mcp,
        } = self;

        // Fail-closed fingerprint guard: re-read config from store to detect
        // any write that arrived between prepare and now. If the fingerprint
        // drifted, this catalog was built from stale data — abort before any
        // irreversible in-process change.
        let current = manager.load_managed_config().await?;
        if current.fingerprint != snapshot_fingerprint {
            prepared_mcp.cleanup().await;
            return Err(ConfigRuntimeError::FingerprintChanged);
        }

        // Materialize the registry set to install (no-op for unversioned runtimes).
        let runtime_set = match manager.published_or_candidate_registry_set(candidate).await {
            Ok(set) => set,
            Err(error) => {
                prepared_mcp.cleanup().await;
                return Err(error);
            }
        };

        // POINT OF NO RETURN: atomic runtime swap. All subsequent steps are
        // commit-only — no error returns after this succeeds.
        let version = match manager.runtime.replace_registry_set(runtime_set) {
            Some(v) => v,
            None => {
                prepared_mcp.cleanup().await;
                return Err(ConfigRuntimeError::RuntimeNotConfigurable);
            }
        };

        if let Some(skills) = prepared_skills {
            skills.commit();
        }

        {
            let mut cache = manager.provider_cache.lock();
            cache.replace_executors(next_provider_cache);
            cache.commit_capabilities(staged_capabilities);
        }

        let previous_mcp = if prepared_mcp.state_changed {
            let mut active = manager.active_mcp_registry.lock();
            std::mem::replace(&mut *active, prepared_mcp.next_state)
        } else {
            None
        };

        *manager.last_applied_fingerprint.write() = Some(snapshot_fingerprint);

        if let Some(previous) = previous_mcp
            && let Err(error) = previous.handle.close().await
        {
            tracing::warn!(
                error = %error,
                "failed to close replaced MCP registry"
            );
        }

        Ok(version)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use awaken_server_contract::{AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelSpec, ProviderSpec};

    use super::super::provider_cache::ProviderRuntimeCache;
    use super::*;

    fn base_seed() -> BuiltinSeedSet {
        BuiltinSeedSet {
            binary_version: "test".into(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "p".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("m", "p", "upstream")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "a".into(),
                    model_id: "m".into(),
                    system_prompt: "v1".into(),
                    max_rounds: 1,
                    ..Default::default()
                })),
            ],
        }
    }

    /// When the config store is mutated between the time the catalog is built
    /// and the time `commit_to` runs, `FingerprintChanged` must be returned
    /// and the active runtime catalog must remain unchanged.
    #[tokio::test]
    async fn commit_to_fails_closed_when_fingerprint_changes() {
        let (manager, store) = super::super::tests::make_manager_with_store().await;
        manager.apply_seed(&base_seed()).await.expect("seed");
        manager.apply().await.expect("initial apply");

        let initial_version = manager
            .runtime
            .registry_version()
            .expect("version after apply");

        // Capture the fingerprint of the current committed config.
        let managed = manager.load_managed_config().await.expect("load config");
        let snapshot_fingerprint = managed.fingerprint;

        // Mutate the store so the fingerprint changes before commit_to runs.
        let changed = awaken_server_contract::ConfigRecord {
            spec: AgentSpec {
                id: "a".into(),
                model_id: "m".into(),
                system_prompt: "v2-concurrent-write".into(),
                max_rounds: 1,
                ..Default::default()
            },
            meta: awaken_server_contract::RecordMeta::new_user(),
        };
        store
            .put(
                "agents",
                "a",
                &changed.to_value().expect("serialize changed agent"),
            )
            .await
            .expect("write changed config");

        // Confirm the store now yields a different fingerprint.
        let new_managed = manager.load_managed_config().await.expect("reload");
        assert_ne!(
            snapshot_fingerprint, new_managed.fingerprint,
            "store mutation must change the fingerprint"
        );

        // Build an installer with the OLD fingerprint (as if prepare happened before the write).
        let registry_set = manager
            .runtime
            .registry_handle()
            .expect("registry handle")
            .snapshot()
            .into_registries();

        let staged = ProviderRuntimeCache::default().stage_capability_snapshots(
            &[],
            std::collections::HashMap::new(),
            &HashSet::new(),
            |_| String::new(),
            std::time::SystemTime::UNIX_EPOCH,
        );

        let prepared_mcp = PreparedMcpRegistry {
            tool_registry: None,
            next_state: None,
            state_changed: false,
        };

        let installer = RuntimeCatalogInstaller::new(
            snapshot_fingerprint,
            registry_set,
            ProviderExecutorCache::default(),
            staged,
            None,
            prepared_mcp,
        );

        // commit_to must fail closed.
        let err = installer
            .commit_to(&manager)
            .await
            .expect_err("commit_to must fail on fingerprint mismatch");
        assert!(
            matches!(err, ConfigRuntimeError::FingerprintChanged),
            "expected FingerprintChanged, got: {err:?}"
        );

        // Active catalog must be unchanged.
        assert_eq!(
            manager
                .runtime
                .registry_version()
                .expect("version after failed commit"),
            initial_version,
            "active catalog version must not change on fingerprint mismatch"
        );
    }

    /// When the fingerprint matches, `commit_to` succeeds and the runtime
    /// catalog is updated to a new version.
    #[tokio::test]
    async fn commit_to_succeeds_and_updates_catalog_on_fingerprint_match() {
        let (manager, _store) = super::super::tests::make_manager_with_store().await;
        manager.apply_seed(&base_seed()).await.expect("seed");
        manager.apply().await.expect("initial apply");

        let initial_version = manager
            .runtime
            .registry_version()
            .expect("version after apply");

        let managed = manager.load_managed_config().await.expect("load config");
        let snapshot_fingerprint = managed.fingerprint;

        let registry_set = manager
            .runtime
            .registry_handle()
            .expect("registry handle")
            .snapshot()
            .into_registries();

        let staged = ProviderRuntimeCache::default().stage_capability_snapshots(
            &[],
            std::collections::HashMap::new(),
            &HashSet::new(),
            |_| String::new(),
            std::time::SystemTime::UNIX_EPOCH,
        );

        let prepared_mcp = PreparedMcpRegistry {
            tool_registry: None,
            next_state: None,
            state_changed: false,
        };

        let installer = RuntimeCatalogInstaller::new(
            snapshot_fingerprint,
            registry_set,
            ProviderExecutorCache::default(),
            staged,
            None,
            prepared_mcp,
        );

        let new_version = installer
            .commit_to(&manager)
            .await
            .expect("commit_to must succeed when fingerprint matches");

        assert!(
            new_version > initial_version,
            "catalog version must advance on successful commit"
        );
        assert_eq!(
            *manager.last_applied_fingerprint.read(),
            Some(snapshot_fingerprint),
            "last_applied_fingerprint must be set to the committed snapshot fingerprint"
        );
    }
}
