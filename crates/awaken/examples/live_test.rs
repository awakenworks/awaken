//! Live integration test with a real LLM provider via GenaiExecutor.
//!
//! Run: cargo run --example live_test
//!
//! Requires: OPENAI_API_KEY (or ANTHROPIC_API_KEY, etc.) + model env var

use async_trait::async_trait;
use awaken::agent::config::AgentConfig;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::loop_runner::{AgentLoopParams, LoopStatePlugin, build_agent_env, run_agent_loop};
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
    agent: AgentConfig,
}

impl AgentResolver for SimpleResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, awaken::RuntimeError> {
        let env = build_agent_env(&[], &self.agent)?;
        Ok(ResolvedAgent {
            config: self.agent.clone(),
            env,
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("=== awaken live integration test ===\n");

    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    println!("Model: {model}\n");

    let llm = Arc::new(GenaiExecutor::new());
    let agent = AgentConfig::new(
        "live-test",
        &model,
        "You are a helpful assistant. Be concise.",
        llm,
    );
    let resolver = SimpleResolver {
        agent: agent.clone(),
    };

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();

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
