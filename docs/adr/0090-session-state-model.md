# ADR-0090: One Session State Machine, Everything Else a Projection

- Status: Proposed
- Date: 2026-10-09
- Builds on: ADR-0088 (Session is a core subdomain); ADR-0089 (realization has one
  owner); the runtime-host split plan, work items W14 (state convergence), W21
  (`SessionApplication` by use case) and W20.
- Affects: `awaken-session-state-contract`, `awaken-session-application`,
  `awaken-protocol-managed`, `awaken-session-executor`, `awaken-session-store`; wire shapes
  are unchanged.

## Context

A Session's state has five representations on main `5cafd57b49`: the persisted
aggregate (`PersistedSession` in `awaken-session-state-contract`, 17,441 lines), the
application service that mutates it (`SessionApplication`, 303 methods in 28 files), the
runtime-host slot (37 fields, part of it copied state), the protocol cache
(`ManagedState`, 160 methods, with its own `SessionStatus` and `SessionThreadStatus`),
and per-concern states for budget, MCP and baseline.

The aggregate itself spreads one lifecycle over several types: `SessionDisposition`
(Active, Archived, Deleting, Deleted), `SessionExecutionState` (Preparing, Activating,
ActivationFailed, Running, Rescheduling, Idle, Terminated), `SessionEnvironmentState`
and an identical `SessionEnvironmentPhase` (Unmaterialized, Resident, Suspending,
Hibernated, Restoring), `SessionResourceState`, and the projected `SessionPublicStatus`.

Transitions are written where they are needed: `SessionExecutionState` variants are
constructed 77 times in the state contract, 73 in session-application and 12 in
protocol-managed; `SessionEnvironmentState` variants 41 times in runtime-host, 34 in the
contract and 19 in session-application. Which combinations are legal is nowhere written
down, and warm/cold equivalence and recovery are protected by scenario tests rather than
by construction.

## Decision

1. **The aggregate owns its transitions.** `PersistedSession` stays the aggregate. Its
   lifecycle is one value with sub-states (disposition, execution, environment,
   resources). Every change goes through one transition function on the aggregate:
   (aggregate, command or observation) → (aggregate, events) or a typed refusal. The
   legal combinations are listed in one table next to that function, and combinations
   that are cheap to make unrepresentable are made so.

2. **One mutation entry point.** In the use-case split of `SessionApplication` (W21),
   the core `SessionMutations` is the only caller of the transition function: load,
   transition, commit with the fence, append outbox events. Use-case services build
   commands; they never set a sub-state.

3. **The environment sub-state is fed by realization observations only.** Under
   ADR-0089 the realizer reports what happened; the Session records it through the
   transition function. runtime-host stops constructing `SessionEnvironmentState`.

4. **Projections are pure.** `SessionPublicStatus`, the Managed `SessionStatus` and
   `SessionThreadStatus`, and the budget view are pure functions of the aggregate and the
   committed facts. Protocol adapters never decide state.

5. **Remove the duplicate.** `SessionEnvironmentPhase` merges into
   `SessionEnvironmentState`; the stored strings are identical, so the persisted shape
   does not change (the store keeps reading both names until a migration retires the old
   column name, if any).

6. **Decision-table tests.** The transition function is tested exhaustively over
   (sub-state, command) pairs; recovery and warm/cold equivalence follow from the
   projections being pure.

7. **Ratchet.** Construction of state variants outside the state contract (and, for the
   environment sub-state, outside the realizer's observation mapping) may only shrink.

## Alternatives

- **One flat enum for the whole lifecycle.** Readable at first, but the product of the
  sub-states is large and most combinations are legal; sub-states with a documented
  table are smaller and keep the existing wire.
- **Converge state at the end (W14 as planned).** W21 would first spread today's
  transition code over seven services, making the later convergence harder.
- **Event-source the Session.** A larger change than the problem requires; the outbox
  already gives durable events, and the aggregate stays the source of truth.

## Consequences

- One place answers whether a Session may move from one state to another; the W21
  services become thin, and W20's protocol adapter keeps no lifecycle logic.
- runtime-host and protocol-managed lose their state decisions (41 and 12 construction
  sites), which also shrinks the slot work in slices F and S.
- Persisted data and public wire shapes are unchanged.
- Order: this ADR before W21; the environment part lands with ADR-0089's slice R.
