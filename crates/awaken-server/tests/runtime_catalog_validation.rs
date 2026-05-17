//! Integration test for the runtime apply path's catalog validation.
//!
//! Pre-populates the config store with a bare agent spec carrying an
//! unparseable `allowed_tool_patterns` entry, then calls `apply()` and
//! asserts the runtime refuses to publish — mirroring the write-path and
//! seed-time guards so direct-store writes and migrations can't smuggle
//! invalid pattern syntax past apply.

use std::sync::Arc;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::{AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelBindingSpec, ProviderSpec};
use awaken_runtime::AgentRuntime;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::services::config_runtime::ConfigRuntimeManager;
use awaken_stores::InMemoryStore;
use serde_json::json;

struct StubExec;
#[async_trait::async_trait]
impl awaken_contract::contract::executor::LlmExecutor for StubExec {
    async fn execute(
        &self,
        _: awaken_contract::contract::executor::InferenceRequest,
    ) -> Result<
        awaken_contract::contract::inference::StreamResult,
        awaken_contract::contract::executor::InferenceExecutionError,
    > {
        Ok(awaken_contract::contract::inference::StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(awaken_contract::contract::inference::TokenUsage::default()),
            stop_reason: Some(awaken_contract::contract::inference::StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }
    fn name(&self) -> &str {
        "stub"
    }
}

async fn make_manager() -> (ConfigRuntimeManager, Arc<dyn ConfigStore>) {
    let store = Arc::new(InMemoryStore::new()) as Arc<dyn ConfigStore>;
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<AgentRuntime> = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("boot", Arc::new(StubExec))
            .with_model_binding(
                "boot",
                ModelBinding {
                    provider_id: "boot".into(),
                    upstream_model: "boot-model".into(),
                },
            )
            .with_agent_spec(AgentSpec {
                id: "boot".into(),
                model_id: "boot".into(),
                system_prompt: "boot".into(),
                max_rounds: 1,
                ..Default::default()
            })
            .with_thread_run_store(thread_store)
            .build()
            .expect("build runtime"),
    );
    let manager = ConfigRuntimeManager::new(runtime, store.clone()).expect("manager");
    // Seed a minimal working set so apply() reaches validate_candidate.
    let seed = BuiltinSeedSet {
        binary_version: "v1".into(),
        specs: vec![
            BuiltinSpec::Provider(ProviderSpec {
                id: "p".into(),
                adapter: "openai".into(),
                ..Default::default()
            }),
            BuiltinSpec::Model(ModelBindingSpec {
                id: "m".into(),
                provider_id: "p".into(),
                upstream_model: "gpt-4o".into(),
            }),
        ],
    };
    manager.apply_seed(&seed).await.expect("seed");
    (manager, store)
}

#[tokio::test]
async fn runtime_apply_rejects_invalid_catalog_pattern_from_store() {
    let (manager, store) = make_manager().await;

    // Direct-store write of a bare AgentSpec with an unparseable pattern
    // (trailing backslash). Bypasses the write-path validator entirely.
    let bad_agent = json!({
        "id": "bad",
        "model_id": "m",
        "system_prompt": "p",
        "allowed_tool_patterns": ["foo\\"],
    });
    store
        .put("agents", "bad", &bad_agent)
        .await
        .expect("direct put");

    let err = manager
        .apply()
        .await
        .expect_err("apply must reject invalid catalog pattern");
    let msg = err.to_string();
    assert!(
        msg.contains("bad:") && msg.contains("allowed_tool_patterns") && msg.contains("foo\\"),
        "error must name the agent + field + offending entry: {msg}"
    );
}
