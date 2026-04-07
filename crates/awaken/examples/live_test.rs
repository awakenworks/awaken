//! Live integration test with a real LLM provider via GenaiExecutor.
//!
//! Run with BigModel GLM (OpenAI adapter):
//!   LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4/ LLM_API_KEY=<key> LLM_MODEL=GLM-4.7-Flash cargo run --example live_test
//!
//! Run with BigModel Claude-compatible (Anthropic adapter):
//!   LLM_BASE_URL=https://open.bigmodel.cn/api/anthropic/v1/ LLM_API_KEY=<key> LLM_ADAPTER=anthropic LLM_MODEL=GLM-4.7 cargo run --example live_test
//!
//! Also supports native provider env vars: OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.

use async_trait::async_trait;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::loop_runner::{AgentLoopParams, LoopStatePlugin, build_agent_env, run_agent_loop};
use awaken::registry::ResolvedAgent;
use awaken::*;
use std::sync::Arc;

struct ConsoleSink;

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::RunStart { run_id, .. } => println!("🚀 Run started: {run_id}"),
            AgentEvent::StepStart { .. } => println!("📍 Step started"),
            AgentEvent::TextDelta { delta } => print!("{delta}"),
            AgentEvent::InferenceComplete {
                model,
                usage,
                duration_ms,
            } => {
                let tokens = usage.as_ref().and_then(|u| u.total_tokens).unwrap_or(0);
                println!("\n⚡ Inference: {model} | {tokens} tokens | {duration_ms}ms");
            }
            AgentEvent::StepEnd => println!("✅ Step complete"),
            AgentEvent::RunFinish { termination, .. } => {
                println!("🏁 Run finished: {termination:?}")
            }
            _ => {}
        }
    }
}

struct SimpleResolver {
    agent: ResolvedAgent,
}

impl AgentResolver for SimpleResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, awaken::RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&[], &agent)?;
        Ok(agent)
    }
}

fn build_llm_executor() -> GenaiExecutor {
    if let (Ok(mut base_url), Ok(api_key)) =
        (std::env::var("LLM_BASE_URL"), std::env::var("LLM_API_KEY"))
    {
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
        GenaiExecutor::with_client(client)
    } else {
        GenaiExecutor::new()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("=== awaken live integration test ===\n");

    let model = std::env::var("LLM_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into());
    println!("Model: {model}\n");

    // Wrap in Arc so the executor can be shared across async tasks.
    let llm = Arc::new(build_llm_executor());
    let agent = ResolvedAgent::new(
        "live-test",
        &model,
        "You are a helpful assistant. Be concise.",
        llm,
    );
    // SimpleResolver always returns the same agent, regardless of agent_id.
    let resolver = SimpleResolver {
        agent: agent.clone(),
    };

    // StateStore holds per-run plugin state; LoopStatePlugin tracks loop progress.
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();

    // RunIdentity ties thread, run, and agent IDs together for event correlation.
    let identity = RunIdentity::new(
        "thread-live".into(),
        None,
        "run-live".into(),
        None,
        "live-test".into(),
        RunOrigin::User,
    );

    println!("--- Sending: 'What is 2+2? Answer in one word.' ---\n");

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "live-test",
        runtime: &runtime,
        sink: Arc::new(ConsoleSink),
        checkpoint_store: None,
        messages: vec![Message::user("What is 2+2? Answer in one word.")],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await;

    match result {
        Ok(r) => {
            println!("\n--- Result ---");
            println!("Response: {}", r.response);
            println!("Termination: {:?}", r.termination);
            println!("Steps: {}", r.steps);
        }
        Err(e) => {
            eprintln!("\n--- Error ---");
            eprintln!("{e}");
        }
    }

    println!("\n=== test complete ===");
}
