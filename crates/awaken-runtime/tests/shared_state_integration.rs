//! Integration tests for shared state via ProfileAccess + StateScope.

use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::profile_store::{ProfileKey, ProfileStore};
use awaken_contract::contract::shared_state::StateScope;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::profile::ProfileAccess;
use awaken_stores::InMemoryStore;

// ── Test types ──

#[derive(Clone, Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct AgentMemory {
    facts: Vec<String>,
}

struct AgentMemoryKey;
impl ProfileKey for AgentMemoryKey {
    const KEY: &'static str = "agent_memory";
    type Value = AgentMemory;
}

#[derive(Clone, Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SharedConfig {
    max_retries: u32,
}

struct SharedConfigKey;
impl ProfileKey for SharedConfigKey {
    const KEY: &'static str = "shared_config";
    type Value = SharedConfig;
}

// ── Test plugin ──

struct SharedStateTestPlugin;

impl Plugin for SharedStateTestPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "shared-state-test",
        }
    }

    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_profile_key::<AgentMemoryKey>()?;
        r.register_profile_key::<SharedConfigKey>()?;
        Ok(())
    }
}

// ── Helper ──

fn build_access() -> ProfileAccess {
    let mut registrar = PluginRegistrar::new_for_test();
    SharedStateTestPlugin
        .register(&mut registrar)
        .expect("register");
    let key_names = registrar.profile_keys_for_test().into_iter().map(|r| r.key);
    let registry = awaken_runtime::profile::ProfileKeyRegistry::new(key_names);
    let store: Arc<dyn ProfileStore> = Arc::new(InMemoryStore::default());
    ProfileAccess::new(store, registry)
}

// ── Tests ──

#[tokio::test]
async fn global_scope_read_write() {
    let access = build_access();
    let key = StateScope::global();

    let mem = AgentMemory {
        facts: vec!["sky is blue".into()],
    };
    access
        .write::<AgentMemoryKey>(key.as_str(), &mem)
        .await
        .unwrap();
    let read = access.read::<AgentMemoryKey>(key.as_str()).await.unwrap();
    assert_eq!(read, mem);
}

#[tokio::test]
async fn parent_thread_scopes_are_isolated() {
    let access = build_access();
    let key_a = StateScope::parent_thread("parent-1");
    let key_b = StateScope::parent_thread("parent-2");

    access
        .write::<AgentMemoryKey>(
            key_a.as_str(),
            &AgentMemory {
                facts: vec!["from parent-1".into()],
            },
        )
        .await
        .unwrap();
    access
        .write::<AgentMemoryKey>(
            key_b.as_str(),
            &AgentMemory {
                facts: vec!["from parent-2".into()],
            },
        )
        .await
        .unwrap();

    let a = access.read::<AgentMemoryKey>(key_a.as_str()).await.unwrap();
    let b = access.read::<AgentMemoryKey>(key_b.as_str()).await.unwrap();
    assert_eq!(a.facts, vec!["from parent-1"]);
    assert_eq!(b.facts, vec!["from parent-2"]);
}

#[tokio::test]
async fn agent_type_scope() {
    let access = build_access();
    let key = StateScope::agent_type("planner");

    access
        .write::<SharedConfigKey>(key.as_str(), &SharedConfig { max_retries: 5 })
        .await
        .unwrap();
    assert_eq!(
        access
            .read::<SharedConfigKey>(key.as_str())
            .await
            .unwrap()
            .max_retries,
        5
    );

    let other = StateScope::agent_type("executor");
    assert_eq!(
        access
            .read::<SharedConfigKey>(other.as_str())
            .await
            .unwrap()
            .max_retries,
        0
    );
}

#[tokio::test]
async fn custom_scope_works() {
    let access = build_access();
    let key = StateScope::new("region::us-west::cluster-1");

    let mem = AgentMemory {
        facts: vec!["custom scope".into()],
    };
    access
        .write::<AgentMemoryKey>(key.as_str(), &mem)
        .await
        .unwrap();
    assert_eq!(
        access.read::<AgentMemoryKey>(key.as_str()).await.unwrap(),
        mem
    );
}

#[tokio::test]
async fn multiple_namespaces_same_key() {
    let access = build_access();
    let key = StateScope::global();

    let mem = AgentMemory {
        facts: vec!["fact".into()],
    };
    let config = SharedConfig { max_retries: 3 };

    access
        .write::<AgentMemoryKey>(key.as_str(), &mem)
        .await
        .unwrap();
    access
        .write::<SharedConfigKey>(key.as_str(), &config)
        .await
        .unwrap();

    assert_eq!(
        access.read::<AgentMemoryKey>(key.as_str()).await.unwrap(),
        mem
    );
    assert_eq!(
        access.read::<SharedConfigKey>(key.as_str()).await.unwrap(),
        config
    );

    let entries = access.list(key.as_str()).await.unwrap();
    assert_eq!(entries.len(), 2);
}

#[tokio::test]
async fn delete_is_idempotent() {
    let access = build_access();
    let key = StateScope::global();

    access.delete::<AgentMemoryKey>(key.as_str()).await.unwrap();

    access
        .write::<AgentMemoryKey>(
            key.as_str(),
            &AgentMemory {
                facts: vec!["gone".into()],
            },
        )
        .await
        .unwrap();
    access.delete::<AgentMemoryKey>(key.as_str()).await.unwrap();
    access.delete::<AgentMemoryKey>(key.as_str()).await.unwrap();

    assert_eq!(
        access.read::<AgentMemoryKey>(key.as_str()).await.unwrap(),
        AgentMemory::default()
    );
}

#[tokio::test]
async fn overwrite_replaces_value() {
    let access = build_access();
    let key = StateScope::thread("t-1");

    access
        .write::<SharedConfigKey>(key.as_str(), &SharedConfig { max_retries: 1 })
        .await
        .unwrap();
    access
        .write::<SharedConfigKey>(key.as_str(), &SharedConfig { max_retries: 9 })
        .await
        .unwrap();

    assert_eq!(
        access
            .read::<SharedConfigKey>(key.as_str())
            .await
            .unwrap()
            .max_retries,
        9
    );
}

#[tokio::test]
async fn unregistered_namespace_errors() {
    let access = build_access();

    struct Rogue;
    impl ProfileKey for Rogue {
        const KEY: &'static str = "rogue_key";
        type Value = String;
    }

    let err = access.read::<Rogue>(StateScope::global().as_str()).await;
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("not registered"));
}

#[tokio::test]
async fn different_keys_are_isolated() {
    let access = build_access();

    access
        .write::<AgentMemoryKey>(
            "alice",
            &AgentMemory {
                facts: vec!["profile data".into()],
            },
        )
        .await
        .unwrap();
    access
        .write::<AgentMemoryKey>(
            StateScope::new("alice_scope").as_str(),
            &AgentMemory {
                facts: vec!["shared data".into()],
            },
        )
        .await
        .unwrap();

    let profile = access.read::<AgentMemoryKey>("alice").await.unwrap();
    let shared = access
        .read::<AgentMemoryKey>(StateScope::new("alice_scope").as_str())
        .await
        .unwrap();

    assert_eq!(profile.facts, vec!["profile data"]);
    assert_eq!(shared.facts, vec!["shared data"]);
}
