use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::profile_store::{ProfileKey, ProfileOwner};
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::profile::{ProfileAccess, ProfileKeyRegistry};
use awaken_stores::InMemoryStore;

// ── Test types ──────────────────────────────────────────────────────

struct AgentMemoryKey;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AgentMemory {
    facts: Vec<String>,
}

impl ProfileKey for AgentMemoryKey {
    const KEY: &'static str = "test-plugin.memory";
    type Value = AgentMemory;
}

// ── Test plugin ─────────────────────────────────────────────────────

struct TestProfilePlugin;

impl Plugin for TestProfilePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "test-profile-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_profile_key::<AgentMemoryKey>()?;
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn build_access_from_plugin(plugin: &dyn Plugin) -> ProfileAccess {
    let mut registrar = PluginRegistrar::new_for_test();
    plugin.register(&mut registrar).unwrap();
    let keys = registrar.profile_keys_for_test();
    let registry = ProfileKeyRegistry::new(keys.into_iter().map(|k| k.key));
    let store: Arc<dyn awaken_contract::contract::profile_store::ProfileStore> =
        Arc::new(InMemoryStore::new());
    ProfileAccess::new(store, registry)
}

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn plugin_registers_key_and_access_reads_writes() {
    let access = build_access_from_plugin(&TestProfilePlugin);

    let alice = ProfileOwner::Agent("alice".into());
    let bob = ProfileOwner::Agent("bob".into());

    // Read default (missing key returns Default)
    let val = access.read::<AgentMemoryKey>(&alice).await.unwrap();
    assert_eq!(val, AgentMemory::default());

    // Write
    let memory = AgentMemory {
        facts: vec!["likes rust".into(), "hates nulls".into()],
    };
    access
        .write::<AgentMemoryKey>(&alice, &memory)
        .await
        .unwrap();

    // Read back
    let loaded = access.read::<AgentMemoryKey>(&alice).await.unwrap();
    assert_eq!(loaded, memory);

    // Isolation: bob still has default
    let bob_val = access.read::<AgentMemoryKey>(&bob).await.unwrap();
    assert_eq!(bob_val, AgentMemory::default());

    // Write for bob and verify both exist independently
    let bob_memory = AgentMemory {
        facts: vec!["prefers python".into()],
    };
    access
        .write::<AgentMemoryKey>(&bob, &bob_memory)
        .await
        .unwrap();
    assert_eq!(access.read::<AgentMemoryKey>(&alice).await.unwrap(), memory);
    assert_eq!(
        access.read::<AgentMemoryKey>(&bob).await.unwrap(),
        bob_memory
    );
}

#[tokio::test]
async fn clear_owner_removes_all_entries() {
    let access = build_access_from_plugin(&TestProfilePlugin);

    let owner = ProfileOwner::Agent("charlie".into());
    let memory = AgentMemory {
        facts: vec!["fact one".into()],
    };
    access
        .write::<AgentMemoryKey>(&owner, &memory)
        .await
        .unwrap();

    // Verify written
    let entries = access.list(&owner).await.unwrap();
    assert_eq!(entries.len(), 1);

    // Clear
    access.clear_owner(&owner).await.unwrap();

    // Verify gone (reads default)
    let val = access.read::<AgentMemoryKey>(&owner).await.unwrap();
    assert_eq!(val, AgentMemory::default());
    assert!(access.list(&owner).await.unwrap().is_empty());
}
