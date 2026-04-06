# HITL and Mailbox

This page explains how Awaken handles human-in-the-loop (HITL) interactions through tool call suspension and the mailbox queue that manages run requests.

## SuspendTicket

When a tool call needs external approval or input, it produces a `SuspendTicket`:

```rust,ignore
pub struct SuspendTicket {
    pub suspension: Suspension,
    pub pending: PendingToolCall,
    pub resume_mode: ToolCallResumeMode,
}
```

**suspension** -- the external-facing payload describing what input is needed:

```rust,ignore
pub struct Suspension {
    pub id: String,             // Unique suspension ID
    pub action: String,         // Action identifier (e.g., "confirm", "approve")
    pub message: String,        // Human-readable prompt
    pub parameters: Value,      // Action-specific parameters
    pub response_schema: Option<Value>,  // JSON Schema for expected response
}
```

**pending** -- the tool call projection emitted to the event stream so the frontend knows which call is waiting:

```rust,ignore
pub struct PendingToolCall {
    pub id: String,        // Tool call ID
    pub name: String,      // Tool name
    pub arguments: Value,  // Original arguments
}
```

`suspension.id` and `pending.id` serve different purposes:

- `suspension.id` is the external-facing suspension key. Protocols that expose
  approval or interrupt IDs should use this value.
- `pending.id` is the underlying runtime tool-call ID.

The runtime can resolve resume decisions against either identifier.

Why both are supported:

- Internal runtime control paths naturally hold the stable tool-call ID.
- Protocols and frontends often expose the current suspension as a separate
  approval / interrupt object, so they naturally send back the external
  `suspension.id`.
- A single tool call can suspend multiple times over its lifetime; each
  suspension gets a fresh external key while the underlying tool-call ID stays
  stable.

**resume_mode** -- how the agent loop should handle the decision when it arrives.

## ToolCallResumeMode

```rust,ignore
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

```rust,ignore
pub enum ResumeDecisionAction {
    Resume,  // Proceed with the tool call
    Cancel,  // Cancel the tool call
}
```

Each decision carries an action. `Resume` continues execution according to the `ToolCallResumeMode`. `Cancel` transitions the tool call to `ToolCallStatus::Cancelled`.

## ToolCallResume

The full resume payload:

```rust,ignore
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
5. If the current step reaches quiescence with unresolved suspended calls, the
   run transitions to `RunStatus::Waiting`.
6. A frontend presents the approval prompt to the user.
7. The user submits a `ToolCallResume` with `ResumeDecisionAction::Resume` or `Cancel`.
8. On `Resume`, the tool call replays and executes normally.
9. On `Cancel`, the tool call is cancelled and the run may continue with remaining calls.

`RunStatus` is coarse. Today the runtime does not expose a separate
`Running+Waiting` state. In sequential execution, a suspension usually moves the
run to `Waiting` immediately because later calls have not started yet. In
parallel modes, already-started calls in the current batch are allowed to
finish first; only then, if suspended calls remain and no runnable work is
left, does the run move to `Waiting`.

## Mailbox Architecture

The mailbox provides a persistent queue for all run requests. Every run -- streaming, background, A2A, internal -- enters the system as a `MailboxJob`.

### MailboxJob

```rust,ignore
pub struct MailboxJob {
    // Identity
    pub job_id: String,          // UUID v7
    pub mailbox_id: String,      // Thread ID (routing anchor)

    // Request payload
    pub agent_id: String,
    pub messages: Vec<Message>,
    pub origin: MailboxJobOrigin,
    pub sender_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub request_extras: Option<Value>,

    // Queue semantics
    pub priority: u8,            // 0 = highest, 255 = lowest, default 128
    pub dedupe_key: Option<String>,
    pub generation: u64,

    // Lifecycle
    pub status: MailboxJobStatus,
    pub available_at: u64,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub last_error: Option<String>,

    // Lease
    pub claim_token: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<u64>,

    // Timestamps
    pub created_at: u64,
    pub updated_at: u64,
}
```

### MailboxJobStatus

```text
Queued --claim--> Claimed --ack--> Accepted (terminal)
  |                  |
  |               nack(retry) --> Queued (attempt_count++, available_at = retry_at)
  |                  |
  |               nack(permanent) --> DeadLetter (terminal)
  |
  |-- cancel --> Cancelled (terminal)
  +-- interrupt(generation bump) --> Superseded (terminal)
```

```rust,ignore
pub enum MailboxJobStatus {
    Queued,      // Waiting to be claimed
    Claimed,     // Claimed by a consumer, executing
    Accepted,    // Successfully processed (terminal)
    Cancelled,   // Cancelled by caller (terminal)
    Superseded,  // Replaced by a newer generation (terminal)
    DeadLetter,  // Permanently failed (terminal)
}
```

### MailboxJobOrigin

```rust,ignore
pub enum MailboxJobOrigin {
    User,      // HTTP API, SDK
    A2A,       // Agent-to-Agent protocol
    Internal,  // Child run notification, handoff
}
```

### MailboxStore Trait

`MailboxStore` defines the persistent queue interface:

- **enqueue** -- persist a job, auto-assign generation, reject duplicate `dedupe_key`
- **claim** -- atomically claim up to N `Queued` jobs for a mailbox (lease-based)
- **claim_job** -- claim a specific job by ID (for inline streaming)
- **ack** -- mark a job as `Accepted` (validates claim token)
- **nack** -- return a job to `Queued` for retry, or `DeadLetter` if max attempts exceeded
- **cancel** -- cancel a `Queued` job
- **extend_lease** -- heartbeat to extend an active claim
- **interrupt** -- atomically bump generation, supersede stale `Queued` jobs, return the active `Claimed` job for cancellation

Implementations must guarantee: durable enqueue, atomic claim (exactly one winner), claim token validation on ack/nack, and atomic interrupt with generation bump.

### MailboxInterrupt

When a new high-priority request arrives for a thread that already has queued or running work:

```rust,ignore
pub struct MailboxInterrupt {
    pub new_generation: u64,       // Generation after bump
    pub active_job: Option<MailboxJob>,  // Currently running job to cancel
    pub superseded_count: usize,   // Queued jobs superseded
}
```

The caller cancels the `active_job`'s runtime run if present, ensuring the new request takes priority.

## See Also

- [Run Lifecycle and Phases](./run-lifecycle-and-phases.md) -- how suspension bridges run and tool-call layers
- [Enable Tool Permission HITL](../how-to/enable-tool-permission-hitl.md) -- practical setup guide
