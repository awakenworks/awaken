# Architecture

Tirea runtime is organized as four layers:

```text
Application -> AgentOs -> Agent Loop -> Thread/State Engine
```

## 1. Application Layer

Your application defines tools, agent definitions, and integration endpoints.

Primary call path:

- Build `AgentOs` via `AgentOsBuilder`
- Submit `RunRequest`
- Consume streamed `AgentEvent`

## 2. AgentOs (Orchestration)

`AgentOs` handles pre-run orchestration:

- Resolve agent/model/plugin wiring
- Load or create thread
- Deduplicate incoming messages
- Persist pre-run checkpoint
- Construct `RunContext`

This keeps run setup deterministic and testable.

## 3. Agent Loop (Execution)

Loop is phase-driven:

- `RunStart`
- `StepStart -> BeforeInference -> AfterInference -> BeforeToolExecute -> AfterToolExecute -> StepEnd`
- `RunEnd`

Termination is explicit in `RunFinish.termination`.

## 4. Thread + State Engine

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
