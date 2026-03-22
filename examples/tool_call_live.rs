//! Live tool call test with a real LLM provider via GenaiExecutor.
//!
//! Run: cargo run --example tool_call_live
//!
//! Requires: OPENAI_API_KEY (or other provider key) + model env var

use async_trait::async_trait;
use awaken::agent::config::AgentConfig;
use awaken::agent::loop_runner::{LoopStatePlugin, build_agent_env, run_agent_loop};
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::message::Message;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
use awaken::engine::GenaiExecutor;
use awaken::*;
use serde_json::{Value, json};
use std::sync::Arc;

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
        let result = eval_simple(expr);
        Ok(ToolResult::success_with_message(
            "calculator",
            json!({ "result": result }),
            result.to_string(),
        ))
    }
}

fn eval_simple(expr: &str) -> f64 {
    let expr = expr.trim();
    if let Some(pos) = expr.rfind('+') {
        if pos > 0 {
            let a: f64 = expr[..pos].trim().parse().unwrap_or(0.0);
            let b: f64 = expr[pos + 1..].trim().parse().unwrap_or(0.0);
            return a + b;
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
    if let Some(pos) = expr.rfind('-') {
        if pos > 0 {
            let a: f64 = expr[..pos].trim().parse().unwrap_or(0.0);
            let b: f64 = expr[pos + 1..].trim().parse().unwrap_or(0.0);
            return a - b;
        }
    }
    expr.parse().unwrap_or(0.0)
}

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
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let llm = Arc::new(GenaiExecutor::new());

    let agent = AgentConfig::new(
        "calc-agent",
        &model,
        "You are a helpful math assistant. Use the calculator tool for any calculation.",
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

    eprintln!("=== Tool Call Live Test (model: {model}) ===\n");
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
