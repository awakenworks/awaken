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
owns which step may follow which and which effect a step demands. Every
aggregate wrapper (`SessionEnvironmentOperations`: effect admission, receipt
application, suspend and restore intents, the quiescence, checkpoint,
source-release and restore observations, expired-checkpoint deletion) decides
first and applies its data law only after the decision admitted it, so the
terminality, lease and evidence rules hold on every path.

`SessionEnvironmentFacts::project_continuation` projects the facts for the two
continuation commands: the asserted lease is the one whose fence the step's
next effect presents (a committed quiesce, checkpoint or restore operation
presents the lease it captured; every other step acts under the root lease, so
a higher-epoch successor may finish a source release). The facts also carry
the frozen-topology guard (`SessionPlacementClass::supports_idle_retention`)
and the suspend Memory preflight.

## Errors

`SessionEnvironmentRejection` is the one step refusal. The two law error types
are projections of it: `SessionEnvironmentTransitionError::Rejected` and
`SessionEnvironmentReceiptError::Rejected` carry it unchanged, and keep only the
exact-evidence refusals a data law alone can judge (receipt mismatch,
checkpoint expiry, checkpoint policy; environment mismatch, reservation,
Resource transition, unknown materialization). A lost lease keeps its receipt
name `RealizationStale`, the one the realization boundary reads as lost
authority. `SessionEnvironmentRejection::recovery_action` classifies retry
versus reject once. Control and continuation map a rejection in one place each
(`From<SessionEnvironmentRejection> for SessionRealizationControlFailure`,
`SessionContinuationError::rejected`). `SessionEnvironmentPhase`, the store's
count selector, is `SessionEnvironmentStep::phase`. A failure
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

## Inventory of Environment decisions outside the aggregate (main `fdd3553173`, updated after the session-application and realization migrations)

D = decision, E = effect, O = observation mapping, P = read projection.

| Site | Class | Moves to |
|---|---|---|
| session-application `continuation.rs:212` `advance_environment_continuation` arm selection | D+O | **moved**: `AdvanceContinuation`; the application carries out the decided instruction |
| `continuation.rs:392` `ensure_environment_resident` state dispatch | D+O | **moved**: `RequireResident` |
| `continuation.rs:64` memory preflight; `application.rs:24` topology guard | D (guards) | **moved**: facts `suspend_memory_admitted`, `continuation_topology_supported`; layout validation reads the same `supports_idle_retention` |
| `realization.rs:32` `environment_admits_realization_effects` (6 callers) | D | **moved**: `SessionEnvironmentState::admits_realization_effects` |
| `realization/control.rs:245` renew gate | D | **moved**: `RenewRealization` |
| `realization/control.rs:321`, `:804`, `:13`, `:1089`, `:990` lease grant, expiry, terminal claim and rollback | D (lease axis) | stay with `session_realization_assignment`; not Environment steps |
| `application.rs:1077` binding command | O | **moved**: admission and receipt decide `AdmitMaterialization` / `Materialized` in the aggregate |
| `resource_reconciliation.rs:635/727` terminal cleanup authorization and disposal records | O | **moved**: `PersistedSession::authorize_terminal_cleanup_effect`, `authorize_terminal_cleanup_disposal_effect` and `record_terminal_cleanup_disposal` decide `RetireForCleanup` / `CleanedUp` after their exact lease-generation check; the application classifies every cleanup refusal through the one `terminal_cleanup_authorization_failure` |
| `application.rs:1152` `Unowned` on a missing root; realization `publication.rs:157` `Unowned` | D (two inputs) | open: unify behind `AdmitMaterialization` |
| `resource_reconciliation/terminal_cleanup.rs:103` cleanup path (Completed / NormalizeLegacy / WaitForExternalWorker / DriveLocal) | scheduling | stays: which actor drives; the effect orchestration is the driver's |
| realization `projection_synchronizer.rs:88` legacy claim merge (writes a projection copy) | D | **moved**: `SessionEnvironmentState::deliver_legacy_binding` decides `LegacyBindingDelivered` |
| store `row_codec.rs:187` legacy `environment_binding` column | decode | **moved**: `SessionEnvironmentState::from_legacy_binding` |
| realization `bound_environment.rs`, `terminal_effects/preparation.rs`, `session_environment.rs` `TerminalEffectMode` (six writers plus `for_terminal_source`) | D (effect mode) | **moved**: writers record a `TerminalSourceObservation`; `TerminalEffectMode::decide(observation, SourceReleasePredecessor)` is the one pure decision (table HT0) |
| realization `environment_lifecycle/*` owner, adoption and restore on the process-local owner | D (local) | stay: process-local, not durable state |
| realization `environment_continuation.rs` and runtime-host `lib.rs:939` adapters | E | unchanged |
| driver `terminal_cleanup_driver.rs:76` | O + retry policy | unchanged; the action comes from the aggregate |
| runtime-host `application.rs` `reconcile_terminal_cleanup_for_lease` (NotFound table T1–T3) and `recover_terminal_cleanup_assignments` | orchestration | open: belongs to the driver; it reads the process-local installed Sessions the executor split (P4) restructures |
| `lib.rs:113` recovery hints | P | **moved**: `SessionEnvironmentFacts::continuation_due` (the `AdvanceContinuation` decision at the scan's `now`) |
| `event_batch_cutover_validation.rs:121` | P | stays: a count over `SessionEnvironmentPhase` |

protocol-managed and runtime-host have no production Environment decision:
their remaining state constructions are in tests.

## Disagreements recorded

| # | Paths | Choice |
|---|---|---|
| 1 | Uploading past generation expiry fails on every tick (`continuation.rs:268`); Hibernated expiry deletes and resets | kept: `GenerationExpired` rejection; a recovery edge needs an ADR-0074 decision |
| 2 | Suspending evidence that disagrees with its sub-phase: ReadyToDispose/Disposing return "no work", Uploading errors, Quiescing proceeds | uniform `InconsistentEvidence` (fail closed); **implemented** |
| 3 | Aggregate wrappers `record_quiescence`, `record_checkpoint`, `complete_suspend`, `complete_restore` and `discard_expired_checkpoint` do not check terminality; the saga and source-release laws do | continuation observations on a terminal Session are refused; **implemented** |
| 4 | The legacy claim merge ignores terminality | refused unless the Session is active; a realization directive's projection stands Active (only `BeginRealization`'s admission issues one), so the refusal is unreachable there |
| 5 | Renew admits a terminal Session in every step; Begin refuses it | kept: two commands |
| 6 | Rebuild admission is always pending, yet a Rebuild receipt still applies | kept as two rows (admission versus receipt); the admission row now refuses a recorded Rebuild too, as production always did |
| 7 | Quiesce, checkpoint and restore effects are not checked against the root lease (only the operation's captured fence reaches the provider) | stale standing refused; unfenced legacy kept; **implemented** through `project_continuation` |
| 8 | The driver retries `NotReady` in Prepare but fails in Dispose | out of scope here (driver policy) |

## Realization migration

Realization and the store no longer construct or rewrite the Environment
state, and the cleanup laws decide through the transition function:

- **Legacy binding.** `SessionEnvironmentState::deliver_legacy_binding`
  decides `LegacyBindingDelivered` over
  `SessionEnvironmentFacts::project_realization_directive` (Active, Fenced: a
  directive is issued only under `BeginRealization`) and applies the raw
  Resident value only when the decision enters Resident from Unmaterialized.
  The store decodes the predecessor column with
  `SessionEnvironmentState::from_legacy_binding`; a decode is not a
  transition.
- **Terminal cleanup.** `authorize_terminal_cleanup_effect` and
  `authorize_terminal_cleanup_disposal_effect` decide `RetireForCleanup`;
  `record_terminal_cleanup_disposal` decides `CleanedUp` (the completed replay
  with `already_recorded`) and retires the state to the decided step. The
  events assert no lease: each law first checks the exact lease generation, so
  the root's own lease stands. `SessionEnvironmentFacts::project_terminal_cleanup`
  reads no clock; table T11 proves neither row reads a time-dependent fact.
  `SessionCleanupError::EnvironmentRejected` carries a refusal; `LeaseNotCurrent`
  keeps the name `RealizationMismatch` (one `From`).
- **Terminal effect mode.** The six writers record what they observed
  (`TerminalSourceObservation`: Unbound, Ready, Terminal{data_readable},
  DefinitivelyUnavailable, Disposing, RestoreAwaiting, ClosedPublicationSource)
  and the cleanup authorization records the frozen `SourceReleasePredecessor`;
  `PreparedBoundEnvironment::effect_mode` derives the mode once through
  `TerminalEffectMode::decide`. Table HT0 sweeps all 16 inputs and asserts the
  two safety properties (live I/O only from a readable, unprepared source;
  provider preparation skipped only on Disposing or an inherited disposal).
- **Recovery hint.** The scan's continuation hint is
  `SessionEnvironmentFacts::continuation_due(root, now)`: true exactly when
  `AdvanceContinuation` instructs an effect.
- **Guard.** `_single_owner_fitness.py` no longer exempts the realization from
  `state-construction`, and its `environment-rewrite` rule counts mutable
  borrows of the state, undecided data laws called on an `.environment` value
  (the list is read from the contract) and assignments of another state value.
  Both are 0 outside the store conformance kit (5 fixture rewrites, baselined
  with a reason).

### Behaviour changes

1. A claim-first legacy binding is merged only through the aggregate's
   decision (same outcome for every reachable projection; a terminal
   projection would now be refused).
2. Preparation and disposal authorization and the disposal receipt refuse an
   active Session (`EnvironmentRejected(NotTerminal)`); the completed replay
   refuses one too. Production reaches them only for terminal Sessions.
3. The application classifies a cleanup command that is no longer pending as
   `NotReady` on the preparation and disposal record paths too (previously
   `Invalid`), matching the authorization path: the driver re-reads the root.
4. The continuation hint is false for a resident idle Session before its
   checkpoint deadline (previously true, then a no-op reconcile) and for every
   refused advance (activities open, Memory unproven, stale lease,
   inconsistent evidence, expired generation, unsupported topology), which is
   no longer re-attempted and logged on every sweep; the root re-enters the
   hint when its facts admit the effect.

## Remaining

1. runtime-host `reconcile_terminal_cleanup_for_lease` and
   `recover_terminal_cleanup_assignments` still orchestrate terminal recovery
   (NotFound table T1–T3, sweep); they move into the driver with the executor
   split, which owns the installed-Session reads they use.
2. The process-local `Unowned` admission (`publication` path) and
   `application.rs:1168` both stand for a missing root; unify behind
   `AdmitMaterialization`.
3. `realization_error.rs` still classifies
   `SessionEnvironmentReceiptError::RealizationStale` and wraps
   `SessionEnvironmentTransitionError` opaquely.
4. `normalize_legacy_terminal_cleanup` retires the state from historical
   one-stage receipts without a lease, which no `CleanedUp` row expresses.

## Verification

- `awaken-session-state-contract` `environment::transition::tests` T1–T11,
  `environment::legacy_binding::tests` L1–L4, and
  `session_repo::environment_facts_tests` (topology, Memory, continuation lease,
  the aggregate wrappers W1–W10, cleanup laws W11–W12).
- `awaken-session-realization` `terminal_effects::tests` HT0 (exhaustive mode
  table) and HT1.
- `awaken-session-application` `tests::recovery_sweep` H2 (continuation hint).
- `awaken-session-realization-contract`
  `environment_rejections_project_onto_one_control_failure` (R1–R4).
- `awaken-session-application` `tests::continuation` (existing saga tables and
  O1–O3 for the adopted fail-closed rows).
- Kani `environment::transition::proofs`. Obligations are under
  `formal/obligations/session_environment/`.
