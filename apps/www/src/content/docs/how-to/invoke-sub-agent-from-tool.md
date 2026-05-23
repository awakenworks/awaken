---
title: "Invoke a Sub-Agent from a Tool"
description: "Use this when a tool needs to delegate work to another agent and control exactly which parent state flows in and which child state flows back."
---

Use this when a tool needs to delegate work to another agent **and** control exactly which parent state flows into the child run and which child state flows back into the parent store.

Awaken exposes this through one helper function plus the normal `Tool::execute` shape you already know. The framework does not introduce hooks, phases, or strategy types — state passing is plain Rust code inside your `execute` method.

## Prerequisites

- A working agent runtime (see [Build an Agent](/awaken/how-to/build-an-agent/))
- A `Tool` implementation (see [Add a Tool](/awaken/how-to/add-a-tool/))
- A child agent registered with the runtime's resolver so the helper can resolve it

```toml
[dependencies]
awaken = { version = "0.5" }
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

## Steps

1. Declare a `StateKey` that both parent and child agree on.

```rust
use awaken::contract::state::{StateKey, StateKeyOptions};
use awaken::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken::contract::StateError;

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchConfig {
    pub topic: String,
    pub max_sources: u32,
}

pub struct ResearchConfigKey;

impl StateKey for ResearchConfigKey {
    const KEY: &'static str = "research.config";
    type Value = ResearchConfig;
    type Update = ResearchConfig;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

pub struct ResearchPlugin;

impl Plugin for ResearchPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "research-plugin" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<ResearchConfigKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })
    }
}
```

The child agent must have `ResearchPlugin` in its plugin set — otherwise the seed step will fail with `StateError::UnknownKey`. The parent agent must have it too if you intend to write to `ResearchConfigKey` from the parent side.

2. Implement the tool. The key call is [`run_child_agent`](/awaken/reference/) from `awaken_runtime::child_agent`.

```rust
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken::contract::event_sink::NullEventSink;
use awaken::contract::message::Message;
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::contract::state::PersistedState;

use awaken_runtime::backend::{
    BackendControl, BackendDelegatePolicy, BackendParentContext, BackendRunStatus,
};
use awaken_runtime::child_agent::{ChildAgentParams, run_child_agent};
use awaken_runtime::registry::ExecutionResolver;
use awaken_runtime::{MutationBatch, StateCommand, StateStore};

pub struct ResearchTool {
    pub resolver: Arc<dyn ExecutionResolver>,
}

#[async_trait]
impl Tool for ResearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("research_topic", "research_topic",
            "Deep-dive research on a topic with cited sources")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "topic":       { "type": "string" },
                    "max_sources": { "type": "integer", "minimum": 1 }
                },
                "required": ["topic"]
            }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext)
        -> Result<ToolOutput, ToolError>
    {
        let topic = args["topic"].as_str()
            .ok_or_else(|| ToolError::InvalidArguments("topic required".into()))?;
        let max_sources = args["max_sources"].as_u64().unwrap_or(5) as u32;

        let seed = build_seed(topic, max_sources)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let outcome = run_child_agent(ChildAgentParams {
            resolver:           self.resolver.as_ref(),
            agent_id:           "researcher",
            messages:           vec![Message::user(&format!("Research: {topic}"))],
            parent: BackendParentContext {
                parent_run_id:       Some(ctx.run_identity.run_id.clone()),
                parent_thread_id:    Some(ctx.run_identity.thread_id.clone()),
                parent_tool_call_id: Some(ctx.call_id.clone()),
            },
            initial_state_seed: Some(seed),
            sink:               ctx.activity_sink.clone()
                                   .unwrap_or_else(|| Arc::new(NullEventSink)),
            control:            BackendControl::default(),
            policy:             BackendDelegatePolicy::default(),
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let command = build_export(&outcome, topic);

        Ok(ToolOutput {
            result: ToolResult::success("research_topic", json!({
                "response":     outcome.response,
                "child_run_id": outcome.run_id,
                "steps":        outcome.steps,
            })),
            command,
            ..Default::default()
        })
    }

    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> { Ok(()) }
}
```

3. Build the seed (parent → child) using a temporary store as a typed encoder.

```rust
fn build_seed(topic: &str, max_sources: u32) -> Result<PersistedState, awaken::contract::StateError> {
    let scratch = StateStore::new();
    scratch.install_plugin(ResearchPlugin)?;
    let mut batch = MutationBatch::new();
    batch.update::<ResearchConfigKey>(ResearchConfig {
        topic: topic.into(),
        max_sources,
    });
    scratch.commit(batch)?;
    scratch.export_persisted()
}
```

Only `StateKey` entries with `persistent: true` survive `export_persisted`. If a seed key was registered with `persistent: false`, write it directly into `PersistedState.extensions` as raw JSON instead.

4. Build the export (child → parent) from the child's terminal state.

```rust
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchSummary {
    pub topic: String,
    pub summary: String,
}

pub struct ResearchSummaryKey;

impl StateKey for ResearchSummaryKey {
    const KEY: &'static str = "research.summary";
    type Value = ResearchSummary;
    type Update = ResearchSummary;
    fn apply(value: &mut Self::Value, update: Self::Update) { *value = update; }
}

fn build_export(outcome: &awaken_runtime::backend::BackendRunResult, topic: &str)
    -> StateCommand
{
    let mut cmd = StateCommand::new();
    // Skip the export on non-Completed terminations so failed/cancelled child
    // runs do not corrupt parent state.
    if !matches!(outcome.status, BackendRunStatus::Completed) {
        return cmd;
    }
    let mut batch = MutationBatch::new();
    batch.update::<ResearchSummaryKey>(ResearchSummary {
        topic: topic.into(),
        summary: outcome.response.clone().unwrap_or_default(),
    });
    let _ = cmd.patch.extend(batch);
    cmd
}
```

The loop runner commits `ToolOutput.command` to the parent store after the tool returns — see [Tool and Plugin Boundary](/awaken/explanation/tool-and-plugin-boundary/). No new commit path is involved; this is the same machinery any tool already uses.

## Stream the child's text into the parent tool's output

When the parent tool wants the child's tokens to appear inside its own streaming output (typical for generative-UI tools), wrap the activity sink with `StreamingPassthroughSink` before passing it to `run_child_agent`:

```rust
use awaken_runtime::StreamingPassthroughSink;

let parent_sink = ctx.activity_sink.clone()
    .unwrap_or_else(|| Arc::new(NullEventSink));
let (passthrough, buffer) = StreamingPassthroughSink::new(
    ctx.call_id.clone(),
    ctx.tool_name.clone(),
    parent_sink,
);

let outcome = run_child_agent(ChildAgentParams {
    sink: Arc::new(passthrough),
    // ...other fields as above...
}).await?;

let streamed_text = buffer.lock().await.clone();
```

Child `AgentEvent::TextDelta` events become `AgentEvent::ToolCallStreamDelta` on the parent sink, keyed by the parent tool's `call_id`. `buffer` accumulates the full text.

## What to avoid

- **Do not seed keys the child agent has not registered.** The child runs `apply_seed` with `UnknownKeyPolicy::Error` — an unregistered key aborts the child before its first step. This is by design: it surfaces contract drift at startup rather than runtime.
- **Do not export on non-`Completed` status.** `outcome.state` is populated even on failure/cancellation so you can inspect diagnostics, but writing partial child state back into the parent risks inconsistency. Check `outcome.status` first.
- **Do not assume non-persistent keys cross the run boundary.** `BackendRunResult.state` is built via `export_persisted` and only includes keys registered with `persistent: true`.
- **Do not pass `ctx.activity_sink` directly to a streaming sub-agent.** Without `StreamingPassthroughSink`, the child's `TextDelta` events would surface as the parent's text — leaking the child's tokens into the parent's primary message stream. Wrap or pass `NullEventSink`.

## See Also

- [Multi-Agent Patterns](/awaken/explanation/multi-agent-patterns/) — when to use delegation vs handoff vs sub-agent
- [Add a Tool](/awaken/how-to/add-a-tool/) — the underlying `Tool` trait
- [Use Generative UI](/awaken/how-to/use-generative-ui/) — `run_streaming_subagent` is now a thin wrapper around `run_child_agent` + `StreamingPassthroughSink`
- [Use Shared State](/awaken/how-to/use-shared-state/) — defining `StateKey` and plugins
