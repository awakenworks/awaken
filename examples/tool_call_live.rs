//! Live tool call test with a real LLM provider.
//!
//! Demonstrates: LLM decides to call a tool, framework executes it,
//! result fed back, LLM produces final answer.
//!
//! Run: cargo run --example tool_call_live
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
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
use awaken::*;
use serde_json::{Value, json};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// OpenAI executor with tool call support
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
                let mut msg = json!({ "role": role, "content": m.text() });
                if let Some(ref tc_id) = m.tool_call_id {
                    msg["tool_call_id"] = Value::String(tc_id.clone());
                }
                if let Some(ref calls) = m.tool_calls {
                    let tool_calls: Vec<Value> = calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default()
                                }
                            })
                        })
                        .collect();
                    msg["tool_calls"] = Value::Array(tool_calls);
                    // OpenAI requires content to be null when tool_calls present
                    if m.text().is_empty() {
                        msg["content"] = Value::Null;
                    }
                }
                msg
            })
            .collect();

        // Convert tool descriptors to OpenAI function format
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters
                    }
                })
            })
            .collect();

        let mut body = json!({
            "model": if request.model.is_empty() { &self.model } else { &request.model },
            "messages": messages,
            "max_tokens": 500,
        });

        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
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

        let json_resp: Value = serde_json::from_str(&text)
            .map_err(|e| InferenceExecutionError::Provider(e.to_string()))?;

        let choice = &json_resp["choices"][0];
        let message = &choice["message"];

        let content = message["content"].as_str().unwrap_or("").to_string();
        let finish_reason = choice["finish_reason"].as_str().unwrap_or("");

        // Parse tool calls from response
        let mut tool_calls = Vec::new();
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let id = call["id"].as_str().unwrap_or("").to_string();
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                let arguments: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                tool_calls.push(ToolCall::new(id, name, arguments));
            }
        }

        let stop = match finish_reason {
            "stop" => Some(StopReason::EndTurn),
            "length" => Some(StopReason::MaxTokens),
            "tool_calls" => Some(StopReason::ToolUse),
            _ => None,
        };

        let usage = json_resp.get("usage").map(|u| TokenUsage {
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
            tool_calls,
            usage,
            stop_reason: stop,
        })
    }

    fn name(&self) -> &str {
        "openai"
    }
}

// ---------------------------------------------------------------------------
// Calculator tool
// ---------------------------------------------------------------------------

struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "calculator".into(),
            name: "calculator".into(),
            description: "Evaluate a simple math expression. Input: {\"expression\": \"2+2\"}"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "A math expression to evaluate, e.g. '2+2' or '10*5'"
                    }
                },
                "required": ["expression"]
            }),
            category: None,
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let expr = args["expression"].as_str().unwrap_or("0");
        // Simple eval: just handle basic operations
        let result = eval_simple(expr);
        Ok(ToolResult::success_with_message(
            "calculator",
            json!({ "result": result }),
            result.to_string(),
        ))
    }
}

fn eval_simple(expr: &str) -> f64 {
    // Very basic: parse "a op b" patterns
    let expr = expr.trim();
    if let Some(pos) = expr.rfind('+') {
        if pos > 0 {
            let a: f64 = expr[..pos].trim().parse().unwrap_or(0.0);
            let b: f64 = expr[pos + 1..].trim().parse().unwrap_or(0.0);
            return a + b;
        }
    }
    if let Some(pos) = expr.rfind('-') {
        if pos > 0 {
            let a: f64 = expr[..pos].trim().parse().unwrap_or(0.0);
            let b: f64 = expr[pos + 1..].trim().parse().unwrap_or(0.0);
            return a - b;
        }
    }
    if let Some(pos) = expr.rfind('*') {
        let a: f64 = expr[..pos].trim().parse().unwrap_or(0.0);
        let b: f64 = expr[pos + 1..].trim().parse().unwrap_or(0.0);
        return a * b;
    }
    if let Some(pos) = expr.rfind('/') {
        let a: f64 = expr[..pos].trim().parse().unwrap_or(0.0);
        let b: f64 = expr[pos + 1..].trim().parse().unwrap_or(1.0);
        return a / b;
    }
    expr.parse().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Console sink
// ---------------------------------------------------------------------------

struct ConsoleSink;

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::RunStart { .. } => eprintln!("🚀 Run started"),
            AgentEvent::StepStart { .. } => eprintln!("📍 Step"),
            AgentEvent::TextDelta { delta } => print!("{delta}"),
            AgentEvent::ToolCallStart { id, name } => {
                eprintln!("🔧 Tool call: {name} (id={id})")
            }
            AgentEvent::ToolCallDone {
                id,
                result,
                outcome,
                ..
            } => {
                eprintln!(
                    "✅ Tool result: {id} → {:?} ({})",
                    outcome,
                    result.message.as_deref().unwrap_or("no message")
                )
            }
            AgentEvent::InferenceComplete {
                usage, duration_ms, ..
            } => {
                let tokens = usage.as_ref().and_then(|u| u.total_tokens).unwrap_or(0);
                eprintln!("⚡ {tokens} tokens, {duration_ms}ms");
            }
            AgentEvent::RunFinish { termination, .. } => {
                eprintln!("🏁 {termination:?}")
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Resolver
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
    let executor = OpenAIExecutor::new();
    let model_name = executor.model.clone();
    let llm = Arc::new(executor);

    let agent = AgentConfig::new(
        "calc-agent",
        &model_name,
        "You are a helpful math assistant. Use the calculator tool for any calculation. Show the result clearly.",
        llm,
    )
    .with_tool(Arc::new(CalculatorTool));

    let resolver = SimpleResolver {
        agent: agent.clone(),
    };

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();

    let identity = RunIdentity::new(
        "thread-calc".into(),
        None,
        "run-calc".into(),
        None,
        "calc-agent".into(),
        RunOrigin::User,
    );

    eprintln!("=== Tool Call Live Test ===\n");
    eprintln!("Asking: 'What is 137 * 42?'\n");

    let result = run_agent_loop(
        &resolver,
        "calc-agent",
        &runtime,
        &ConsoleSink,
        None,
        vec![Message::user("What is 137 * 42? Use the calculator tool.")],
        identity,
        None,
    )
    .await;

    match result {
        Ok(r) => {
            eprintln!("\n--- Response: {} ---", r.response);
            eprintln!("Steps: {}", r.steps);
        }
        Err(e) => {
            eprintln!("\n--- Error: {e} ---");
        }
    }
}
