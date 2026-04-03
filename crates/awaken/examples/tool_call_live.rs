//! Live tool call test with a real LLM provider via GenaiExecutor.
//!
//! Run with BigModel GLM:
//!   LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4/ LLM_API_KEY=<key> LLM_MODEL=GLM-4.7-Flash cargo run --example tool_call_live
//!
//! Also supports: OPENAI_API_KEY + OPENAI_MODEL, or ANTHROPIC_API_KEY, etc.

use async_trait::async_trait;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::message::Message;
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::engine::GenaiExecutor;
use awaken::loop_runner::{AgentLoopParams, LoopStatePlugin, build_agent_env, run_agent_loop};
use awaken::registry::ResolvedAgent;
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
            metadata: Default::default(),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let expr = args["expression"].as_str().unwrap_or("0");
        let result = eval_simple(expr);
        Ok(ToolResult::success_with_message(
            "calculator",
            json!({ "result": result }),
            result.to_string(),
        )
        .into())
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
    let model = std::env::var("LLM_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into());
    let llm = Arc::new(build_llm_executor());

    // Attach the tool so the loop runner advertises it to the LLM and dispatches calls.
    let agent = ResolvedAgent::new(
        "calc-agent",
        &model,
        "You are a helpful math assistant. Use the calculator tool for any calculation.",
        llm,
    )
    .with_tool(Arc::new(CalculatorTool));

    let resolver = SimpleResolver {
        agent: agent.clone(),
    };

    // StateStore holds per-run plugin state; LoopStatePlugin tracks loop progress.
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

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "calc-agent",
        runtime: &runtime,
        sink: Arc::new(ConsoleSink),
        checkpoint_store: None,
        messages: vec![Message::user("What is 137 * 42? Use the calculator tool.")],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
    })
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
