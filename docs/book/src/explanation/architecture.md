# Architecture

Tirea runtime is organized as three layers:

```text
Application -> AgentOs (orchestration + execution engine) -> Thread/State Engine
```

## 1. Application Layer

Your application defines tools, agent definitions, and integration endpoints.

Primary call path:

- Build `AgentOs` via `AgentOsBuilder`
- Submit `RunRequest`
- Consume streamed `AgentEvent`

## Representative Request Sequence

The diagram below follows one concrete request path:

- HTTP endpoint: `POST /v1/ai-sdk/agents/:agent_id/runs`
- Handler: `crates/tirea-agentos-server/src/protocol/ai_sdk_v6/http.rs`
- Runtime bootstrap: `crates/tirea-agentos-server/src/service/run.rs`
- Agent loop:
  - `crates/tirea-agentos/src/runtime/run.rs`
  - `crates/tirea-agentos/src/runtime/loop_runner/mod.rs`

It shows the full path from the frontend request to persisted thread/run state
and the streamed SSE response.

```mermaid
sequenceDiagram
    autonumber
    participant Client as AI SDK client
    participant Http as ai_sdk_v6::http::run
    participant Os as AgentOs
    participant Store as ThreadStore
    participant Relay as HTTP SSE relay
    participant Loop as loop_runner
    participant Llm as LLM provider
    participant Tool as Tool executor

    Client->>Http: POST /v1/ai-sdk/agents/:agent_id/runs
    Http->>Http: validate request + extract suspension decisions
    Http->>Os: resolve(agent_id)
    Os-->>Http: ResolvedRun
    Http->>Http: apply_ai_sdk_extensions(resolved, request)
    Http->>Os: start_active_run_with_persistence(...)
    Os->>Store: load/create thread
    Store-->>Os: thread head / new version
    Os->>Store: append(UserMessage + run-start patches)
    Store-->>Os: committed version
    Os->>Loop: execute_prepared(prepared_run)
    Loop-->>Os: RunStream(thread_id, run_id, AgentEvent stream)
    Os-->>Http: Prepared HTTP run
    Http->>Relay: wire_http_sse_relay(run, AiSdkEncoder, ingress_rx)
    Http-->>Client: SSE response headers

    Relay->>Loop: send RuntimeInput::Run(request)
    loop each agent step
        Loop->>Loop: RunStart / StepStart / BeforeInference
        Loop->>Llm: exec_chat_response(...)
        Llm-->>Loop: streamed/tool-call-capable response
        Loop->>Store: append(AssistantTurnCommitted)
        alt tool calls present
            Loop->>Tool: execute tool round
            Tool-->>Loop: ToolResult / Suspended
            Loop->>Store: append(ToolResultsCommitted)
        end
        Loop-->>Relay: AgentEvent stream
        Relay-->>Client: AI SDK SSE frames
    end

    Loop->>Store: append(RunFinished + final snapshot)
    Loop-->>Relay: RunFinish
    Relay-->>Client: final AI SDK events + [DONE]
```

Notes:

- The transport layer starts the already-prepared run lazily when the relay
  forwards `RuntimeInput::Run`.
- `AgentOs` owns thread/run preparation and persistence; the relay only bridges
  runtime events into protocol-specific SSE frames.
- AG-UI follows the same backbone, but applies AG-UI-specific runtime
  extensions and a different event encoder.

## 2. AgentOs (Orchestration + Execution)

`AgentOs` handles both pre-run orchestration and loop execution:

**Orchestration** (`composition/`, `runtime/`):
- Resolve agent/model/plugin wiring (plugins implement the `AgentBehavior` trait)
- Load or create thread
- Deduplicate incoming messages
- Persist pre-run checkpoint
- Construct `RunContext`

**Execution engine** (`engine/`, `runtime/loop_runner/`):

Loop is phase-driven:

- `RunStart`
- `StepStart -> BeforeInference -> AfterInference -> BeforeToolExecute -> AfterToolExecute -> StepEnd`
- `RunEnd`

Termination is explicit in `RunFinish.termination`.

## 3. Thread + State Engine

State mutation is patch-based:

- `State' = apply_patch(State, Patch)`
- `Thread` stores base state + patch history + messages
- `RunContext` accumulates run delta and emits `take_delta()` for persistence

## Design Intent

- Deterministic state transitions
- Append-style persistence with version checks
- Transport-independent runtime (`AgentEvent` as core stream)

## See Also

- [Run Lifecycle and Phases](./run-lifecycle-and-phases.md)
- [Frontend Interaction and Approval Model](./frontend-interaction-and-approval-model.md)
- [Persistence and Versioning](./persistence-and-versioning.md)
- [HTTP API](../reference/http-api.md)
- [Events](../reference/events.md)
