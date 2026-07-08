# ADR-0044: Remote Hand — Tool Execution Over a Channel

- Status: Accepted
- Date: 2026-07-08
- Implemented: 2026-07-08 — `awaken-tool-relay` (`RemoteToolExecutor`, `serve_hand`,
  `HandSession`, wire types); kernel seam in `awaken-runtime` (`LocalToolExecutor`,
  `RuntimeRunContext::tool_executor`); host seam `SharedHost::with_remote_hand`;
  served `AWAKEN_MODEL_MODE=remote-hand` mode + `managed_remote_hand_e2e.mjs`
  (a served run runs `bash` on a hand and the output round-trips); guardrail G33.
  Deferred (ADR-0046): per-run hand placement, pooled workers, out-of-process/
  networked hands (the served slice runs the hand as an in-process framed task).
- Depends on: [ADR-0007](0007-runtime-owns-tool-execution.md) (this is the "future
  ADR" that D3 deferred), [ADR-0041](0041-sandbox-execution-environment-provider.md)
  (process-level provider seam that hosts a hand)
- Relates to: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (kernel is
  placement-agnostic), [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  (resolved-secret boundary)
- Prior art: awaken-next ADR-0049/0050/0086 (neutral brain/hand interface) —
  adopted in spirit, **not** its `ExecRequest`/`Capability`/`FqId`/`Verb` wire
  vocabulary (see D2).
- Topology and placement are deliberately **out of this ADR**:
  [ADR-0045](0045-connection-plan-and-network-topology.md) owns how the two ends
  meet; a later ADR owns where the hand runs.

## Context

ADR-0007 fused tool execution into the runtime process ("the runtime owns tool
execution in-process") and **retired** the `ExecutionBackend`/`BackendProfile`
placement seam (G7), explicitly deferring remote execution "to a future ADR that
introduces it with a concrete driver and tests." This is that ADR.

The pieces to reach a split are already in place and unused:

- `ToolExecutor` (`awaken-runtime-contract/src/tool.rs:90`) — a port whose own
  doc reads *"Where the call runs is hidden behind this port and owned by its
  implementer (the orchestration layer above), not the runtime core."* It has no
  implementor today; the loop calls `RawTool::invoke` directly.
- `ToolCall` / `ToolOutput` — the neutral, serializable tool value objects the
  kernel already speaks.
- `awaken-agent-channel` — a duplex byte-channel seam (`AgentChannel`) already
  used to drive a remote **brain** (ACP CLI). No remote **hand** consumes it yet.
- `awaken-provisioning-contract` — `Sandbox::spawn` + serializable `SandboxHandle`
  + `renew_lease`/`adopt`: a leased remote process that can host a hand.
- G26 already requires *"indeterminate remote execution is explicit"* — written
  in anticipation of exactly this seam.

So "brain–hand separation" (手腦分離) is not a new architecture. It is: **make the
one already-defined `ToolExecutor` port real, with an in-process degenerate case
and a remote case, and prove the isolation invariant by dependency guardrail.**

Note this is the *remote hand* flavor. Remote *brain* already ships two ways
(run-level ACP, sub-agent-level A2A delegation); neither is touched here.

## Decision

### D1: The brain–hand seam is the existing `ToolExecutor` port, not a new one

The kernel loop stops calling `RawTool::invoke` directly and instead calls an
injected `ToolExecutor::invoke(&ToolCall) -> Result<ToolOutput, ToolError>` for an
already-authorized, already-gated call. Two implementors:

- `LocalToolExecutor` — the **degenerate case**: looks up the `RawTool` by id in
  the runtime's own registry and invokes it in-process. This is byte-for-byte
  today's behavior and remains the default. A run with no remote hand pays
  nothing; the single-machine path is unchanged.
- `RemoteToolExecutor` — frames the call onto a channel to a hand and awaits the
  result (D2/D3).

Selection of which executor a run uses is host wiring driven by the resolved
snapshot's `BackendProfile`; the kernel never learns placement. This keeps
ADR-0034's "kernel is placement/scheduling-agnostic" intact — the port is the
only thing that changed, from "unused" to "used."

### D2: The wire is the domain's own `ToolCall`/`ToolOutput`, plus a thin envelope

The brain–hand protocol carries `ToolCall` in and `ToolOutput` out, wrapped in a
minimal envelope:

```text
HandRequest  { correlation_id, catalog_fingerprint, deadline_unix_ms, call: ToolCall }
HandReply    { correlation_id, result: HandResult }
HandResult   = Ok(ToolOutput) | Err(ToolError) | Indeterminate
```

We **do not** introduce a parallel execution vocabulary (`ExecRequest`,
`Capability`, `FqId`, `Verb`, `ExecStatus`) the way awaken-next did. The kernel
already has the neutral value objects; reusing them is fewer types, no adapter
between "tool call" and "exec request," and no risk of the two drifting.
`catalog_fingerprint` lets the hand fail closed if its tool catalog does not match
the run's resolved snapshot (mirrors G4 fingerprint discipline on the hand side).

Serialization is serde/JSON of `ToolCall`/`ToolOutput`; framing is a
length-delimited codec over an established channel (D3). No transport is chosen
here.

### D3: The hand is a value-returning function, not a service

The hand side is one thin server, `serve_hand(channel, registry)`, that loops
`decode HandRequest → RawTool::invoke → encode HandReply`. Its dependency surface
is **exactly** a `RawTool` registry (e.g. from `awaken-ext-builtin-tools`) and the
channel. It links:

- **no** `LlmExecutor` / model client,
- **no** `CommitCoordinator` / durable write path,
- **no** `*-store`.

It writes no durable runtime truth; it returns serializable data only. The brain
converts a `HandReply` into the tool result and commits it through the existing
`CommitCoordinator` (G1). This is the isolation invariant of 手腦分離, and it is
enforced mechanically, not by convention — see G33.

The hand runs over an **abstract `Channel`** (`awaken-agent-channel` /
`awaken-connection`); it is transport- and topology-blind. In-process, Unix
socket, TCP, and NATS-relay hands are the same `serve_hand`.

### D4: Indeterminate results are explicit and idempotently resolvable

A remote call can fail after the tool ran but before the reply arrived (dropped
channel, hand death). `RemoteToolExecutor` surfaces this as
`HandResult::Indeterminate`, never a silent failure or a fabricated success
(satisfies G26). Resolution:

- Requests carry a stable `correlation_id`; the hand keeps a small in-flight/
  recently-completed ledger keyed by it, so a re-drive returns the original
  `ToolOutput` instead of running the effect twice.
- The brain's re-drive path is the existing dispatch retry (ADR-0015 budget); no
  new retry machinery.

This makes the remote hand safe under the crash-retry budget without a
distributed transaction.

### D5: This ADR introduces the seam and framing only

Scope fence, to keep one decision per ADR:

- **How** the two ends meet (address, dial direction, broker) → ADR-0045.
- **Where** the hand process is placed and leased → a later ADR over the existing
  `SandboxProvider`/`SandboxHandle`/`renew_lease`.

ADR-0044 delivers: the `ToolExecutor` wiring, the `HandRequest`/`HandReply`
envelope, `serve_hand`, and one working transport arm (in-process duplex, then
Unix) sufficient to prove the split end to end.

## Development-Ready Design (G14)

| Required item | This ADR |
|---|---|
| Bounded context | Port stays in **Runtime Core** (`ToolExecutor`); remote impl + `serve_hand` are **Dispatch/Server** |
| Model element | Value objects `HandRequest` / `HandReply` / `HandResult`; reuses `ToolCall` / `ToolOutput` |
| Port / repository | `ToolExecutor` (existing, promoted to used); `HandServer` (`serve_hand`) |
| Owning crate | new `awaken-tool-relay` (client `RemoteToolExecutor` + `serve_hand`); port unchanged in `awaken-runtime-contract` |
| Guardrail + enforcer | **G33** (new): the hand build links no model/commit/store — `deny.toml` dependency-direction + `scripts/ci/check_crate_boundaries.py` |
| First vertical slice | `RemoteToolExecutor` ⇄ in-process duplex ⇄ `serve_hand` running only `bash`; kernel unchanged; one e2e runs `bash` out-of-process and commits its `ToolOutput` in the brain, then a dropped-channel test asserts `Indeterminate` + idempotent re-drive |

## Consequences

- G7 stays a tombstone; this ADR does **not** revive `ExecutionBackend`/
  `BackendProfile`. It promotes the narrow `ToolExecutor` port instead — the
  placement concern returns as a single call-site port, not a god-seam.
- New guardrail **G33** is added to [INVARIANTS.md](../INVARIANTS.md) as **Active**
  in the same change that lands `awaken-tool-relay` (a Target guardrail must gain
  its enforcer in the change that builds its subsystem).
- The kernel diff is one line of intent: `RawTool::invoke(call)` becomes
  `tool_executor.invoke(&call)`; `LocalToolExecutor` preserves behavior.
- `ToolExecutor`'s doc comment stops being aspirational.
- Opens the door for ADR-0045 (topology) and the placement ADR to plug in without
  touching the kernel or the wire.

## References

- [ADR-0007](0007-runtime-owns-tool-execution.md) — D3 deferral this ADR fulfills;
  G7 tombstone.
- [neutral-waist.md](../design/neutral-waist.md) — `ToolExecutor` in the port
  table; "no new execution-transport framework before a future ADR."
- [ADR-0045](0045-connection-plan-and-network-topology.md) — the transport/topology
  this seam runs over.
- [INVARIANTS.md](../INVARIANTS.md) — G1 (commit boundary), G26 (indeterminate
  remote execution explicit), G33 (new: hand isolation).
