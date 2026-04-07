//! Generative UI example: parent agent delegates UI rendering to a streaming sub-agent.
//!
//! Demonstrates the full pipeline:
//! 1. Parent agent receives "create a dashboard" and returns a tool call to `render_ui`
//! 2. `render_ui` tool calls `run_streaming_subagent()` which spins up a sub-agent
//! 3. Sub-agent returns OpenUI Lang text, streamed as ToolCallStreamDelta events
//! 4. Parent agent gets a second inference (no tools) and returns final text

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::{EventSink, VecEventSink};
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_ext_generative_ui::{openui, run_streaming_subagent};
use awaken_runtime::loop_runner::{AgentLoopParams, build_agent_env, run_agent_loop};
use awaken_runtime::plugins::Plugin;
use awaken_runtime::{AgentResolver, PhaseRuntime, ResolvedAgent, RuntimeError, StateStore};

// ---------------------------------------------------------------------------
// Scripted LLM — returns canned responses in sequence
// ---------------------------------------------------------------------------

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(StreamResult {
                content: vec![ContentBlock::text("(no more scripted responses)")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        } else {
            Ok(responses.remove(0))
        }
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

// ---------------------------------------------------------------------------
// RenderUITool — calls run_streaming_subagent
// ---------------------------------------------------------------------------

struct RenderUITool {
    resolver: Arc<dyn AgentResolver>,
}

#[async_trait]
impl Tool for RenderUITool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "render_ui",
            "render_ui",
            "Render a UI component via a sub-agent",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Description of the UI to render"
                }
            },
            "required": ["prompt"]
        }))
    }

    fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
        if args.get("prompt").and_then(Value::as_str).is_none() {
            return Err(ToolError::InvalidArguments(
                "missing required field \"prompt\"".into(),
            ));
        }
        Ok(())
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let result =
            run_streaming_subagent(self.resolver.as_ref(), "ui-agent", prompt, ctx).await?;

        Ok(ToolResult::success(
            "render_ui",
            json!({
                "ui_content": result.content,
                "steps": result.steps,
            }),
        )
        .into())
    }
}

// ---------------------------------------------------------------------------
// Multi-agent resolver
// ---------------------------------------------------------------------------

struct MultiAgentResolver {
    agents: Vec<ResolvedAgent>,
}

impl AgentResolver for MultiAgentResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let mut agent = self
            .agents
            .iter()
            .find(|a| a.id() == agent_id)
            .ok_or_else(|| RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })?
            .clone();
        agent.env = build_agent_env(&[], &agent)?;
        Ok(agent)
    }
}

// ---------------------------------------------------------------------------
// State plugin (required by the agent loop)
// ---------------------------------------------------------------------------

struct LoopStatePlugin;

impl Plugin for LoopStatePlugin {
    fn descriptor(&self) -> awaken_runtime::PluginDescriptor {
        awaken_runtime::PluginDescriptor { name: "loop-state" }
    }

    fn register(
        &self,
        registrar: &mut awaken_runtime::PluginRegistrar,
    ) -> Result<(), awaken_contract::StateError> {
        use awaken_runtime::agent::state::{
            ContextMessageStore, ContextThrottleState, RunLifecycle, ToolCallStates,
        };
        registrar.register_key::<RunLifecycle>(awaken_contract::StateKeyOptions::default())?;
        registrar.register_key::<ToolCallStates>(awaken_contract::StateKeyOptions::default())?;
        registrar
            .register_key::<ContextThrottleState>(awaken_contract::StateKeyOptions::default())?;
        registrar
            .register_key::<ContextMessageStore>(awaken_contract::StateKeyOptions::default())?;
        Ok(())
    }
}

fn make_runtime() -> PhaseRuntime {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    runtime
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    // -- Sub-agent LLM: returns OpenUI Lang text --
    let sub_agent_llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text(
            "root = Card(\"Revenue\")\nmetric = Metric(\"$1.2M\")",
        )],
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: Some(50),
            completion_tokens: Some(30),
            total_tokens: Some(80),
            ..Default::default()
        }),
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));

    // -- Parent agent LLM: first call returns tool call, second returns text --
    let parent_llm = Arc::new(ScriptedLlm::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("I'll create a dashboard UI for you.")],
            tool_calls: vec![ToolCall::new(
                "call-1",
                "render_ui",
                json!({"prompt": "revenue dashboard"}),
            )],
            usage: Some(TokenUsage {
                prompt_tokens: Some(20),
                completion_tokens: Some(15),
                total_tokens: Some(35),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text(
                "Here is your revenue dashboard with a Card and Metric component.",
            )],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(40),
                completion_tokens: Some(20),
                total_tokens: Some(60),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    // -- Build agent configs --
    let sub_agent = ResolvedAgent::new(
        "ui-agent",
        "mock",
        openui::system_prompt("Card, Metric, Chart, Table"),
        sub_agent_llm,
    );

    // Build the resolver first (without the parent tool), then wrap in Arc
    // so we can share it with the RenderUITool.
    let resolver = Arc::new(MultiAgentResolver {
        agents: vec![sub_agent],
    });

    let render_tool: Arc<dyn Tool> = Arc::new(RenderUITool {
        resolver: resolver.clone(),
    });

    let parent_agent = ResolvedAgent::new(
        "parent",
        "mock",
        "You are a dashboard assistant. Use the render_ui tool to create UI components.",
        parent_llm,
    )
    .with_tool(render_tool);

    // Now build the final resolver with both agents.
    let full_resolver = Arc::new(MultiAgentResolver {
        agents: vec![
            parent_agent.clone(),
            // Re-use sub-agent (the LLM is shared via Arc so this is fine)
            resolver.resolve("ui-agent").expect("ui-agent must resolve"),
        ],
    });

    // -- Run the parent agent loop --
    let runtime = make_runtime();
    let sink = Arc::new(VecEventSink::new());
    let sink_ref: Arc<dyn EventSink> = sink.clone();

    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "parent".into(),
        RunOrigin::User,
    );

    let result = run_agent_loop(AgentLoopParams {
        resolver: full_resolver.as_ref(),
        agent_id: "parent",
        runtime: &runtime,
        sink: sink_ref,
        checkpoint_store: None,
        messages: vec![Message::user("Create a dashboard")],
        run_identity: identity,
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .expect("agent loop failed");

    // -- Log results --
    tracing::info!(steps = result.steps, response = %result.response, "agent loop complete");

    // Log interesting events
    let events = sink.events();
    tracing::info!(total = events.len(), "collected events");
    for (i, event) in events.iter().enumerate() {
        match event {
            AgentEvent::ToolCallStreamDelta { id, name, delta } => {
                tracing::info!(idx = i, id, name, delta, "ToolCallStreamDelta");
            }
            AgentEvent::ToolCallDone {
                id,
                message_id,
                result,
                ..
            } => {
                tracing::info!(idx = i, id, message_id, ?result, "ToolCallDone");
            }
            _ => {
                tracing::debug!(idx = i, ?event, "event");
            }
        }
    }

    // -- Verify key expectations --
    let tool_stream_deltas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallStreamDelta { .. }))
        .collect();

    assert!(
        !tool_stream_deltas.is_empty(),
        "expected at least one ToolCallStreamDelta from the streaming sub-agent"
    );

    let tool_done_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .collect();

    assert!(
        !tool_done_events.is_empty(),
        "expected at least one ToolCallDone event"
    );

    assert_eq!(
        result.steps, 2,
        "expected 2 steps (tool call + final response)"
    );

    tracing::info!("all assertions passed");
}
