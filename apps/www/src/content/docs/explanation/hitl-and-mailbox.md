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

## Agent message routing

Awaken uses two message paths and keeps them explicit:

| Path | Code surface | Use when | Delivery boundary |
|---|---|---|---|
| Live child inbox | `BackgroundTaskManager::spawn_agent_with_context(...)` plus `SendMessageTool` with `relation: "child"` | Parent and background child agent run in the same process and need low-latency messages | In-process inbox; task id/name must resolve to a live child task on the owning thread |
| Durable mailbox | `Mailbox::submit(...)`, `submit_background(...)`, HTTP `/v1/threads/:id/mailbox`, A2A `message:send`, MCP HTTP mailbox tools, or a host-provided `DurableMessageSink` for `SendMessageTool` `parent` / `agent` | Agents, protocols, or workers may be on different threads, processes, or replicas | Persistent `RunDispatch`; claimed by one mailbox worker with leases, retries, and recovery |

Internal background-agent messages stay on the live inbox so they do not pay durable queue cost. External protocol messages and cross-thread agent messages enter the mailbox so distributed workers can claim and execute them safely. `SendMessageTool` does not invent a third transport: `child` routes to the manager inbox, while `parent` and `agent` require the host to provide a durable sink that maps to mailbox dispatch or another persistent transport.

## Distributed dispatch guarantees

The mailbox is the distributed processing boundary. It separates request storage (`RunRecord.request` plus thread message logs) from delivery (`RunDispatch`) so any worker can reconstruct the activation after claiming a dispatch. Correct stores must provide durable enqueue, atomic claim with a single winner, claim-token validation, lease extension, lease recovery, interrupt epoch bumps, and queue/result projection updates. NATS mailbox uses JetStream/KV for multi-replica ownership and wakeups; SQLite mailbox is durable but single-node; in-memory mailbox is process-local.

## Pending message steering

When the mailbox is built with `Mailbox::new_with_pending_thread_run_store(...)`, user messages are first staged as `PendingMessageRecord` values in the same backend that owns thread messages and run records. A pending record is delivered but not yet appended to committed history. The runtime freezes pending records at an explicit boundary, appends the selected messages, and updates the `RunRecord` input in one backend transaction.

`DeliveryMode` controls when and how a pending message is consumed:

| Field | Code behavior |
|---|---|
| `boundary` | `Interrupt`, `NextStep`, `OnNaturalEnd`, `ResumeInput`, or `NewRun`. Earlier boundaries can fall through to later boundaries through `DeliveryBoundary::eligible_at`, except `ResumeInput`, which is exact-match only. |
| `granularity` | `Batch` consumes all eligible records; `One` stops after the first eligible record. |
| `barrier` | Prevents later pending records from being skipped across the barrier; foreground interrupt preflight reports `DeliveryBlockedByBarrier` before cancelling the active run. |
| `target_run_id` | Restricts active-run delivery to a specific run. `submit_live_then_queue(..., expected_run_id)` also uses this to avoid steering a stale run. |
| `fallback_to_new_run` | Allows active-run pending to become `NewRun` work if the target run ends first. Targeted live steering uses `false`; ordinary queued records default to `true`. |

The mailbox exposes checked pending-edit operations for hosts that present a review queue before freeze:

- `update_pending_message_checked(thread_id, pending_id, expected_revision, message)` edits message content under an optimistic record revision.
- `retract_pending_message_checked(thread_id, pending_id, expected_revision)` removes a pending entry before it is consumed.
- `reorder_pending_messages_checked(thread_id, expected_queue_revision, ordered_pending_ids)` changes pending order under an optimistic queue revision.

After a pending record is frozen/consumed these edits fail instead of rewriting committed history. Freeze retries use pending selection conflicts and message-version checks so concurrent edits, reorders, or retracts do not leave phantom trigger ids in `RunRecord.input`.

### Choosing the handling mode

HTTP `POST /v1/threads/:id/messages` and `POST /v1/runs/:id/inputs` map to `RunControlService` input modes:

| Mode | Effect |
|---|---|
| `queue` | Create a durable mailbox dispatch. With a pending store, submit appends and freezes `NewRun` pending atomically while preparing the dispatch. |
| `live_then_queue` / `steer` | Try to steer the active run first. With a pending store, messages are staged as targeted `NextStep` pending and the active run receives `PendingBoundaryWake`; if no local or remote subscriber accepts the wake, the pending append is cleaned up and the request falls back to a durable dispatch. |
| `interrupt_then_queue` | Bump the dispatch epoch, supersede queued work, cancel the active run, then queue the new input. Foreground interrupt preflight refuses to cancel when an earlier pending barrier blocks delivery. |
| `resume_open_run` | Continue the thread's reusable waiting run. Fresh user input is staged as `ResumeInput` targeted to that run so unrelated `NewRun` pending stays queued for later. |

At runtime boundaries, `MailboxPendingBoundaryHandler` lets the loop stage and freeze additional pending messages for `NextStep`, `OnNaturalEnd`, or other supported boundaries. This is the mechanism that makes dynamic steering editable and crash-safe while still allowing distributed workers to process the final dispatch.

### Code references

The repository keeps executable coverage for these paths. Use these tests as the closest working examples when wiring a host integration:

- `crates/awaken-server/src/mailbox/pending_delivery_tests.rs` — pending edit, reorder, retract, and freeze.
- `crates/awaken-server/src/mailbox/tests.rs` — local and remote `submit_live_then_queue` steering.
- `crates/awaken-server/src/routes_test.rs` — HTTP `mode: "steer"` alias parsing.

Pending review queue before freeze (production submits stage pending through mailbox submit paths; the test uses the internal `deliver` helper to set up the same state):

```rust
use awaken::contract::message::{Message, pending_queue_revision};

let pending = pending_store
    .load_pending_message_records("thread-edit-pending")
    .await?;
let queue_revision = pending_queue_revision(&pending);

mailbox
    .update_pending_message_checked(
        "thread-edit-pending",
        &pending[0].pending_id,
        Some(pending[0].revision),
        Message::user("edited").with_id(pending[0].pending_id.clone()),
    )
    .await?;

mailbox
    .reorder_pending_messages_checked(
        "thread-edit-pending",
        Some(queue_revision),
        &[pending[1].pending_id.clone(), pending[0].pending_id.clone()],
    )
    .await?;

mailbox
    .retract_pending_message_checked(
        "thread-edit-pending",
        &pending[1].pending_id,
        Some(pending[1].revision),
    )
    .await?;
```

Steer an active run first, then fall back to queue if live delivery is unavailable:

```rust
let result = mailbox
    .submit_live_then_queue(
        RunActivation::new("thread-live-steer", vec![Message::user("live steer")])
            .with_agent_id("agent"),
        Some(active_run_id),
    )
    .await?;

assert_eq!(result.status, MailboxDispatchStatus::Running);
assert_eq!(result.run_id, active_run_id);
```

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

`MailboxStore` defines the persistent queue interface. The trait surface lives at `crates/awaken-server-contract/src/contract/mailbox.rs`:

**Enqueue / claim / lifecycle:**

- **enqueue** -- persist a dispatch, assign the current dispatch epoch, reject duplicate `dedupe_key`
- **claim** -- atomically claim up to N `Queued` dispatches for a mailbox (lease-based)
- **claim_dispatch** -- claim a specific dispatch by ID (for inline streaming)
- **ack** -- mark a dispatch as `Acked` (validates claim token)
- **nack** -- return a dispatch to `Queued` for retry
- **dead_letter** -- mark a dispatch as `DeadLetter` (permanently failed)
- **cancel** -- cancel a `Queued` dispatch
- **extend_lease** -- heartbeat to extend an active claim
- **interrupt** -- atomically bump the dispatch epoch, supersede stale `Queued` dispatches, return the active `Claimed` dispatch for cancellation
- **supersede_claimed** -- replace a `Claimed` dispatch when a newer epoch arrives

**Runtime projection (so operators can see what happened):**

- **record_dispatch_start** -- mark `run_status = Running` for the projected runtime view
- **record_run_result** -- write the compact `RunDispatchResult` projection (separate from `ack` — the ack only closes the queue lifecycle, not the business outcome)

**Inspection:**

- **load_dispatch** -- fetch a single dispatch by ID
- **list_dispatches** -- list dispatches for a thread (paged, filterable)
- **reclaim_expired_leases** -- recover dispatches whose lease expired without ack

Implementations must guarantee: durable enqueue, atomic claim (exactly one winner), claim-token validation on ack / nack / dead_letter, and atomic interrupt with a dispatch epoch bump. The two-track design (queue lifecycle vs runtime projection) lets operators debug a consumed dispatch without conflating `Acked` queue state with run success.

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

- [Run Lifecycle and Phases](/awaken/explanation/run-lifecycle-and-phases/) -- how suspension bridges run and tool-call layers
- [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/) -- practical setup guide
- ADR-0022: Run Dispatch Data Model -- durable run/dispatch model
