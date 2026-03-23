//! Multi-turn conversation using AgentRuntime with persistent thread storage.
//!
//! Run: cargo run --example multi_turn
//!
//! Requires: OPENAI_API_KEY (or other provider key) + model env var

use async_trait::async_trait;
use awaken::agent::config::AgentConfig;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::loop_runner::build_agent_env;
use awaken::stores::InMemoryStore;
use awaken::*;
use std::sync::Arc;

struct ConsoleSink;

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::TextDelta { delta } => print!("{delta}"),
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
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let llm = Arc::new(GenaiExecutor::new());

    let agent = AgentConfig::new(
        "chat",
        &model,
        "You are a helpful assistant. Be concise. Remember what the user tells you.",
        llm,
    );
    let resolver = Arc::new(SimpleResolver {
        agent: agent.clone(),
    });

    let store = Arc::new(InMemoryStore::new());
    let runtime = AgentRuntime::new(resolver).with_thread_run_store(store);

    let thread_id = "conversation-1";

    let turns = vec![
        "My name is Alice. Remember it.",
        "What is the capital of France?",
        "What is my name?",
    ];

    for (i, user_msg) in turns.iter().enumerate() {
        print!(
            "\n[Turn {}] User: {}\n[Turn {}] Assistant: ",
            i + 1,
            user_msg,
            i + 1
        );

        let request =
            RunRequest::new(thread_id, vec![Message::user(*user_msg)]).with_agent_id("chat");

        match runtime.run(request, Arc::new(ConsoleSink)).await {
            Ok(result) => {
                print!(" [{:?}]", result.termination);
            }
            Err(e) => {
                print!(" [ERROR: {e}]");
            }
        }
    }

    print!("\n\n=== Multi-turn test complete ===\n");
}
