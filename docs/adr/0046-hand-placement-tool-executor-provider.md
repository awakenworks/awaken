# ADR-0046: Hand Placement — the `ToolExecutorProvider` Seam

- Status: Proposed
- Date: 2026-07-09
- Depends on: [ADR-0044](0044-remote-hand-tool-executor-over-a-channel.md) (the
  `ToolExecutor` port and `LocalToolExecutor`/`RemoteToolExecutor` this ADR
  selects between), [ADR-0045](0045-connection-plan-and-network-topology.md) (the
  `ConnectionPlan` a placement resolves to), [ADR-0041](0041-sandbox-execution-environment-provider.md)
  (the `SandboxProvider` that owns a hand process's lifecycle — placement selects,
  the sandbox provider leases)
- Relates to: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (the kernel
  stays placement/scheduling-agnostic — axis separation), G16, G33
- Fulfils: the placement fence ADR-0044 D5 and ADR-0045 D-final deferred ("where
  the hand process is placed" was named as a later ADR)

## Context

ADR-0044 introduced the `ToolExecutor` port with two implementors —
`LocalToolExecutor` (in-process, the degenerate default) and `RemoteToolExecutor`
(frames a call onto a `ConnectionPlan` channel to a hand). ADR-0045 made the
channel's topology a serializable `ConnectionPlan`. Both ADRs deliberately fenced
out **which** executor a given run uses and **where** its hand lives:

> ADR-0044 D5: *Where the hand process is placed and leased → a later ADR over
> the existing `SandboxProvider`.*

Today that selection is a **static branch in host wiring**: `awaken-server-local`
reads the resolved snapshot and constructs either a `LocalToolExecutor` or a
`RemoteToolExecutor` with a hardcoded plan (`AWAKEN_MODEL_MODE=remote-hand`). This
is fine for a single fixed deployment, but there is no seam a host can implement
to say "for *this* run, put the hand *there*." A self-hosting operator who wants
brain and hands split across machines — a light brain box and a separate,
resource-heavy hand box (a GPU host, a network-isolated tool sandbox) — has to
fork the wiring rather than inject a policy.

The forces:

- The kernel must remain placement-agnostic (ADR-0034): it invokes
  `tool_executor.invoke(&call)` and must never learn *how* the executor was
  chosen.
- The two existing value objects — `ToolExecutor` (what runs the call) and
  `ConnectionPlan` (how the ends meet) — are enough; a placement decision is just
  "pick a `ToolExecutor` for a run," possibly by resolving a `ConnectionPlan`.
- Placement is a genuine open, self-hosting concern; it must be justified and
  fully exercised by the open runtime on its own, not left as an empty extension
  point for some downstream to fill.

## Decision

### D1: Placement is one call-site port, `ToolExecutorProvider`, not a god-seam

We add a single port that turns a run into the `ToolExecutor` it should use:

```rust
pub trait ToolExecutorProvider: Send + Sync {
    /// Choose the tool executor for this run. May resolve a `ConnectionPlan`
    /// and return a `RemoteToolExecutor`, or return the in-process default.
    fn provide(&self, activation: &RunActivation) -> Arc<dyn ToolExecutor>;
}
```

This mirrors the runtime's existing provider seams (`ExecutorProvider`,
`SandboxProvider`) exactly: a run-scoped factory the host wires once. The kernel
diff is nil — it still calls `tool_executor.invoke(&call)`; only *the construction
of that executor* moves from a static wiring branch behind this port. This
honours ADR-0044's own guidance that "the placement concern returns as a single
call-site port, not a god-seam" — we do **not** revive the G7
`ExecutionBackend`/`BackendProfile` tombstone.

### D2: The open default is a real, config-driven placement — not a stub

The default implementation the runtime ships, `ConfigToolExecutorProvider`, does
real work: it reads a configured **placement policy** (a set of named worker
endpoints, each an ADR-0045 `ConnectionPlan`, plus a match rule) and, per run,
either

- returns `LocalToolExecutor` (no placement configured — byte-for-byte today's
  single-machine path, unchanged and free), or
- selects a configured worker plan and returns a `RemoteToolExecutor` over it.

This is the **self-hosted brain–hand split**: an operator declares "hands run on
`worker-a` (a Unix/TCP `ConnectionPlan`)" in config, and the runtime places them
there — no fork. The seam is therefore justified and exercised entirely by the
open runtime; it is not an empty hook. The placement policy is static
configuration (a fixed worker set), which is all a self-hosted deployment needs.

### D3: The port is placement-*mechanism*-agnostic

`ToolExecutorProvider::provide` takes a `RunActivation` and returns a
`ToolExecutor`. It carries **no** vocabulary about *how* the choice is made — no
worker registry, no lease, no pool, no scheduler type crosses the port. A host
that needs a richer policy than static config (for example, choosing a worker
dynamically at dispatch time) supplies its **own** `ToolExecutorProvider`
implementation; the runtime neither defines nor names that policy. This keeps the
neutral-vocabulary invariant (G16): the runtime's port surface says only "given a
run, here is its tool executor," and every deployment-specific placement strategy
lives behind the port, invisible to the kernel.

### D4: No new value objects; reuse `ToolExecutor` + `ConnectionPlan`

The provider composes the two things ADR-0044/0045 already defined. It introduces
one trait and one config-shaped policy value (`PlacementPolicy` — a list of
`{ name, match, ConnectionPlan }`), and nothing else. There is no parallel
"assignment"/"scheduling" vocabulary. A hand a provider returns still links no
model/commit/store — the isolation invariant **G33** is unchanged and continues to
be enforced mechanically on the hand crate.

### D5: Scope fence — selection only

To keep one decision per ADR:

- **Which** executor a run uses → this ADR (`ToolExecutorProvider`).
- **How** the two ends meet (address, dial direction, broker) → ADR-0045
  (`ConnectionPlan`, which a placement resolves to).
- **The hand process's lifecycle** (spawn, lease, renew, reclaim) → ADR-0041
  (`SandboxProvider`/`renew_lease`). A placement *selects* an endpoint; it does
  not own the process. A provider that leases a fresh hand composes a
  `SandboxProvider` behind its own `provide` — that composition is the provider's
  concern, not a new runtime port.

## Development-Ready Design (G14)

| Required item | This ADR |
|---|---|
| Bounded context | Port in **Runtime Core** (`awaken-runtime-contract`); default impl in **Dispatch/Server** (`awaken-server-local`) |
| Model element | `ToolExecutorProvider` (port); `PlacementPolicy` (config value: named `ConnectionPlan`s + match rule); reuses `ToolExecutor`, `ConnectionPlan`, `RunActivation` |
| Port / repository | `ToolExecutorProvider::provide(&RunActivation) -> Arc<dyn ToolExecutor>` |
| Owning crate | port in `awaken-runtime-contract`; `ConfigToolExecutorProvider` default in `awaken-server-local` (composes `awaken-connection-plan` + `awaken-tool-relay`) |
| Guardrail + enforcer | **Reuses G33** (a provider-returned hand links no model/commit/store) and **G16** (the port carries no placement-mechanism/product vocabulary — enforced by the existing vocabulary deny-list + `check_crate_boundaries.py`). No new guardrail. |
| First vertical slice | `ConfigToolExecutorProvider` reads a two-entry policy: unmatched runs → `LocalToolExecutor` (behaviour unchanged); a matched run → `RemoteToolExecutor` over a configured Unix/TCP `ConnectionPlan`. One e2e: two agents in one server, one placed local and one placed on a separate hand process, both commit identically. The `remote-hand` env mode becomes a one-entry policy expressed through this provider. |

## Consequences

- The static wiring branch in `awaken-server-local` becomes one injected
  `provide` call; the kernel is untouched (ADR-0034 preserved).
- Self-hosting gains real brain–hand placement from config, with no fork: the
  open runtime is the seam's first and complete consumer.
- A host that needs dynamic placement implements its own `ToolExecutorProvider`;
  the runtime exposes the port and stays ignorant of the strategy (G16). No
  placement-strategy vocabulary ever enters the neutral surface.
- Lifecycle stays with `SandboxProvider` (ADR-0041); this ADR does not grow a
  process-management responsibility.
- The three axes remain orthogonal and reference no shared placement type:
  *what runs the call* (`ToolExecutor`, 0044) / *how ends meet* (`ConnectionPlan`,
  0045) / *which executor a run gets* (`ToolExecutorProvider`, this ADR).

## References

- [ADR-0044](0044-remote-hand-tool-executor-over-a-channel.md) — the
  `ToolExecutor` port and the "placement returns as a single call-site port"
  guidance this ADR follows; the D5 fence it fulfils.
- [ADR-0045](0045-connection-plan-and-network-topology.md) — the `ConnectionPlan`
  a placement resolves to.
- [ADR-0041](0041-sandbox-execution-environment-provider.md) — the
  `SandboxProvider` lifecycle seam a leasing provider composes.
- [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) — axis separation; the
  kernel stays placement-agnostic.
- [INVARIANTS.md](../INVARIANTS.md) — G16 (neutral vocabulary), G33 (the hand
  links no model/commit/store).
