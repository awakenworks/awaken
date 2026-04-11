#![allow(missing_docs)]

use awaken::contract::lifecycle::TerminationReason;
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, RunRequest};
use std::sync::Arc;
use std::time::Duration;

fn live_executor() -> Option<(String, GenaiExecutor)> {
    if let (Ok(mut base_url), Ok(api_key), Ok(model)) = (
        std::env::var("LLM_BASE_URL"),
        std::env::var("LLM_API_KEY"),
        std::env::var("LLM_MODEL"),
    ) {
        use genai::adapter::AdapterKind;
        use genai::resolver::{AuthData, Endpoint};
        use genai::{ModelIden, ServiceTarget};

        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let adapter = match std::env::var("LLM_ADAPTER").as_deref() {
            Ok("anthropic") => AdapterKind::Anthropic,
            _ => AdapterKind::OpenAI,
        };
        let client = genai::Client::builder()
            .with_service_target_resolver_fn(move |st: ServiceTarget| {
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(base_url.clone()),
                    auth: AuthData::from_single(api_key.clone()),
                    model: ModelIden::new(adapter, st.model.model_name),
                })
            })
            .build();
        return Some((model, GenaiExecutor::with_client(client)));
    }

    if std::env::var("OPENAI_API_KEY").is_ok() {
        let model = std::env::var("LLM_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());
        return Some((model, GenaiExecutor::new()));
    }

    None
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY or LLM_BASE_URL + LLM_API_KEY + LLM_MODEL"]
async fn readme_live_provider_smoke_test() {
    let (model, executor) =
        live_executor().expect("set OPENAI_API_KEY, or set LLM_BASE_URL + LLM_API_KEY + LLM_MODEL");

    let agent_spec = AgentSpec::new("assistant")
        .with_model_id("live-model")
        .with_system_prompt("You are a concise test assistant.")
        .with_max_rounds(2);

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_provider("live-provider", Arc::new(executor))
        .with_model_binding(
            "live-model",
            ModelBinding {
                provider_id: "live-provider".into(),
                upstream_model: model,
            },
        )
        .build()
        .expect("live runtime should build");

    let request = RunRequest::new(
        "thread-live-provider",
        vec![Message::user("Reply with one short sentence.")],
    )
    .with_agent_id("assistant");

    let result = tokio::time::timeout(Duration::from_secs(90), runtime.run_to_completion(request))
        .await
        .expect("live provider request timed out")
        .expect("live provider run should succeed");

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert!(
        !result.response.trim().is_empty(),
        "live provider returned an empty response"
    );
}
