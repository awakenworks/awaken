---
title: "HITL and Mailbox"
description: "This page explains how Awaken handles human-in-the-loop (HITL) interactions through tool call suspension and the mailbox queue that manages run requests."
---

This page explains how Awaken handles human-in-the-loop (HITL) interactions through tool call suspension and the mailbox queue that manages run requests.

## SuspendTicket

When a tool call needs external approval or input, it produces a `SuspendTicket`:

```rust
pub struct SuspendTicket {
    pub suspension: Suspension,
    pub pending: PendingToolCall,
    pub resume_mode: ToolCallResumeMode,
}
```

**suspension** -- the external-facing payload describing what input is needed:

```rust
pub struct Suspension {
    pub id: String,             // Unique suspension ID
    pub action: String,         // Action identifier (e.g., "confirm", "approve")
    pub message: String,        // Human-readable prompt
    pub parameters: Value,      // Action-specific parameters
    pub response_schema: Option<Value>,  // JSON Schema for expected response
}
```

**pending** -- the tool call projection emitted to the event stream so the frontend knows which call is waiting:

```rust
pub struct PendingToolCall {
    pub id: String,        // Tool call ID
    pub name: String,      // Tool name
    pub arguments: Value,  // Original arguments
}
```

**resume_mode** -- how the agent loop should handle the decision when it arrives.

## ToolCallResumeMode

```rust
pub enum ToolCallResumeMode {
    ReplayToolCall,          // Re-execute the original tool call
    UseDecisionAsToolResult, // Use the decision payload as the tool result directly
    PassDecisionToTool,      // Pass the decision payload into the tool as new arguments
}
```

`ReplayToolCall` is the default. The original tool call is re-executed after the decision arrives. Use this when the decision unlocks execution (e.g., permission granted, now run the tool).

`UseDecisionAsToolResult` skips re-execution entirely. The external decision payload becomes the tool result. Use this when a human provides the answer directly (e.g., "the correct value is X").

`PassDecisionToTool` re-executes the tool but injects the decision payload into the arguments. Use this when the decision modifies how the tool should run (e.g., "use this alternative path instead").

## ResumeDecisionAction

```rust
pub enum ResumeDecisionAction {
    Resume,  // Proceed with the tool call
    Cancel,  // Cancel the tool call
}
```

Each decision carries an action. `Resume` continues execution according to the `ToolCallResumeMode`. `Cancel` transitions the tool call to `ToolCallStatus::Cancelled`.

## ToolCallResume

The full resume payload:

```rust
pub struct ToolCallResume {
    pub decision_id: String,         // Idempotency key
    pub action: ResumeDecisionAction,
    pub result: Value,               // Decision payload
    pub reason: Option<String>,      // Human-readable reason
    pub updated_at: u64,             // Unix millis timestamp
}
```

## Permission Plugin and Ask-Mode

The `awaken-ext-permission` plugin uses suspension to implement ask-mode approvals:

1. A tool call matches a permission rule with `behavior: ask`.
2. The permission checker creates a `SuspendTicket` with `ToolCallResumeMode::ReplayToolCall`.
3. The suspension payload describes the tool and its arguments.
4. The tool call transitions to `ToolCallStatus::Suspended`.
5. The run transitions to `RunStatus::Waiting`.
6. A frontend presents the approval prompt to the user.
7. The user submits a `ToolCallResume` with `ResumeDecisionAction::Resume` or `Cancel`.
8. On `Resume`, the tool call replays and executes normally.
9. On `Cancel`, the tool call is cancelled and the run may continue with remaining calls.

## Mailbox Architecture

The mailbox provides a persistent dispatch queue for run activations. Every
durable run activation -- streaming, background, A2A, internal -- enters the
system as a `RunDispatch`.

`RunDispatch` owns delivery, lease, retry, and queue-audit state. The run's
business truth lives on `RunRecord`.

### RunDispatch

```rust
pub struct RunDispatch {
    // Identity
    pub dispatch_id: String,     // UUID v7
    pub thread_id: String,       // Thread ID (routing anchor)
    pub run_id: String,          // Canonical runtime run ID

    // Queue semantics
    pub priority: u8,            // 0 = highest, 255 = lowest, default 128
    pub dedupe_key: Option<String>,
    pub dispatch_epoch: u64,

    // Lifecycle
    pub status: RunDispatchStatus,
    pub available_at: u64,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub last_error: Option<String>,

    // Lease
    pub claim_token: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<u64>,

    // Runtime trace projection
    pub dispatch_instance_id: Option<String>,
    pub run_status: Option<RunStatus>,
    pub termination: Option<TerminationReason>,
    pub run_response: Option<String>,
    pub run_error: Option<String>,
    pub completed_at: Option<u64>,

    // Timestamps
    pub created_at: u64,
    pub updated_at: u64,
}
```

Dispatch records do not store request messages, agent identity, request extras,
or transport payload. Activation reconstruction loads `RunRecord.request` and
the thread message log.

### RunDispatchStatus

```text
Queued --claim--> Claimed --ack--> Acked (terminal)
  |                  |
  |               nack(retry) --> Queued (attempt_count++, available_at = retry_at)
  |                  |
  |               nack(permanent) --> DeadLetter (terminal)
  |
  |-- cancel --> Cancelled (terminal)
  +-- interrupt(dispatch epoch bump) --> Superseded (terminal)
```

```rust
pub enum RunDispatchStatus {
    Queued,      // Waiting to be claimed
    Claimed,     // Claimed by a consumer, executing
    Acked,       // Dispatch consumed, do not retry (terminal)
    Cancelled,   // Cancelled by caller (terminal)
    Superseded,  // Replaced by a newer dispatch epoch (terminal)
    DeadLetter,  // Permanently failed (terminal)
}
```

`Acked` is a dispatch state, not a success state. Read `RunRecord.status`,
`RunRecord.waiting`, and `RunRecord.outcome` to decide whether the agent
succeeded, failed, or is still waiting.

### RunDispatchResult

The queue record stores a compact projection of the runtime result so operators
can debug a consumed dispatch without treating queue status as business status:

```rust
pub struct RunDispatchResult {
    pub run_id: String,
    pub dispatch_instance_id: String,
    pub status: RunStatus,
    pub termination: Option<TerminationReason>,
    pub response: Option<String>,
    pub error: Option<String>,
}
```

### RunRequestOrigin

```rust
pub enum RunRequestOrigin {
    User,      // HTTP API, SDK
    A2A,       // Agent-to-Agent protocol
    Internal,  // Child run notification, handoff
}
```

### MailboxStore Trait

`MailboxStore` defines the persistent queue interface:

- **enqueue** -- persist a dispatch, assign the current dispatch epoch, reject duplicate `dedupe_key`
- **claim** -- atomically claim up to N `Queued` dispatches for a mailbox (lease-based)
- **claim_dispatch** -- claim a specific dispatch by ID (for inline streaming)
- **ack** -- mark a dispatch as `Acked` (validates claim token)
- **nack** -- return a dispatch to `Queued` for retry, or `DeadLetter` if max attempts exceeded
- **cancel** -- cancel a `Queued` dispatch
- **extend_lease** -- heartbeat to extend an active claim
- **interrupt** -- atomically bump the dispatch epoch, supersede stale `Queued` dispatches, return the active `Claimed` dispatch for cancellation

Implementations must guarantee: durable enqueue, atomic claim (exactly one winner), claim token validation on ack/nack, and atomic interrupt with a dispatch epoch bump.

## Waiting Runs and Run Control

Suspension is a non-terminal state of the same run. A waiting run persists
`RunWaitingState` on `RunRecord`:

```rust
pub struct RunWaitingState {
    pub reason: WaitingReason,
    pub ticket_ids: Vec<String>,
    pub tickets: Vec<RunWaitingTicket>,
    pub since_dispatch_id: Option<String>,
    pub message: Option<String>,
}
```

When a run waits for approval or input, the current dispatch is acked and the
thread keeps `open_run_id`. A later approval or user input creates another
dispatch for the same `run_id`.

`RunControlService` is the server-side control surface for this flow:

- `get_active_run` reads the thread's active/open run projection.
- `decide` records a tool-call decision and resumes the waiting run.
- `cancel_run` terminates a run.
- `interrupt_thread` interrupts current work for a thread.
- `inject_user_input` and `inject_run_input` append user input and can resume
  the same open run.

This is the API layer used by Web/IDE-style frontends to reconnect, approve,
cancel, interrupt, or steer a run without inventing protocol-specific state.

### MailboxInterrupt

When a new high-priority request arrives for a thread that already has queued or running work:

```rust
pub struct MailboxInterrupt {
    pub new_dispatch_epoch: u64,
    pub active_dispatch: Option<RunDispatch>,
    pub superseded_count: usize,
}
```

The caller cancels the `active_dispatch`'s runtime run if present, ensuring the
new request takes priority.

## See Also

- [Run Lifecycle and Phases](/run-lifecycle-and-phases/) -- how suspension bridges run and tool-call layers
- [Enable Tool Permission HITL](/how-to/enable-tool-permission-hitl/) -- practical setup guide
- ADR-0022: Run Dispatch Data Model -- durable run/dispatch model
