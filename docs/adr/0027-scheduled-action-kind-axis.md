# ADR-0027: A Plugin-Owned Scheduled-Action-Kind Capability Axis

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0004, ADR-0020
- Amended: 2026-10-09 (reconciled with ADR-0004: its `scheduled_actions` bound
  is this `action_kinds` axis)

## Context

ScheduledAction (ADR-0020) currently carries a resolved tool as its pending
action, so it is bounded by the existing tool axis. But G30 names *scheduled
actions* as their own id-bearing capability axis, and the scenarios require that a
scheduled-action kind owned by a plugin **not selected** for a run fails closed —
the kind is absent from the resolved environment (RS-SCH-005). That axis did not
exist in `CapabilityBound`.

## Decision

### D1: `action_kinds` is a first-class capability axis

`CapabilityBound`, `Contributions`, and `ResolvedExecutionEnv` gain
`action_kinds: Vec<String>`. `enforce_bound` rejects an action kind outside the
plugin's declared bound (`BoundViolation::ActionKind`), and `merge` collects kinds
in dependency order, rejecting cross-plugin duplicates
(`MergeError::DuplicateActionKind`) — exactly the rules the tool axis already
follows (ADR-0004's `contributions ⊆ bound`, unique ids).

### D2: An unselected plugin contributes nothing, so its kinds are absent

Plugins not in the run's `plugin_ids` are inert (they never resolve into the env),
so their action kinds are simply not in `ResolvedExecutionEnv.action_kinds`.
`permits_action_kind(kind)` is the fail-closed check: a kind absent from the
resolved env is not permitted. No deny-list is needed — absence is denial.

### D3: A kind-based Schedule validates against the env

`GateOutcome::Schedule` gains an optional `action_kind`. When set, the engine
checks `env.permits_action_kind(kind)` before committing the ScheduledAction; an
unpermitted kind ends the run closed with `CapabilityBound`, committing no ticket
and running no action (RS-SCH-005). `action_kind = None` is the ordinary
tool-backed scheduled action (ADR-0020), unchanged.

## Consequences

- Scheduled actions have their own bounded axis, matching G30; a plugin-owned kind
  cannot be staged unless its plugin is selected and within its declared bound.
- The tool-backed scheduled action is unchanged; the new axis is additive.
- The kind names *which* plugin-owned action; resolving a kind to its executable
  behavior (the action runner) is the remaining wiring, deferred until a concrete
  non-tool action exists.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G30 (every id-bearing kind is bounded).
- ADR-0004 — the capability-bound model this axis extends.
- ADR-0020 — ScheduledAction, which this bounds by kind as well as by tool.

## Amendment 2026-10-09: reconciled with ADR-0004

The Context sentence "That axis did not exist in `CapabilityBound`" describes the
code at the time, not the ADR corpus: [ADR-0004](0004-plugin-factory-contributions-and-capability-bound.md)
Amendment A1.2 had already required a bound on `scheduled_actions`. This ADR is
the implementation of that requirement under the name `action_kinds`; there is
one axis, not two. Evidence: `CapabilityBound.action_kinds: IdBound`
(`crates/contract/awaken-runtime-contract/src/plugin/capability.rs:79`) and its
check at `capability.rs:166`. Read `scheduled_actions` in ADR-0004 as `action_kinds`.
