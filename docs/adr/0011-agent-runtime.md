# ADR-0011: Agent Runtime — Run Management, Routing, and Control

- **Status**: Accepted
- **Date**: 2026-03-24
- **Depends on**: ADR-0001, ADR-0006, ADR-0010

> Storage persistence details in this ADR are partially superseded by ADR-0012 (transactional thread+run checkpoint).

## Context

ADR-0010 defines registries and resolution (`AgentSpec → resolve() → ResolvedAgent`). ADR-0006 defines the run lifecycle state machine. What's missing is the layer that orchestrates runs: routing requests to agents, managing active runs, handling resume/cancel, and supporting handoff.

The current `run_agent_loop` is a bare function that takes a fixed `AgentConfig`. It cannot: route requests to different agents, cancel a running loop, accept runtime decisions for suspended tool calls, or perform handoff (switch agent mid-run). These are responsibilities of a runtime layer above the loop runner.

Reference: uncarve's `AgentOs` + `ThreadRunHandle` + `ActiveThreadRunRegistry` pattern.

## Decisions

### D1: AgentRuntime — top-level orchestrator

```rust
pub struct AgentRuntime {
    resolver: Arc<dyn AgentResolver>,
    thread_store: Option<Arc<dyn ThreadStore>>,
    run_store: Option<Arc<dyn RunStore>>,
    active_runs: ActiveRunRegistry,
}
```

Single entry point for all run operations: start, cancel, send decisions. Manages the lifecycle of active runs across threads.

### D2: AgentResolver — dynamic agent resolution

```rust
pub trait AgentResolver: Send + Sync {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, ResolveError>;
}

pub struct ResolvedAgent {
    pub config: AgentConfig,
    pub env: ExecutionEnv,
}
```

`RegistrySet` implements `AgentResolver`. Resolution produces a fully wired `AgentConfig` + `ExecutionEnv` from the registry chain: `AgentSpec → Model → Provider → LlmExecutor`, tools filtered, plugins installed.

The loop runner receives `&dyn AgentResolver` (not a fixed config) so it can re-resolve at step boundaries for handoff.

### D3: RunRequest — unified request

```rust
pub struct RunRequest {
    pub agent_id: Option<String>,
    pub thread_id: String,
    pub messages: Vec<Message>,
    pub overrides: Option<InferenceOverride>,
    pub decisions: Vec<ToolCallDecision>,
}
```

- `agent_id`: target agent. `None` = infer from thread state or default.
- `thread_id`: existing thread → load history; new → create.
- `messages`: new messages to append.
- `decisions`: resume decisions for suspended tool calls. Empty = fresh run.

No separate "resume" request type. The runtime detects resume from decisions + thread state.

### D4: RunHandle — external control

```rust
pub struct RunHandle {
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    cancellation_token: CancellationToken,
    decision_tx: mpsc::UnboundedSender<ToolCallDecision>,
}

impl RunHandle {
    pub fn cancel(&self);
    pub fn send_decision(&self, d: ToolCallDecision) -> Result<(), ...>;
}
```

Returned by `AgentRuntime::run()`. Enables:
- **Cancel**: cooperative cancellation via `CancellationToken`. Loop checks at phase boundaries and during LLM inference.
- **Send decision**: forward tool call approvals/rejections to a running loop via mpsc channel.

### D5: ActiveRunRegistry — one run per thread

```rust
struct ActiveRunRegistry {
    by_run_id: RwLock<HashMap<String, RunEntry>>,
    by_thread_id: RwLock<HashMap<String, String>>,
}
```

Invariant: at most one active run per thread. Enforced at `AgentRuntime::run()`:
- Thread has active Running run → reject (or queue).
- Thread has active Waiting run + decisions present → auto-resume.
- Thread has no active run → start new.

### D6: Loop runner accepts AgentResolver for handoff

```rust
pub async fn run_agent_loop(
    resolver: &dyn AgentResolver,
    initial_agent_id: &str,
    runtime: &PhaseRuntime,
    sink: &dyn EventSink,
    thread_store: Option<&dyn ThreadStore>,
    initial_messages: Vec<Message>,
    run_identity: RunIdentity,
    cancellation_token: Option<CancellationToken>,
    decision_rx: Option<mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Result<AgentRunResult, AgentLoopError>
```

The loop resolves the initial agent, then at each step boundary checks `ActiveAgentKey`. If changed, re-resolves from the resolver — new config, new env, new model, new tools. Handoff is a re-resolve, not a loop restart.

### D7: State-driven resume

No `ResumeInput` parameter. Resume is state-driven:

1. Caller writes decisions to `ToolCallStates` via `prepare_resume()`.
2. Loop detects `Resuming` tool calls at startup.
3. Loop replays resumed calls before entering the step loop.

```rust
pub fn prepare_resume(
    store: &StateStore,
    decisions: Vec<(String, ToolCallResume)>,
    resume_mode: ToolCallResumeMode,
) -> Result<(), StateError>
```

### D8: CancellationToken — cooperative cancellation

The loop checks the token at:
- Phase boundaries (after each `run_phase`)
- Before LLM inference
- During tool execution (passed to tool via `ToolCallContext`)

When cancelled, the loop writes `RunLifecycle::Done { reason: "cancelled" }` and returns.

### D9: Runtime decision channel

During execution, the loop monitors `decision_rx` for incoming `ToolCallDecision`s. When a decision arrives for a suspended tool call, the loop applies it and continues. This enables "live resume" without stopping the loop.

Initially implemented as polling between phases. Future: `tokio::select!` with streaming inference for true concurrent monitoring.

## Runtime flow

```
AgentRuntime::run(request)
  ├─ Resolve: resolver.resolve(agent_id) → ResolvedAgent
  ├─ Load: thread_store.load_messages(thread_id) → messages
  ├─ Prepare: append new messages, prepare_resume if decisions present
  ├─ Register: ActiveRunRegistry.insert(run_id, thread_id)
  ├─ Create: CancellationToken + decision channel
  ├─ Execute: spawn run_agent_loop(resolver, ...)
  └─ Return: RunHandle { cancel, send_decision }

run_agent_loop(resolver, ...)
  ├─ Initial resolve → config + env
  ├─ Detect resuming tool calls → replay
  ├─ RunStart phase
  └─ Step loop:
      ├─ Check cancellation token
      ├─ Check ActiveAgentKey → re-resolve if changed (handoff)
      ├─ StepStart → BeforeInference → inference → AfterInference
      ├─ Tool execution
      ├─ Check decision_rx → apply if pending
      ├─ StepEnd → checkpoint persist
      └─ Check RunLifecycle → break if not Running
```

## Consequences

- Single entry point (`AgentRuntime::run`) handles fresh runs, resumes, and auto-detection
- `RunHandle` enables external cancel and live decision injection
- Handoff is a re-resolve at step boundary, not a loop restart
- One active run per thread enforced by registry
- State-driven resume eliminates separate resume function and parameter
- `CancellationToken` enables cooperative cancellation without killing the task
