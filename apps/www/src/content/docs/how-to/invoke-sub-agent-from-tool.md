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
awaken-contract = "0.5"
awaken-runtime = "0.5"
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

The helper and its companion types live in `awaken_runtime::child_agent`; the `awaken` facade does not re-export them, so import directly from `awaken_runtime`.

## Steps

1. Declare a `StateKey` that both parent and child agree on.

```rust
use awaken_contract::state::{StateKey, StateKeyOptions};
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_contract::StateError;

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

use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::state::PersistedState;

use awaken_runtime::backend::{
    BackendControl, BackendDelegatePolicy, BackendParentContext, BackendRunResult,
    BackendRunStatus,
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

        // Surface non-success child terminations as a tool error rather than
        // returning ToolResult::success with no state update. See "What to
        // avoid" below for the suspension passthrough alternative.
        if !matches!(outcome.status, BackendRunStatus::Completed) {
            return Err(ToolError::ExecutionFailed(format!(
                "child agent did not complete: {}",
                outcome.status,
            )));
        }

        let command = build_export(&outcome, topic)?;

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
fn build_seed(topic: &str, max_sources: u32) -> Result<PersistedState, awaken_contract::StateError> {
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

4. Build the export (child → parent) by decoding the child's terminal state.

The child's `StateStore` final snapshot is returned in `BackendRunResult.state` (a `PersistedState`). Decode the keys you care about and translate them into a `StateCommand` keyed against parent state keys — the loop runner will commit it after your tool returns.

```rust
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchFindings {
    pub items: Vec<String>,
}

pub struct ResearchFindingsKey;

impl StateKey for ResearchFindingsKey {
    const KEY: &'static str = "research.findings";
    type Value = ResearchFindings;
    type Update = ResearchFindings;
    fn apply(value: &mut Self::Value, update: Self::Update) { *value = update; }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchSummary {
    pub topic: String,
    pub items: Vec<String>,
}

pub struct ResearchSummaryKey;

impl StateKey for ResearchSummaryKey {
    const KEY: &'static str = "research.summary";
    type Value = ResearchSummary;
    type Update = ResearchSummary;
    fn apply(value: &mut Self::Value, update: Self::Update) { *value = update; }
}

/// Decode child terminal state into a parent `StateCommand`. The outer
/// `execute` already rejects non-Completed terminations, so this helper
/// only needs to handle the success path's "no findings key present" edge.
fn build_export(outcome: &BackendRunResult, topic: &str) -> Result<StateCommand, ToolError> {
    let mut cmd = StateCommand::new();
    let Some(state) = outcome.state.as_ref() else {
        return Ok(cmd);
    };
    let Some(json) = state.extensions.get(ResearchFindingsKey::KEY) else {
        return Ok(cmd);
    };
    let findings: ResearchFindings = serde_json::from_value(json.clone())
        .map_err(|e| ToolError::ExecutionFailed(format!("decode findings: {e}")))?;

    let mut batch = MutationBatch::new();
    batch.update::<ResearchSummaryKey>(ResearchSummary {
        topic: topic.into(),
        items: findings.items,
    });
    cmd.patch
        .extend(batch)
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    Ok(cmd)
}
```

The loop runner commits `ToolOutput.command` to the parent store after the tool returns — see [Tool and Plugin Boundary](/awaken/explanation/tool-and-plugin-boundary/). No new commit path is involved; this is the same machinery any tool already uses.

Only keys registered with `persistent: true` on the child appear in `outcome.state.extensions`. If a value you need is non-persistent, either change the child key registration or fall back to `outcome.response` / `outcome.output` (the structured text output is preserved regardless of persistence).

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
- **`initial_state_seed` is Local-backend only.** State seeding is gated by `BackendCapabilities::delegate_state_seed`; only the in-process Local backend currently advertises support. A2A and any other non-local backend that does not implement a seed-passing wire protocol reject seeded delegate requests with `ExecutionBackendError` — they will not silently succeed. If you need to ship data to a remote child, encode it in the prompt yourself.
- **Do not export on non-`Completed` status.** `outcome.state` is populated even on failure/cancellation so you can inspect diagnostics, but writing partial child state back into the parent risks inconsistency. Check `outcome.status` first.
- **Do not assume non-persistent keys cross the run boundary.** `BackendRunResult.state` is built via `export_persisted` and only includes keys registered with `persistent: true`.
- **Do not pass `ctx.activity_sink` directly to a streaming sub-agent.** Without `StreamingPassthroughSink`, the child's `TextDelta` events would surface as the parent's text — leaking the child's tokens into the parent's primary message stream. Wrap or pass `NullEventSink`.
- **Be aware of non-local transcript semantics.** When the child runs through the A2A backend (or any other transcript-incremental backend), only `User`-role content with `Visibility::All` is forwarded to the remote agent — assistant/tool history is not. If your child needs prior context, bake it into the user prompt or use the Local backend.
- **Child errors on the passthrough sink surface as parent-scope errors.** `StreamingPassthroughSink` re-emits child `AgentEvent::Error` events on the parent sink unchanged. Front-ends that treat run-level errors as fatal will see child failures that way too — wrap or filter in your sink layer if you need tool-scoped error semantics.

## See Also

- [Multi-Agent Patterns](/awaken/explanation/multi-agent-patterns/) — when to use delegation vs handoff vs sub-agent
- [Add a Tool](/awaken/how-to/add-a-tool/) — the underlying `Tool` trait
- [Use Generative UI](/awaken/how-to/use-generative-ui/) — `run_streaming_subagent` is now a thin wrapper around `run_child_agent` + `StreamingPassthroughSink`
- [Use Shared State](/awaken/how-to/use-shared-state/) — defining `StateKey` and plugins
