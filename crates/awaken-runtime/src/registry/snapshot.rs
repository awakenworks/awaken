//! Versioned registry snapshots for runtime resolution.

use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use super::traits::RegistrySet;

#[derive(Clone)]
pub struct RegistrySnapshot {
    version: u64,
    registries: RegistrySet,
}

impl RegistrySnapshot {
    pub fn new(version: u64, registries: RegistrySet) -> Self {
        Self {
            version,
            registries,
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn registries(&self) -> &RegistrySet {
        &self.registries
    }

    pub fn into_registries(self) -> RegistrySet {
        self.registries
    }
}

#[derive(Clone)]
pub struct RegistryHandle {
    snapshot: Arc<RwLock<RegistrySnapshot>>,
    /// Serializes mutations so that build/validate done by `update` cannot
    /// observe a stale base snapshot relative to a concurrent `replace` or
    /// `update`. Held only by writers; readers go straight to `snapshot`.
    update_lock: Arc<Mutex<()>>,
}

impl RegistryHandle {
    pub fn new(registries: RegistrySet) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(RegistrySnapshot::new(1, registries))),
            update_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn snapshot(&self) -> RegistrySnapshot {
        self.snapshot.read().clone()
    }

    pub fn version(&self) -> u64 {
        self.snapshot.read().version()
    }

    pub fn replace(&self, registries: RegistrySet) -> u64 {
        let _writer = self.update_lock.lock();
        let mut snapshot = self.snapshot.write();
        let version = snapshot.version().saturating_add(1);
        *snapshot = RegistrySnapshot::new(version, registries);
        version
    }

    /// Build the next registry set from the current snapshot and publish it.
    ///
    /// The closure runs OUTSIDE the snapshot's write lock so that heavy work
    /// (deep copies, validation) does not block readers calling `snapshot()`
    /// or `version()`. Concurrent writers are serialized by `update_lock`,
    /// preserving the invariant that each update observes the predecessor's
    /// committed state — see `concurrent_provider_registration_preserves_all_updates`.
    pub fn update<E>(
        &self,
        update: impl FnOnce(&RegistrySet) -> Result<RegistrySet, E>,
    ) -> Result<u64, E> {
        let _writer = self.update_lock.lock();
        let base = self.snapshot.read().clone();
        let registries = update(base.registries())?;
        let mut snapshot = self.snapshot.write();
        let version = snapshot.version().saturating_add(1);
        *snapshot = RegistrySnapshot::new(version, registries);
        Ok(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapBackendRegistry, MapModelRegistry, MapPluginSource,
        MapProviderRegistry, MapToolRegistry,
    };
    use awaken_contract::registry_spec::AgentSpec;

    fn make_registry_set(agent_id: &str) -> RegistrySet {
        let mut agents = MapAgentSpecRegistry::new();
        agents
            .register_spec(AgentSpec {
                id: agent_id.into(),
                model_id: "default".into(),
                system_prompt: "test".into(),
                ..Default::default()
            })
            .expect("register test agent");

        let mut models = MapModelRegistry::new();
        models
            .register_model(
                "default",
                crate::registry::ModelBinding {
                    provider_id: "provider".into(),
                    upstream_model: "gpt-test".into(),
                },
            )
            .expect("register test model");

        RegistrySet {
            agents: Arc::new(agents),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(models),
            providers: Arc::new(MapProviderRegistry::new()),
            plugins: Arc::new(MapPluginSource::new()),
            backends: Arc::new(MapBackendRegistry::new()),
        }
    }

    #[test]
    fn new_starts_at_version_one() {
        let handle = RegistryHandle::new(make_registry_set("agent-a"));
        assert_eq!(handle.version(), 1);
        assert_eq!(
            handle.snapshot().registries().agents.agent_ids(),
            vec!["agent-a"]
        );
    }

    #[test]
    fn replace_publishes_new_version() {
        let handle = RegistryHandle::new(make_registry_set("agent-a"));
        let version = handle.replace(make_registry_set("agent-b"));
        assert_eq!(version, 2);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.version(), 2);
        assert_eq!(snapshot.registries().agents.agent_ids(), vec!["agent-b"]);
    }

    #[test]
    fn update_publishes_new_version() {
        let handle = RegistryHandle::new(make_registry_set("agent-a"));
        let version = handle
            .update::<()>(|_| Ok(make_registry_set("agent-b")))
            .expect("update succeeds");
        assert_eq!(version, 2);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.version(), 2);
        assert_eq!(snapshot.registries().agents.agent_ids(), vec!["agent-b"]);
    }

    #[test]
    fn update_does_not_block_readers_while_closure_runs() {
        use std::sync::mpsc;

        let handle = Arc::new(RegistryHandle::new(make_registry_set("agent-a")));
        let (closure_started_tx, closure_started_rx) = mpsc::channel::<()>();
        let (release_closure_tx, release_closure_rx) = mpsc::channel::<()>();

        let writer_handle = Arc::clone(&handle);
        let writer = std::thread::spawn(move || {
            writer_handle
                .update::<()>(|registries| {
                    closure_started_tx
                        .send(())
                        .expect("notify reader closure has started");
                    release_closure_rx
                        .recv()
                        .expect("reader must release closure before commit");
                    assert_eq!(registries.agents.agent_ids(), vec!["agent-a"]);
                    Ok(make_registry_set("agent-b"))
                })
                .expect("update succeeds");
        });

        closure_started_rx
            .recv()
            .expect("update closure must signal start");
        // Old implementation held the snapshot's write lock for the closure body,
        // which would block these reads indefinitely. New implementation only
        // takes the write lock to publish, so readers proceed immediately.
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.version(), 1);
        assert_eq!(snapshot.registries().agents.agent_ids(), vec!["agent-a"]);
        assert_eq!(handle.version(), 1);

        release_closure_tx
            .send(())
            .expect("release update closure to publish");
        writer.join().expect("writer thread must not panic");

        let snapshot = handle.snapshot();
        assert_eq!(snapshot.version(), 2);
        assert_eq!(snapshot.registries().agents.agent_ids(), vec!["agent-b"]);
    }
}
