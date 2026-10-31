# ADR-0090: One Session State Machine, Everything Else a Projection

- Status: Accepted
- Date: 2026-10-09
- Revised: 2026-10-09 (after the ADR consistency review: plan references described
  inline; relation to ADR-0089 restated)
- Builds on: [ADR-0088](0088-bounded-context-map-and-sdk-platform-contract.md) (Session
  is a core subdomain); [ADR-0066](0066-session-service-binding-and-realization.md),
  [ADR-0074](0074-session-environment-suspension-and-checkpointed-continuation.md) and
  [ADR-0075](0075-unified-managed-session-worker-execution.md) (the aggregate and its
  root CAS are the only durable Session authority);
  [ADR-0081](0081-contract-layering-and-ratchet-guardrails.md) §9 (resource activation
  and terminal cleanup remain with the aggregate);
  [ADR-0089](0089-session-realization-single-owner.md) (realization decisions in the
  aggregate, effects in one executor).
- Affects: `awaken-session-state-contract` (merged into `awaken-session-contract`
  on 2026-10-30, see the amendment below), `awaken-session-application`,
  `awaken-protocol-managed`, `awaken-runtime-host`, `awaken-session-store`; wire shapes
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

2. **One mutation entry point.** When `SessionApplication` is split by use case
   (lifecycle, inputs, resources, credentials, realization, runs, queries), the core `SessionMutations` is the only caller of the transition function: load,
   transition, commit with the fence, append outbox events. Use-case services build
   commands; they never set a sub-state.

3. **The environment sub-state changes only through the transition function.** Under
   ADR-0089 the aggregate decides each realization transition and the realization executor reports
   what its effects did; both commands and those observations enter through the
   transition function. runtime-host stops constructing `SessionEnvironmentState`.

4. **Projections are pure.** `SessionPublicStatus`, the Managed `SessionStatus` and
   `SessionThreadStatus`, and the budget view are pure functions of the aggregate and the
   committed facts. Protocol adapters never decide state. Whether Managed session
   events are derived when read or materialized at commit is a separate decision; this
   ADR only requires that either way they are a pure function of committed facts.

5. **Remove the duplicate.** `SessionEnvironmentPhase` merges into
   `SessionEnvironmentState`; the stored strings are identical, so the persisted shape
   does not change (the store keeps reading both names until a migration retires the old
   column name, if any).

6. **Decision-table tests.** The transition function is tested exhaustively over
   (sub-state, command) pairs; recovery and warm/cold equivalence follow from the
   projections being pure.

7. **Ratchet.** Construction of state variants outside the state contract (and, for the
   environment sub-state, outside the realization executor's observation mapping) may only shrink.

## Alternatives

- **One flat enum for the whole lifecycle.** Readable at first, but the product of the
  sub-states is large and most combinations are legal; sub-states with a documented
  table are smaller and keep the existing wire.
- **Converge state after the use-case split.** The split would first spread today's
  transition code over seven services, making the later convergence harder.
- **Event-source the Session.** A larger change than the problem requires; the outbox
  already gives durable events, and the aggregate stays the source of truth.

## Consequences

- One place answers whether a Session may move from one state to another; the
  use-case services become thin, and the Managed protocol adapter keeps no lifecycle
  logic.
- runtime-host and protocol-managed lose their state decisions (41 and 12 construction
  sites), which also shrinks the work of layering the runtime-host per-Session slot.
- Persisted data and public wire shapes are unchanged.
- Order: this ADR lands before the use-case split of `SessionApplication`; the
  environment part lands with ADR-0089's move of realization out of runtime-host.

The existing `SessionRunReplacement` and `SessionRunCommandFingerprint` values
are defined only in `awaken-session-types`. Admission services, Dispatch,
protocol adapters and golden-wire fixtures depend on that owner directly.
Runtime and Run Ingress contracts consume those types without re-exporting
them: moving a value cannot leave a second public contract path behind. This
changes import ownership, not serialization, fingerprint framing, persistence
or Run admission behavior; the existing persisted-wire and reservation decision
tables remain the compatibility evidence.

Archive admission and business-command identity are likewise pure Session values.
The state contract owns their existing `session-archive-intent-v2` framing and the
expected receipt record for an archived, never-attempted preparation. The application
only reads and compares the repository receipt; mutation and recovery consume the
same identity law. Sampling a route timestamp never changes that business identity.
Runtime retains the complete terminal cleanup assignment as one disposable projection,
rather than splitting its Environment state and no-Create witness into separate fields.
Neither projection grants authority beside the root CAS and its realization lease.
Expired pristine cleanup-affinity eligibility and receipt visibility are also pure
queries of that same state owner; the application obtains the receipt and performs
the existing fenced assignment CAS.

## Amendment 2026-10-30: one Session Core contract

The existing Session state, Event, budget and repository contracts consolidate
into `awaken-session-contract`, with private modules and one explicit public
vocabulary. This changes code ownership and paths, not the aggregate transition
laws, persistence schema, serde fields, replay/CAS rules or runtime authorities.
The historical before-state tables above retain their original subjects.
SessionTypes, Dream, configuration, physical provider SPI and transport contracts
keep their existing owners. The empty `SessionStore` composition marker moves
to the existing Session application's persistence composition; its real reader,
maintenance, write and outbox ports remain Core-owned. Tooling conformance uses
those Core role bounds and gains no normal application dependency.

All original owned tests, proof bodies and the persisted proptest regression seed
are retained. Previously public Event/Budget value fields retain their exact
reachability, declarations and serde shape; their newly inherited guard measurement
is recorded with original-source provenance, not approval of new public fields.
Source conservation is not current native or formal qualification.
