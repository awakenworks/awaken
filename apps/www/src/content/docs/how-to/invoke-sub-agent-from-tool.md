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
awaken = { git = "https://github.com/AwakenWorks/awaken" }
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
use awaken::{StateError, StateKey, StateKeyOptions};
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};

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

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchFindings {
    pub items: Vec<String>,
}

pub struct ResearchFindingsKey;

impl StateKey for ResearchFindingsKey {
    const KEY: &'static str = "research.findings";
    type Value = ResearchFindings;
    type Update = ResearchFindings;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
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
        })?;
        r.register_key::<ResearchFindingsKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })?;
        r.register_key::<ResearchSummaryKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })
    }
}
```

The child agent must register `ResearchConfigKey` so seeding can apply, and it must register `ResearchFindingsKey` with `persistent: true` if you want findings to appear in `outcome.state.extensions`. The parent agent must register `ResearchSummaryKey` before committing the returned `StateCommand`. The single `ResearchPlugin` above registers all three for copy-paste simplicity; in production you may split that into `ChildResearchPlugin` and `ParentResearchPlugin` as long as each side registers the keys it reads or writes.

2. Implement the tool. The key call is [`run_child_agent`](/awaken/reference/) from `awaken_runtime::child_agent`. It returns the child run's terminal [`BackendRunResult`](/awaken/reference/); the parent tool decides how to interpret that lifecycle status as its own `ToolOutput.result`. The example below uses a semantic pass-through policy: the parent tool succeeds with a payload that includes `child_status`, while state export stays conservative.

```rust
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken::contract::event_sink::NullEventSink;
use awaken::contract::message::Message;
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::PersistedState;

use awaken_runtime::backend::{BackendParentContext, BackendRunResult, BackendRunStatus};
use awaken_runtime::child_agent::{ChildAgentParams, run_child_agent};
use awaken_runtime::registry::AgentResolver;
use awaken_runtime::{MutationBatch, StateCommand, StateStore};

pub struct ResearchTool {
    pub resolver: Arc<dyn AgentResolver>,
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

        let outcome = run_child_agent(
            ChildAgentParams::new(
                self.resolver.as_ref(),
                "researcher",
                vec![Message::user(&format!("Research: {topic}"))],
                BackendParentContext {
                    parent_run_id:       Some(ctx.run_identity.run_id.clone()),
                    parent_thread_id:    Some(ctx.run_identity.thread_id.clone()),
                    parent_tool_call_id: Some(ctx.call_id.clone()),
                },
                ctx.activity_sink.clone()
                    .unwrap_or_else(|| Arc::new(NullEventSink)),
            )
            .with_initial_state_seed(seed)
            .with_cancellation_token(ctx.cancellation_token.clone()),
        )
        .await
        .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let command = build_export(&outcome, topic)?;

        Ok(ToolOutput::with_command(
            ToolResult::success("research_topic", json!({
                "child_status": outcome.status.to_string(),
                "response":     outcome.response,
                "child_run_id": outcome.run_id,
                "steps":        outcome.steps,
            })),
            command,
        ))
    }

    fn validate_args(&self, _args: &Value) -> Result<(), ToolError> { Ok(()) }
}
```

3. Build the seed (parent → child) using a temporary store as a typed encoder.

```rust
fn build_seed(topic: &str, max_sources: u32) -> Result<PersistedState, awaken::StateError> {
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
/// Decode child terminal state into a parent `StateCommand`. This export
/// policy is intentionally stricter than the semantic tool-result policy:
/// only a completed child may write research findings back to parent state.
fn build_export(outcome: &BackendRunResult, topic: &str) -> Result<StateCommand, ToolError> {
    let mut cmd = StateCommand::new();
    if !matches!(outcome.status, BackendRunStatus::Completed) {
        return Ok(cmd);
    }
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

### Choose a parent policy for child status

`BackendRunResult.status` is the child run lifecycle status. `ToolOutput.result` is the parent tool's interpretation of that result. The semantic pass-through example above returns a successful parent tool result even when the child reports `Failed`, `Cancelled`, `Timeout`, `Suspended`, or a waiting status, so the parent agent can inspect `child_status` and decide what to do next.

Use a strict policy when the parent tool should fail unless the child completed:

```rust
if !matches!(outcome.status, BackendRunStatus::Completed) {
    return Err(ToolError::ExecutionFailed(format!(
        "sub-agent did not complete: {}",
        outcome.status
    )));
}
```

`run_streaming_subagent` is one such strict helper: because it treats the child's stream as the current tool's output, it rejects non-`Completed` child results. State export is a separate policy decision; do not blindly write child state back to the parent just because the parent tool returned a semantic success payload.

## Stream the child's text into the parent tool's output

When the parent tool wants the child's tokens to appear inside its own streaming output (typical for generative-UI tools), wrap the activity sink with `StreamingPassthroughSink` before passing it to `run_child_agent_checked` or `run_child_agent`:

```rust
use awaken::contract::message::Message;
use awaken_runtime::backend::BackendParentContext;
use awaken_runtime::{
    ChildAgentParams, StreamingPassthroughSink, run_child_agent_checked,
};

let parent_sink = ctx.activity_sink.clone()
    .unwrap_or_else(|| Arc::new(NullEventSink));
let (passthrough, buffer) = StreamingPassthroughSink::new(
    ctx.call_id.clone(),
    ctx.tool_name.clone(),
    parent_sink,
);

let outcome = run_child_agent_checked(
    ChildAgentParams::new(
        self.resolver.as_ref(),
        "researcher",
        vec![Message::user("stream the research")],
        BackendParentContext::default(),
        Arc::new(passthrough),
    )
    .with_cancellation_token(ctx.cancellation_token.clone()),
)
.await
.map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

let streamed_text = buffer.lock().await.clone();
```

Child `AgentEvent::TextDelta` events become `AgentEvent::ToolCallStreamDelta` on the parent sink, keyed by the parent tool's `call_id`. `buffer` accumulates only child text output. By default, child `AgentEvent::Error` events are also wrapped as `ToolCallStreamDelta` diagnostics so front-ends do not mistake them for fatal parent-run errors, but those diagnostics are not appended to `buffer`; use `StreamingPassthroughSink::new_with_error_forwarding(..., ChildErrorForwarding::ForwardRawParentError)` only when your event consumer explicitly opts into raw child errors.

## Backend implementor migration note

`ExecutionBackend::capabilities()` now returns `BackendProfile`, with typed
dimensions such as continuation, persistence, waits, transcript shape, and
output shape. Construct profiles with `BackendProfile::full_local()` or
`BackendProfile::remote_stateless_text()` and override fields only when your
backend really supports that behavior.

Seeded delegate requests are handled separately from `BackendProfile`.
`BackendDelegateRunRequest.state_seed` is accepted only for local execution
plans; non-local backends reject seeded delegate calls with
`ExecutionBackendError` instead of silently ignoring the seed.

## What to avoid

- **Do not seed keys the child agent has not registered.** The child runs `apply_seed` with `UnknownKeyPolicy::Error` — an unregistered key aborts the child before its first step. This is by design: it surfaces contract drift at startup rather than runtime.
- **Do pass parent cancellation through.** When invoking a child from inside a tool, call `.with_cancellation_token(ctx.cancellation_token.clone())` so cancelling the parent run also cancels the child run.
- **`initial_state_seed` is Local-backend only.** It is accepted only when the resolved `ExecutionPlan` is local. A2A and any other non-local backend that does not implement a seed-passing wire protocol reject seeded delegate requests with `ExecutionBackendError` — they will not silently succeed. If you need to ship data to a remote child, encode it in the prompt yourself.
- **Do not blindly export child state on non-`Completed` status.** The child result is a semantic message for the parent to interpret; decide separately whether the parent tool should fail, return a semantic success payload, or selectively export diagnostic state. For terminal `BackendRunResult` statuses such as `Failed` or `Cancelled`, `outcome.state` may be available depending on the backend and where the failure occurred. Backend dispatch or loop setup errors return `Err` and do not provide `BackendRunResult.state`.
- **Do not assume non-persistent keys cross the run boundary.** `BackendRunResult.state` is built via `export_persisted` and only includes keys registered with `persistent: true`.
- **Do not pass `ctx.activity_sink` directly to a streaming sub-agent.** Without `StreamingPassthroughSink`, the child's `TextDelta` events would surface as the parent's text — leaking the child's tokens into the parent's primary message stream. Wrap or pass `NullEventSink`.
- **Be aware of non-local transcript semantics.** When the child runs through the A2A backend (or any other transcript-incremental backend), only `User`-role content with `Visibility::All` is forwarded to the remote agent — assistant/tool history is not. If your child needs prior context, bake it into the user prompt or use the Local backend.
- **Do not confuse A2A delegate `run_id` with the remote task id.** For delegate calls, `BackendRunResult.run_id` is a local-only correlation id for child tooling, suspension, and tracing. The remote A2A task id remains in A2A progress metadata/state and is not replaced by this synthesized local id.
- **`initial_messages` is a fresh-delegation seed, not a history/new-turn split.** `ChildAgentParams::new(..., initial_messages, ...)` is what the child sees as its starting input — typically a single `Message::user`. The current API does not support resuming a prior delegate transcript. Internally, `run_child_agent` mirrors this fresh input into `BackendDelegateRunRequest.messages` and `.new_messages`; do not rely on that backend-level duplication to mean the public API supports continuation.
- **Raw child errors on the passthrough sink are opt-in.** `StreamingPassthroughSink::new` wraps child `AgentEvent::Error` as parent `ToolCallStreamDelta` output by default. Only choose `ChildErrorForwarding::ForwardRawParentError` if your UI understands that the raw error came from a child tool stream and should not automatically kill the parent run.

## See Also

- [Multi-Agent Patterns](/awaken/explanation/multi-agent-patterns/) — when to use delegation vs handoff vs sub-agent
- [Add a Tool](/awaken/how-to/add-a-tool/) — the underlying `Tool` trait
- [Use Generative UI](/awaken/how-to/use-generative-ui/) — `run_streaming_subagent` is now a thin wrapper around `run_child_agent` + `StreamingPassthroughSink`
- [Use Shared State](/awaken/how-to/use-shared-state/) — defining `StateKey` and plugins
