# HITL and Decision Flow

Human-in-the-loop in Tirea is not a separate transport feature. It is part of the core run model: tools can suspend, runs can wait, and external decisions can be forwarded back into the active run for replay.

## The Three States That Matter

1. A tool call suspends.
2. The run transitions to `Waiting` if all active tool calls are suspended.
3. An external decision resumes or cancels the suspended call, and the loop replays that tool call deterministically.

This is why HITL behavior spans runtime, protocol, and storage layers instead of living only in frontend adapters.

## Two Inbound Paths

There are two distinct ways to continue work after a suspension:

- Decision forwarding:
  an active suspended run receives `ToolCallDecision` payloads and continues in place
- Continuation run:
  a new run starts with additional user messages, optionally carrying decisions with it

These paths are intentionally different:

- decision forwarding preserves the existing run id and resumes the suspended call;
- continuation starts a new run lineage and is the right choice when the user is adding new intent, not only resolving a pending approval.

## Decision Forwarding

Decision forwarding is the canonical HITL path.

Flow:

1. A tool returns `Pending(...)` or a behavior suspends it before execution.
2. Suspended call payload is stored in runtime state.
3. Client posts a `ToolCallDecision` to the active run.
4. The loop resolves the decision, marks the call `Resuming`, replays it, and commits the resulting effects.

The important property is replay: the original tool call is not mutated in place by the client. The client only submits the decision payload; the runtime owns the actual resumed execution.

## Continuation Runs

Continuation is a different mechanism:

- a new user message is appended;
- a new run id is created;
- `parent_run_id` links the new run back to the suspended or completed parent.

Use continuation when the user is changing the conversation, not merely approving or denying a specific tool call.

## Tool Execution Mode Changes the UX

`tool_execution_mode` changes how suspension feels from the outside:

| Mode | Practical effect on HITL |
|---|---|
| `sequential` | Simplest mental model; one call is active at a time |
| `parallel_batch_approval` | Multiple calls may suspend in a round, then resume behavior is applied after batch commit |
| `parallel_streaming` | Stream mode can emit activity updates and apply decisions while tools are still in flight |

If your UI needs live progress, rich activity cards, or low-latency approval loops, `parallel_streaming` is the intended mode.

## Durable State and Lineage

HITL depends on persisted runtime state:

- `__run` tracks run lifecycle
- `__tool_call_states` tracks per-call status
- `__suspended_tool_calls` stores suspended call payloads

Lineage fields keep the larger execution graph understandable:

- `run_id`
- `thread_id`
- `parent_run_id`
- `parent_thread_id`

This matters most once you combine approvals, persisted threads, and sub-agent delegation.

## Transport Mapping

The same model appears through multiple transports:

- Run API:
  `POST /v1/runs/:id/inputs` forwards decisions to the active run
- AG-UI:
  protocol adapters forward decisions onto the same runtime channel
- A2A:
  decision-only requests map to the same `ToolCallDecision` contract

The transport changes payload shape and encoding, but the runtime semantics stay the same.

## When to Reach for Which Doc

- Use [Enable Tool Permission HITL](../how-to/enable-tool-permission-hitl.md) to wire approval behavior
- Use [Run API](../reference/run-api.md) for concrete HTTP payloads
- Use [Frontend Interaction and Approval Model](./frontend-interaction-and-approval-model.md) for the unified design behind frontend tools and approvals
- Use [Run Lifecycle and Phases](./run-lifecycle-and-phases.md) for full internal state-machine detail

This page is the conceptual bridge between those pieces.
