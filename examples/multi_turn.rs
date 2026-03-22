//! Multi-turn conversation test with a real LLM provider.
//!
//! Run: cargo run --example multi_turn
//!
//! Requires: OPENAI_API_KEY, OPENAI_BASE_URL, OPENAI_MODEL env vars

use async_trait::async_trait;
use awaken::agent::config::AgentConfig;
use awaken::agent::loop_runner::{LoopStatePlugin, build_agent_env, run_agent_loop};
use awaken::contract::content::ContentBlock;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken::contract::message::Message;
use awaken::*;
use serde_json::Value;
use std::sync::Arc;

// Reuse the OpenAI executor from live_test
struct OpenAIExecutor {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAIExecutor {
    fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            api_key: std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY required"),
            model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
        }
    }
}

#[async_trait]
impl LlmExecutor for OpenAIExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    awaken::contract::message::Role::System => "system",
                    awaken::contract::message::Role::User => "user",
                    awaken::contract::message::Role::Assistant => "assistant",
                    awaken::contract::message::Role::Tool => "tool",
                };
                serde_json::json!({ "role": role, "content": m.text() })
            })
            .collect();

        let body = serde_json::json!({
            "model": if request.model.is_empty() { &self.model } else { &request.model },
            "messages": messages,
            "max_tokens": 100,
        });

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        let text = resp
            .text()
            .await
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        let json: Value = serde_json::from_str(&text)
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let usage = json.get("usage").map(|u| TokenUsage {
            prompt_tokens: u["prompt_tokens"].as_i64().map(|v| v as i32),
            completion_tokens: u["completion_tokens"].as_i64().map(|v| v as i32),
            total_tokens: u["total_tokens"].as_i64().map(|v| v as i32),
            ..Default::default()
        });

        Ok(StreamResult {
            content: if content.is_empty() {
                vec![]
            } else {
                vec![ContentBlock::text(content)]
            },
            tool_calls: vec![],
            usage,
            stop_reason: Some(StopReason::EndTurn),
        })
    }

    fn name(&self) -> &str {
        "openai"
    }
}

struct ConsoleSink;

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::TextDelta { delta } => print!("{delta}"),
            AgentEvent::InferenceComplete { usage, .. } => {
                let tokens = usage.as_ref().and_then(|u| u.total_tokens).unwrap_or(0);
                print!(" [{tokens} tokens]");
            }
            _ => {}
        }
    }
}

struct SimpleResolver {
    agent: AgentConfig,
}

impl AgentResolver for SimpleResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, awaken::StateError> {
        let env = build_agent_env(&[], &self.agent)?;
        Ok(ResolvedAgent {
            config: self.agent.clone(),
            env,
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let executor = OpenAIExecutor::new();
    let model_name = executor.model.clone();
    let llm = Arc::new(executor);

    let agent = AgentConfig::new(
        "chat",
        &model_name,
        "You are a helpful assistant. Be concise. Remember what the user tells you.",
        llm,
    );
    let resolver = SimpleResolver {
        agent: agent.clone(),
    };

    // Simulate multi-turn: collect messages across runs
    let mut all_messages: Vec<Message> = Vec::new();

    let turns = vec![
        "My name is Alice. Remember it.",
        "What is the capital of France?",
        "What is my name?", // Should remember "Alice" from turn 1
    ];

    for (i, user_msg) in turns.iter().enumerate() {
        // Add new user message
        all_messages.push(Message::user(*user_msg));

        // Create fresh runtime for each run (simulating separate HTTP requests)
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store.clone()).unwrap();
        store.install_plugin(LoopStatePlugin).unwrap();

        let identity = RunIdentity::new(
            "thread-multi".into(),
            None,
            format!("run-{i}"),
            None,
            "chat".into(),
            RunOrigin::User,
        );

        print!(
            "\n[Turn {}] User: {}\n[Turn {}] Assistant: ",
            i + 1,
            user_msg,
            i + 1
        );

        let result = run_agent_loop(
            &resolver,
            "chat",
            &runtime,
            &ConsoleSink,
            None,
            all_messages.clone(),
            identity,
            None,
        )
        .await
        .unwrap();

        // Add assistant response to history for next turn
        all_messages.push(Message::assistant(&result.response));
        println!();
    }

    println!("\n=== Multi-turn test complete ===");
    println!("Total messages in history: {}", all_messages.len());
}
