# Session Environment transition function

The Session aggregate owns one pure decision over its Environment sub-state
(ADR-0089 decision 1, ADR-0090 decision 3):

```text
transition_environment(SessionEnvironmentFacts, SessionEnvironmentEvent)
    -> Result<SessionEnvironmentDecision, SessionEnvironmentRejection>
```

It lives in `awaken-session-state-contract` (`environment/transition.rs`), next
to `PersistedSession::transition_execution`. `SessionEnvironmentFacts::project`
folds the root, the event's asserted realization lease and the caller's `now`
into the facts; it is their only constructor, so no caller decides over facts
the aggregate did not project, and `PersistedSession` gains no further methods.
The function reads no clock and performs no I/O.

## Vocabulary

| Type | Meaning |
|---|---|
| `SessionEnvironmentStep` | the durable phase refined by the suspend sub-phase (8 values); `SessionEnvironmentState::step()` projects it |
| `SessionTerminality` | `Active`, `CleanupJoining` (terminal cleanup Fenced join), `Terminal` |
| `RealizationLeaseStanding` | `Fenced` (current lease), `Unfenced` (no lease anywhere: legacy and Coordinator-local), `Stale` |
| `SessionEnvironmentEvent` | commands: begin/renew realization, admit materialization, advance continuation, require resident, retire for cleanup; observations: materialized, legacy binding delivered, quiesced, checkpointed, source release prepared, source released, restored, expired checkpoint deleted, cleaned up, effect failed |
| `SessionEnvironmentDecision` | the next step and at most one `SessionEnvironmentInstruction` (materialize, quiesce, checkpoint, prepare source release, dispose source, restore, delete expired checkpoint, retire) |
| `SessionEnvironmentRejection` | closed refusal reasons |

The data-carrying laws on `SessionEnvironmentState` (`begin_suspend_at`,
`record_quiescence`, `record_checkpoint`, `record_source_release_prepared`,
`complete_suspend`, `begin_restore`, `complete_restore`, receipt application,
terminal disposal) still build the exact durable value. The transition function
owns which step may follow which and which effect a step demands. A failure
observation never changes the step: a failed effect does not prove physical
absence (ADR-0074); its disposition decides only the lease and execution axes
(`realization_failure_outcome`).

## Safety laws

Proved by Kani over the complete closed domain and swept by the T8 test:

1. A terminal Session never re-enters a live step: it stays, retires to
   Unmaterialized by `CleanedUp`, or, only in the Fenced join, admits an
   in-flight materialization receipt. It is never instructed a continuation or
   materialization effect.
2. Every step change is one declared edge. Resident is entered only from
   Unmaterialized by materialization (or the legacy claim binding) or from
   Restoring by its receipt.
3. No instruction is issued under a stale lease. Create, source-release
   preparation and disposal, and retirement need the `Fenced` standing. So do
   the observations that advance source release or terminal disposal.

## Inventory of Environment decisions outside the aggregate (main `fdd3553173`)

D = decision, E = effect, O = observation mapping, P = read projection.

| Site | Class | Moves to |
|---|---|---|
| session-application `continuation.rs:212` `advance_environment_continuation` arm selection | D+O | `AdvanceContinuation` |
| `continuation.rs:392` `ensure_environment_resident` state dispatch | D+O | `RequireResident` |
| `continuation.rs:64` memory preflight; `application.rs:24` topology guard | D (guards) | stay as aggregate preconditions next |
| `realization.rs:32` `environment_admits_realization_effects` (6 callers) | D | **moved**: `SessionEnvironmentState::admits_realization_effects` |
| `realization/control.rs:245` renew gate | D | `RenewRealization` |
| `realization/control.rs:321`, `:804`, `:13`, `:1089`, `:990` lease grant, expiry, terminal claim and rollback | D (lease axis) | stay with `session_realization_assignment`; not Environment steps |
| `application.rs:1077` binding command, `resource_reconciliation.rs:671/727` cleanup records | O | `Materialized`, `CleanedUp` |
| `application.rs:1152` `Unowned` on a missing root; realization `publication.rs:157` `Unowned` | D (two inputs) | unify behind `AdmitMaterialization` |
| `resource_reconciliation/terminal_cleanup.rs:103/170` path and authorization | D | `RetireForCleanup` |
| realization `projection_synchronizer.rs:88` legacy claim merge (writes a projection copy) | D | `LegacyBindingDelivered` |
| realization `bound_environment.rs:452`, `terminal_preparation.rs:181`, `session_environment.rs:103/149/172` `TerminalEffectMode` (six writers) | D (effect mode) | one pure function in realization from observation and frozen facts |
| realization `environment_lifecycle/*` owner, adoption and restore on the process-local owner | D (local) | stay: process-local, not durable state |
| realization `environment_continuation.rs` and runtime-host `lib.rs:939` adapters | E | unchanged |
| driver `terminal_cleanup_driver.rs:76` | O + retry policy | unchanged; the action comes from the aggregate |
| `lib.rs:113` recovery hints, `event_batch_cutover_validation.rs:121` | P | derive the hint from `AdvanceContinuation` |

protocol-managed and runtime-host have no production Environment decision:
their remaining state constructions are in tests.

## Disagreements recorded

| # | Paths | Choice |
|---|---|---|
| 1 | Uploading past generation expiry fails on every tick (`continuation.rs:268`); Hibernated expiry deletes and resets | kept: `GenerationExpired` rejection; a recovery edge needs an ADR-0074 decision |
| 2 | Suspending evidence that disagrees with its sub-phase: ReadyToDispose/Disposing return "no work", Uploading errors, Quiescing proceeds | uniform `InconsistentEvidence` (fail closed) |
| 3 | Aggregate wrappers `record_quiescence`, `record_checkpoint`, `complete_suspend`, `complete_restore` and `discard_expired_checkpoint` do not check terminality; the saga and source-release laws do | continuation observations on a terminal Session are refused |
| 4 | The legacy claim merge ignores terminality | refused unless the Session is active |
| 5 | Renew admits a terminal Session in every step; Begin refuses it | kept: two commands |
| 6 | Rebuild admission is always pending, yet a Rebuild receipt still applies | kept as two rows (admission versus receipt) |
| 7 | Quiesce, checkpoint and restore effects are not checked against the root lease (only the operation's captured fence reaches the provider) | stale standing refused; unfenced legacy kept |
| 8 | The driver retries `NotReady` in Prepare but fails in Dispose | out of scope here (driver policy) |

## Next sites to move

1. `realization/control.rs` renew gate → `transition_environment(RenewRealization)`
   with the asserted lease, mapping `ContinuationInProgress`→`NotReady` and
   `LeaseNotCurrent`→`StaleOwnership`.
2. `continuation.rs` `advance_environment_continuation` and
   `ensure_environment_resident`: select the arm from the decision's instruction,
   then call the existing data law. The memory preflight and topology guard move
   into the aggregate.
3. Fold `SessionEnvironmentTransitionError` and `SessionEnvironmentReceiptError`
   into `SessionEnvironmentRejection` as the laws start calling the function.
   `SessionEnvironmentPhase` becomes `step().phase()` for the store count
   selector.
4. The legacy claim merge becomes `LegacyBindingDelivered` in the contract. The
   six `TerminalEffectMode` writers collapse into one pure function in the
   realization.

## Verification

- `awaken-session-state-contract` `environment::transition::tests` T1–T8 and
  `session_repo::environment_facts_tests`.
- Kani `environment::transition::proofs`. Obligations are under
  `formal/obligations/session_environment/`.
