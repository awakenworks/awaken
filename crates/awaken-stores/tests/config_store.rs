use std::sync::Arc;

use awaken_contract::contract::config_store::{ConfigNamespace, ConfigRegistry, ConfigStore};
use awaken_contract::{AgentSpec, ModelSpec, ProviderSpec};
use awaken_stores::InMemoryStore;

#[cfg(feature = "file")]
use awaken_stores::FileStore;

struct AgentNamespace;

impl ConfigNamespace for AgentNamespace {
    const NAMESPACE: &'static str = "agents";
    type Value = AgentSpec;

    fn id(value: &Self::Value) -> &str {
        &value.id
    }
}

struct ModelNamespace;

impl ConfigNamespace for ModelNamespace {
    const NAMESPACE: &'static str = "models";
    type Value = ModelSpec;

    fn id(value: &Self::Value) -> &str {
        &value.id
    }
}

struct ProviderNamespace;

impl ConfigNamespace for ProviderNamespace {
    const NAMESPACE: &'static str = "providers";
    type Value = ProviderSpec;

    fn id(value: &Self::Value) -> &str {
        &value.id
    }
}

async fn exercise_store(store: Arc<dyn ConfigStore>) {
    let agents = ConfigRegistry::<AgentNamespace>::new(store.clone());
    let models = ConfigRegistry::<ModelNamespace>::new(store.clone());
    let providers = ConfigRegistry::<ProviderNamespace>::new(store);

    providers
        .put(&ProviderSpec {
            id: "openai".into(),
            adapter: "openai".into(),
            api_key: Some("sk-test".into()),
            base_url: Some("https://proxy.example.com/v1".into()),
            timeout_secs: 120,
        })
        .await
        .unwrap();
    models
        .put(&ModelSpec {
            id: "gpt-4o-mini".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
        })
        .await
        .unwrap();
    agents
        .put(&AgentSpec {
            id: "assistant".into(),
            model: "gpt-4o-mini".into(),
            system_prompt: "You are helpful.".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(providers.exists("openai").await.unwrap());
    assert_eq!(
        models.get("gpt-4o-mini").await.unwrap().unwrap().provider,
        "openai"
    );
    assert_eq!(
        agents.get("assistant").await.unwrap().unwrap().model,
        "gpt-4o-mini"
    );

    let listed = agents.list(0, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "assistant");

    providers.delete("openai").await.unwrap();
    assert!(providers.get("openai").await.unwrap().is_none());
}

#[tokio::test]
async fn in_memory_store_supports_config_store() {
    exercise_store(Arc::new(InMemoryStore::new()) as Arc<dyn ConfigStore>).await;
}

#[cfg(feature = "file")]
#[tokio::test]
async fn file_store_supports_config_store() {
    let dir = tempfile::tempdir().unwrap();
    exercise_store(Arc::new(FileStore::new(dir.path())) as Arc<dyn ConfigStore>).await;
}
