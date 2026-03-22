//! Live integration test with a real LLM provider.
//!
//! Run: cargo run --example live_test
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

// ---------------------------------------------------------------------------
// Real OpenAI-compatible LLM executor
// ---------------------------------------------------------------------------

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
                let mut msg = serde_json::json!({
                    "role": role,
                    "content": m.text(),
                });
                if let Some(ref tc_id) = m.tool_call_id {
                    msg["tool_call_id"] = Value::String(tc_id.clone());
                }
                msg
            })
            .collect();

        let mut body = serde_json::json!({
            "model": if request.model.is_empty() { &self.model } else { &request.model },
            "messages": messages,
        });

        if let Some(ref ovr) = request.overrides {
            if let Some(temp) = ovr.temperature {
                body["temperature"] = Value::from(temp);
            }
            if let Some(max) = ovr.max_tokens {
                body["max_tokens"] = Value::from(max);
            }
        }

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        if !status.is_success() {
            return Err(InferenceExecutionError::Provider(format!(
                "HTTP {status}: {text}"
            )));
        }

        let json: Value = serde_json::from_str(&text)
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        let choice = &json["choices"][0];
        let content = choice["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let stop = match choice["finish_reason"].as_str() {
            Some("stop") => Some(StopReason::EndTurn),
            Some("length") => Some(StopReason::MaxTokens),
            Some("tool_calls") => Some(StopReason::ToolUse),
            _ => None,
        };

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
            stop_reason: stop,
        })
    }

    fn name(&self) -> &str {
        "openai"
    }
}

// ---------------------------------------------------------------------------
// Console event sink
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Simple resolver
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("=== awaken live integration test ===\n");

    let executor = OpenAIExecutor::new();
    println!("Provider: {} ({})", executor.name(), executor.base_url);
    println!("Model: {}\n", executor.model);
    let model_name = executor.model.clone();
    let llm = Arc::new(executor);

    let agent = AgentConfig::new(
        "live-test",
        &model_name,
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

    let result = run_agent_loop(
        &resolver,
        "live-test",
        &runtime,
        &ConsoleSink,
        None,
        vec![Message::user("What is 2+2? Answer in one word.")],
        identity,
        None,
    )
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
