use std::sync::Arc;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::{AgentSpec, ModelBindingSpec, ProviderSpec};
use awaken_stores::InMemoryStore;

#[cfg(feature = "file")]
use awaken_stores::FileStore;

async fn exercise_store(store: Arc<dyn ConfigStore>) {
    let provider = ProviderSpec {
        id: "openai".into(),
        adapter: "openai".into(),
        api_key: Some("sk-test".into()),
        base_url: Some("https://proxy.example.com/v1".into()),
        timeout_secs: 120,
    };
    let model = ModelBindingSpec {
        id: "gpt-4o-mini".into(),
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    };
    let agent = AgentSpec {
        id: "assistant".into(),
        model_id: "gpt-4o-mini".into(),
        system_prompt: "You are helpful.".into(),
        ..Default::default()
    };

    store
        .put(
            "providers",
            &provider.id,
            &serde_json::to_value(&provider).unwrap(),
        )
        .await
        .unwrap();
    store
        .put("models", &model.id, &serde_json::to_value(&model).unwrap())
        .await
        .unwrap();
    store
        .put("agents", &agent.id, &serde_json::to_value(&agent).unwrap())
        .await
        .unwrap();

    assert!(store.exists("providers", "openai").await.unwrap());

    let model_value = store.get("models", "gpt-4o-mini").await.unwrap().unwrap();
    let model_read: ModelBindingSpec = serde_json::from_value(model_value).unwrap();
    assert_eq!(model_read.provider_id, "openai");

    let agent_value = store.get("agents", "assistant").await.unwrap().unwrap();
    let agent_read: AgentSpec = serde_json::from_value(agent_value).unwrap();
    assert_eq!(agent_read.model_id, "gpt-4o-mini");

    let listed = store.list("agents", 0, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    let listed_agent: AgentSpec = serde_json::from_value(listed[0].1.clone()).unwrap();
    assert_eq!(listed_agent.id, "assistant");

    store.delete("providers", "openai").await.unwrap();
    assert!(store.get("providers", "openai").await.unwrap().is_none());
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
