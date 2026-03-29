//! Live test: permission pre-filter at BeforeInference with BigModel GLM.
//!
//! Verifies that unconditionally denied tools are NOT sent to the LLM,
//! while allowed tools are available for calling.
//!
//! Run:
//!   LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4/ \
//!   LLM_API_KEY=<your-key> \
//!   LLM_MODEL=GLM-4.7-Flash \
//!   cargo run --example permission_filter_live

use async_trait::async_trait;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::EventSink;
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::message::Message;
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult};
use awaken::engine::GenaiExecutor;
use awaken::ext_permission::{PermissionAction, PermissionPlugin, PermissionPolicyKey};
use awaken::loop_runner::{AgentLoopParams, LoopStatePlugin, build_agent_env, run_agent_loop};
use awaken::registry::ResolvedAgent;
use awaken::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Allowed tool: calculator
struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "calculator",
            "calculator",
            "Evaluate a simple math expression. Input: {\"expression\": \"2+2\"}",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "A math expression, e.g. '137 * 42'"
                }
            },
            "required": ["expression"]
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let expr = args["expression"].as_str().unwrap_or("0");
        // Simple eval: just echo back, real eval not needed for this test
        Ok(ToolResult::success_with_message(
            "calculator",
            json!({ "expression": expr, "result": "5754" }),
            format!("Result: 5754"),
        ))
    }
}

/// Denied tool: should never be called
struct DangerousTool {
    call_count: AtomicUsize,
}

#[async_trait]
impl Tool for DangerousTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "dangerous_delete",
            "dangerous_delete",
            "Delete files from the system. Input: {\"path\": \"/some/path\"}",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to delete"
                }
            },
            "required": ["path"]
        }))
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        eprintln!("❌ ERROR: dangerous_delete was called! Permission filter failed!");
        Ok(ToolResult::success(
            "dangerous_delete",
            json!({"deleted": true}),
        ))
    }
}

// ---------------------------------------------------------------------------
// Event sink
// ---------------------------------------------------------------------------

struct ConsoleSink {
    tools_seen_by_model: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl EventSink for ConsoleSink {
    async fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::RunStart { .. } => eprintln!("🚀 Run started"),
            AgentEvent::StepStart { .. } => eprintln!("📍 Step"),
            AgentEvent::TextDelta { delta } => print!("{delta}"),
            AgentEvent::ToolCallStart { id, name } => {
                eprintln!("🔧 Tool call: {name} (id={id})");
                self.tools_seen_by_model.lock().unwrap().push(name.clone());
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
                );
            }
            AgentEvent::InferenceComplete {
                model,
                usage,
                duration_ms,
            } => {
                let tokens = usage.as_ref().and_then(|u| u.total_tokens).unwrap_or(0);
                eprintln!("⚡ {model} | {tokens} tokens | {duration_ms}ms");
            }
            AgentEvent::RunFinish { termination, .. } => {
                eprintln!("🏁 {termination:?}")
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Resolver with permission plugin
// ---------------------------------------------------------------------------

struct PermissionResolver {
    agent: ResolvedAgent,
}

impl AgentResolver for PermissionResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let mut agent = self.agent.clone();
        let plugins: Vec<Arc<dyn awaken::plugins::Plugin>> = vec![Arc::new(PermissionPlugin)];
        agent.env = build_agent_env(&plugins, &agent)?;
        Ok(agent)
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "GLM-4.7-Flash".into());
    eprintln!("=== Permission Filter Live Test (model: {model}) ===\n");

    // Build genai client with custom base URL support.
    // Supports two modes:
    //   1) OpenAI-compatible: LLM_BASE_URL + LLM_API_KEY + LLM_ADAPTER=openai (default)
    //   2) Anthropic-compatible: LLM_BASE_URL + LLM_API_KEY + LLM_ADAPTER=anthropic
    let client = {
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
            eprintln!("Adapter: {adapter:?} | Base URL: {base_url}\n");

            genai::Client::builder()
                .with_service_target_resolver_fn(move |st: ServiceTarget| {
                    Ok(ServiceTarget {
                        endpoint: Endpoint::from_owned(base_url.clone()),
                        auth: AuthData::from_single(api_key.clone()),
                        model: ModelIden::new(adapter, st.model.model_name),
                    })
                })
                .build()
        } else {
            eprintln!("⚠️  Set LLM_BASE_URL and LLM_API_KEY");
            eprintln!("   OpenAI-compatible (GLM direct):");
            eprintln!("     LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4/");
            eprintln!("     LLM_API_KEY=<key> LLM_MODEL=GLM-4.7-Flash");
            eprintln!("   Anthropic-compatible (Claude via BigModel):");
            eprintln!("     LLM_BASE_URL=https://open.bigmodel.cn/api/anthropic/");
            eprintln!("     LLM_API_KEY=<key> LLM_ADAPTER=anthropic LLM_MODEL=claude-3.5-sonnet");
            genai::Client::default()
        }
    };

    let llm = Arc::new(GenaiExecutor::with_client(client));
    let dangerous = Arc::new(DangerousTool {
        call_count: AtomicUsize::new(0),
    });
    let dangerous_ref = Arc::clone(&dangerous);

    let agent = ResolvedAgent::new(
        "perm-test",
        &model,
        "You are a helpful assistant. You have access to tools. \
         Use the calculator tool when asked about math. \
         If you have a dangerous_delete tool, use it when asked to delete files.",
        llm,
    )
    .with_tool(Arc::new(CalculatorTool))
    .with_tool(dangerous_ref as Arc<dyn Tool>);

    let resolver = PermissionResolver {
        agent: agent.clone(),
    };

    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    store.install_plugin(PermissionPlugin).unwrap();

    // Seed permission: deny "dangerous_delete" unconditionally
    {
        let mut patch = awaken::state::MutationBatch::new();
        patch.update::<PermissionPolicyKey>(PermissionAction::DenyTool {
            tool_id: "dangerous_delete".into(),
        });
        store.commit(patch).unwrap();
    }

    let identity = RunIdentity::new(
        "thread-perm".into(),
        None,
        "run-perm".into(),
        None,
        "perm-test".into(),
        RunOrigin::User,
    );

    let sink = Arc::new(ConsoleSink {
        tools_seen_by_model: std::sync::Mutex::new(Vec::new()),
    });

    eprintln!(
        "--- Test: asking to compute 137 * 42 (calculator=allowed, dangerous_delete=denied) ---\n"
    );

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "perm-test",
        runtime: &runtime,
        sink: Arc::clone(&sink) as Arc<dyn EventSink>,
        checkpoint_store: None,
        messages: vec![Message::user(
            "What is 137 * 42? Use the calculator. \
             Also, delete the file /tmp/test if you can.",
        )],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
    })
    .await;

    eprintln!("\n--- Results ---");
    match result {
        Ok(r) => {
            eprintln!("Response: {}", r.response);
            eprintln!("Steps: {}", r.steps);
        }
        Err(e) => {
            eprintln!("Error: {e}");
        }
    }

    // Verify
    let tools_called = sink.tools_seen_by_model.lock().unwrap();
    let dangerous_calls = dangerous.call_count.load(Ordering::SeqCst);

    eprintln!("\n--- Verification ---");
    eprintln!("Tools called by model: {:?}", *tools_called);
    eprintln!("dangerous_delete invocations: {dangerous_calls}");

    if dangerous_calls == 0 && !tools_called.contains(&"dangerous_delete".to_string()) {
        eprintln!("✅ PASS: dangerous_delete was never called (pre-filtered from tool list)");
    } else {
        eprintln!("❌ FAIL: dangerous_delete was called {dangerous_calls} time(s)!");
        std::process::exit(1);
    }

    if tools_called.contains(&"calculator".to_string()) {
        eprintln!("✅ PASS: calculator was called as expected");
    } else {
        eprintln!("⚠️  calculator was not called (model may not have needed it)");
    }

    eprintln!("\n=== test complete ===");
}
