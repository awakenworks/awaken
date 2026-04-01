# ADR-0019: Mailbox Architecture — Unified Persistent Run Queue

- **Status**: Accepted
- **Date**: 2026-03-28
- **Depends on**: ADR-0012, ADR-0018
- **Supersedes**: ADR-0017 (partial — execution ownership now fully defined via mailbox)

## Context

Three components share overlapping responsibility for run scheduling:

- **RunDispatcher** — memory-only queue; crash loses all queued runs.
- **MailboxStore** — persistent push/pop, but no consumer drives it.
- **ActiveRunRegistry** — single-process; no distributed coordination possible.

No single component covers persistence, consumption, and distributed claim together.

## Decision

### D1: Single Mailbox service backed by MailboxStore trait

Replace `RunDispatcher`, the old `MailboxStore`, and `ActiveRunRegistry`'s scheduling role with a unified `Mailbox` service in `awaken-server`. `Mailbox` holds `Arc<AgentRuntime>` + `Arc<dyn MailboxStore>` and is the sole entry point for durable multi-request server execution paths (HTTP, A2A, AG-UI, AI-SDK).

### D2: Thread-keyed routing

`mailbox_id = thread_id`. Every run request is routed by thread. Agent-targeted messages auto-generate a `thread_id` at the API level — the queue never sees `agent_id` as an address.

### D3: Write-ahead-log semantics

Every run request persists via `MailboxStore::enqueue()` before dispatch. Crash recovery replays `Queued` entries on startup via `recover()`.

### D4: Six-state lifecycle

```
Queued → Claimed → Accepted | Cancelled | Superseded | DeadLetter
```

Three terminal states (`Accepted`, `Cancelled`, `DeadLetter`). `Superseded` is terminal, triggered by thread interrupt (generation bump). `Claimed → Queued` retry via `nack()` increments `attempt_count`.

### D5: Lease-based distributed claim

Multiple processes compete for jobs through atomic `claim()` on the shared store. Each claim sets `claim_token` + `lease_until`. A renewal heartbeat extends the lease; sweep reclaims expired leases from crashed consumers. No inter-process communication needed — the store is the coordination layer.

### D6: Event-driven dispatch with sweep safety net

Normal path: `enqueue` triggers immediate dispatch if the thread's `MailboxWorker` is idle. Periodic sweep reclaims orphaned leases as a fallback. Per-thread `MailboxWorker` serializes execution (at most one active run per thread).

### D7: Crate placement

- `awaken-contract` — `MailboxJob`, `MailboxJobStatus`, `MailboxJobOrigin`, `MailboxStore` trait.
- `awaken-stores` — `InMemoryMailboxStore`, `FileMailboxStore`, `PostgresMailboxStore`.
- `awaken-server` — `Mailbox` service, `MailboxConfig`, handler integration.

### D8: Mailbox is the control plane; AgentRuntime is the execution plane

`Mailbox` owns the **external lifecycle and ownership** of a run request:

- durable submission (`enqueue` before execution)
- de-duplication / idempotency policy
- thread-level serialization and latest-wins interrupt semantics
- distributed claim, lease renewal, and lease reclamation
- retry scheduling and dead-lettering
- reconnectable event delivery for long-lived server transports
- job-level status query and garbage collection

`AgentRuntime` owns the **internal execution semantics** of a single active run:

- resolve the target agent and create `RunIdentity`
- load thread history and restore execution state
- execute the loop, inference, and tool calls
- suspend / resume semantics for tool calls
- apply incoming resume decisions to active in-memory state
- cooperative cancellation of the currently running loop

This split is intentional:

- `Mailbox` decides **whether a run may start now, who owns it, and what should happen if it is superseded or lost**
- `AgentRuntime` decides **how the accepted run progresses once execution begins**

### D9: Protocol/transport responsibilities stay above both layers

Protocol adapters and transports are responsible for:

- HTTP / SSE / WebSocket / stdio / JSON-RPC / MCP / ACP framing
- encoding `AgentEvent` into protocol-specific messages
- connection-local concerns such as request parsing and response formatting

They may call `Mailbox` or `AgentRuntime` depending on whether they need durable control-plane behavior.

### D10: Use Mailbox when the problem spans requests, processes, connections, or time

Use `Mailbox` for scenarios such as:

| Scenario | Owner | Reason |
| --- | --- | --- |
| Two submissions target the same thread | Mailbox | Thread-level serialization and latest-wins are queue ownership problems |
| A request must survive process crash before execution starts | Mailbox | Requires durable write-ahead submission and recovery |
| A claimed worker crashes mid-run | Mailbox + store | Lease expiry and reclaim are distributed ownership concerns |
| A queued job needs retry / backoff / dead-letter | Mailbox | These are job lifecycle policies, not loop semantics |
| A client disconnects and later reconnects to the same suspended run | Mailbox + transport | Event delivery continuity is outside `AgentRuntime` |
| The system must apply priority / admission control / fairness | Mailbox | Scheduling and backpressure belong to the control plane |

Use `AgentRuntime` directly when the problem is limited to one active in-memory run:

| Scenario | Owner | Reason |
| --- | --- | --- |
| A tool-call decision resumes a suspended run | AgentRuntime | Resume mutates live execution state |
| The loop chooses the next inference/tool step | AgentRuntime | Pure execution semantics |
| A local embedded caller wants to run once with no durability or recovery | AgentRuntime | No queue ownership or reconnect semantics are needed |

Keep the protocol layer separate when the problem is only about representation:

| Scenario | Owner | Reason |
| --- | --- | --- |
| Encode events as SSE or JSON-RPC notifications | Protocol/transport | Formatting concern only |
| Map `AgentEvent` to ACP or MCP payloads | Protocol/transport | Protocol object model concern only |

### D11: Direct `AgentRuntime` use remains valid for ephemeral transports

Not every integration requires `Mailbox`.

Direct `AgentRuntime::run()` is valid when all of the following hold:

- the caller only needs a single in-memory run
- there is no durable queue requirement
- there is no cross-process ownership requirement
- there is no reconnect / retry / dead-letter requirement
- thread-level latest-wins semantics are not delegated to the server

This keeps `AgentRuntime` independently usable for embedded, local, and stdio-style scenarios, while `Mailbox` remains the required path for durable multi-request server execution.

## Consequences

- Crash recovery via lease expiry + startup `recover()` scan.
- Multi-process deployment without inter-process communication; the store is the coordination layer.
- Single submission path for durable mailbox-managed server protocols — `Mailbox::submit()` (streaming) and `Mailbox::submit_background()` (fire-and-forget).
- `RunDispatcher`, old `MailboxStore` trait, and `MailboxEntry` are deleted.
- `AppState` replaces `dispatcher` + `mailbox_store` fields with a single `mailbox: Arc<Mailbox>`.
- ADR-0012's `ThreadRunStore` remains unchanged for checkpoint persistence; `Mailbox` orchestrates around it.
- `AgentRuntime` stays reusable outside the server queue path for ephemeral and embedded execution.
