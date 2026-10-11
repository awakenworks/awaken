# ADR-0089: Session Realization — One Decision Point, One Executor

- Status: Accepted
- Date: 2026-10-09
- Revised: 2026-10-09 (after the ADR consistency review: the Session aggregate keeps
  the durable lease and environment truth; the first draft moved them to the realizer);
  2026-10-10 (naming: the crate is `awaken-session-realization` and its component
  `RealizationExecutor` — "realizer" already names a per-resource-kind realizer in
  ADR-0038/0053/0063/0075, and the component only executes effects)
- Builds on: [ADR-0066](0066-session-service-binding-and-realization.md) (the durable
  `SessionRealizationLease` is the only Session ownership authority);
  [ADR-0074](0074-session-environment-suspension-and-checkpointed-continuation.md)
  (`PersistedSession.environment` is the only durable lifecycle authority);
  [ADR-0075](0075-unified-managed-session-worker-execution.md) D3 (resume reassigns the
  lease through the Session root CAS);
  [ADR-0081](0081-contract-layering-and-ratchet-guardrails.md) §9 (resource activation
  and terminal cleanup remain with the aggregate; the realization surface is a
  capability ladder); [ADR-0090](0090-session-state-model.md) (one Session state
  machine).
- Amends: [ADR-0088](0088-bounded-context-map-and-sdk-platform-contract.md) decision 2
  (realization effects belong to Execution's future `awaken-session-realization`;
  the independent `awaken-session-realization-driver` stays in Session and is
  shared by both topologies).
- Preserves: [ADR-0056](0056-sandbox-reuse-two-orthogonal-volumes.md) amendment item 3
  (provisioning-contract slimming keeps the `lease` pure functions and the
  `select_provider*` kernels).
- Affects: `awaken-runtime-host`, `awaken-session-realization-driver`,
  `awaken-session-application`, `awaken-session-realization-contract`,
  `awaken-session-state-contract`, `awaken-provisioning-contract`; awaken-cloud only
  through later renames.

## Context

Realization is the lifecycle of a Session's environment: grant and renew the lease,
bind an owner, stage resources, deliver content, hibernate or restore, and clean up a
terminal environment. The Accepted ADRs above already fix where its durable truth
lives: the Session aggregate, changed only through the Session root CAS. What they do
not fix is where the *decisions* and the *effects* are made, and today both are spread.

On main `5cafd57b49`, code whose module or file names realization, environment
lifecycle, provisioning or terminal reconciliation totals about 48,000 production
lines in eight crates across four contexts:

| Crate | Lines | Role today |
|---|---:|---|
| `awaken-runtime-host` | 14,264 | decides transitions and executes effects inside `SharedHost` (113 `SharedHost` methods in realization modules, 43 upward module edges into host modules) |
| `awaken-provisioning-contract` | 12,833 | provider port plus lease, spec and preparation vocabulary |
| `awaken-sandbox-local` | 3,805 | provider |
| `awaken-session-application` | 3,790 | decides realization control (`realization`, `realization/control`) |
| `awaken-session-realization-contract` | 3,599 | `FrozenSessionProjection`, five commands (`Begin`, `Renew`, `Activate`, `Acknowledge`, `Fail`), and sixteen control traits |
| `awaken-sandbox-container` | 2,973 | provider |
| `awaken-session-realization-driver` | 1,609 | executes the actions the aggregate projects |
| `awaken-execution-composition` | 1,303 | wires providers |

Decisions are made in three places: `SessionEnvironmentState` variants are constructed
41 times in runtime-host, 34 in the state contract and 19 in session-application.
Effects are executed in two: runtime-host and the driver. No single place answers
"what may this environment do next", and every lease or cleanup fix touches several
crates.

## Decision

1. **Decisions belong to the Session aggregate.** Every realization transition — lease
   grant, renewal and reassignment, activation, failure, hibernation, restore, and each
   terminal-cleanup step — is decided by the aggregate's transition function
   (ADR-0090) and committed through the Session root CAS. The durable
   `SessionRealizationLease` and `PersistedSession.environment` remain the only
   authorities. The decision logic in session-application's `realization` and
   `realization/control` modules, and the environment-state construction in
   runtime-host, move into that function.

2. **Effects belong to one executor.** `awaken-session-realization` (context: Execution,
   as ADR-0088 decision 2 already lists) executes the actions the aggregate projects —
   provider calls, resource staging, content delivery, checkpoint, disposal — and
   reports observations (lease receipts, realized, failed, released, cleaned up). It
   keeps no durable state of its own, only process-local handles, and runs in both
   topologies. runtime-host's realization modules (the module-layer guard's set, still
   named `realizer` until the code rename) move into it. The topology-neutral
   `awaken-session-realization-driver` remains independent in Session. The
   Coordinator-local Session application and Worker-side executor call its same
   stateless realization and terminal-cleanup drivers over contract ports.

3. **The boundary is ports: commands in, observations out.** The Session application
   calls the realization contract's ports; the realization executor implements them; the
   composition root wires them, so the Session application never depends on the
   realization crate. The ports keep ADR-0081 §9's capability ladder as typed roles: each
   topology is constructed with exactly the roles it may use (a remote Worker receives
   only its claimed roles), and no caller discovers a role at run time. Roles are
   regrouped by caller; two roles merge only when no topology withholds one of them.

4. **Functional core, imperative shell.** Decisions are pure functions over
   (aggregate, command or observation) and are tested with exhaustive decision tables;
   the realization executor executes effects and is tested against the provider port.

5. **The provisioning contract shrinks to the provider port and its spec vocabulary**,
   keeping the `lease` pure functions and the `select_provider*` kernels that ADR-0056's
   amendment item 3 excludes from slimming.

6. **Ratchets.** Construction of `SessionEnvironmentState` variants outside the state
   contract, and code named for realization outside the realization executor, its contract and the
   providers, may only shrink.

7. **Order.** In the split of `awaken-runtime-host`, this move follows the layering of
   the per-Session slot into frozen, realization and run parts, so the realization
   state is one type when it moves. The environment part of ADR-0090's transition
   function lands with it.

## Alternatives

- **The realizer owns the lease and the environment state** (this ADR's first draft).
  Contradicts ADR-0066, 0074, 0075 and 0081 §9, and a database-less Worker would need
  either a durable store or a second authority beside the aggregate.
- **One command port and one observation port replacing the ladder.** A topology could
  then no longer be denied a capability by construction.
- **The Session application calls the realization crate directly.** Couples the
  application to execution infrastructure; ports wired by the composition root keep the
  direction.
- **Keep today's spread.** Every lease or cleanup rule keeps spanning three crates.

## Consequences

- One function answers what an environment may do next, and one crate executes it;
  lease and cleanup fixes become one decision-table row and, if needed, one effect.
- runtime-host loses its largest cluster (about 13,000 lines of realization);
  session-application loses its realization decision code to the aggregate. The
  topology-neutral driver remains independent and shared; moving physical effect
  adapters does not change its Control deadline or receipt protocol.
- The Session ↔ realization executor ports are also the remote protocol a Worker uses, through the
  claimed roles.
- Persisted data and wire shapes are unchanged: commands, receipts, the lease and
  `FrozenSessionProjection` keep their shapes.

Terminal cleanup's Worker requirements are a pure projection of the frozen
Session root and its next cleanup action. Their existing lowering belongs to
the published realization contract, alongside the terminal assignment and its
authorization vocabulary. The application still selects and commits the one
root claim; this projection owns no lease, scheduler, provider call or cache.

### Control request deadlines are an executor concern

The existing realization driver owns the topology-neutral Control deadline
adapter and its single shared semaphore. Runtime Host supplies authority-derived
timing and retains the exact Session mutex and response-application checks.
Queue residence and transport I/O consume one absolute monotonic deadline; an
expired queued request must not enter Control. Completion reacquires the same
semaphore. The adapter owns no durable lease, phase, receipt or retry queue.
