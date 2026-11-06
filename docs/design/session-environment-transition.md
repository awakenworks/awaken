# Session Environment transition function

The Session aggregate owns one pure decision over its Environment sub-state
(ADR-0089 decision 1, ADR-0090 decision 3):

```text
transition_environment(SessionEnvironmentFacts, SessionEnvironmentEvent)
    -> Result<SessionEnvironmentDecision, SessionEnvironmentRejection>
```

It lives in `awaken-session-contract` (`state/environment/transition.rs`), next
to `PersistedSession::transition_execution`. `SessionEnvironmentFacts::project`
folds the root, the event's asserted realization lease and the caller's `now`
into the facts. The same domain module privately projects a directive or an
explicit absent-root observation; `admit_unowned` alone consumes the latter,
so no caller fabricates aggregate or ownership facts. `PersistedSession` gains
no further methods. The function reads no clock and performs no I/O.

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
3. No instruction is issued under a stale lease. Create, Rebuild,
   source-release preparation and disposal, and retirement need the `Fenced`
   standing. So do the observations that advance source release or terminal
   disposal.
4. A Rebuild is instructed only for an active, Resident, fenced Session whose
   old source is closed and whose work survives to rebuild from; the
   permanent refusal is issued only for a closed source without access.

## Rebuild admission

A long-lived Managed Session whose sandbox died recovers by a Rebuild: a new
substrate replaces the Resident binding. The aggregate decides it over two
facts the realization supplies on the Rebuild intent
(`RebuildSourceEvidence`, admission data outside the effect identity; rows
written before it decode as no evidence):

| Facts (Resident, fenced, active) | Decision |
|---|---|
| `source_closed` and `source_access` | `Materialize(Rebuild)`, written ahead as the pending effect |
| not `source_closed` | `MaterializationPending` (retry), unchanged |
| `source_closed`, no `source_access` | `RebuildSourceUnavailable`: permanent, `RunError` code `session_environment_rebuild_source_unavailable` (Conflict, HTTP 409, a permanent conflict the Worker fails once, never 503); Managed `session.error` `unknown_error`, retry status terminal |

A replay of a recorded Rebuild stays. `session-realization`
(`rebuild_source`, tables RS/RA) derives the facts: closed means the provider
observed the exact handle Terminal or Unavailable and, for a source in a
Worker's process and filesystem, that incarnation's end is known (this
process supervised it, an exact exit, or `Superseded`); access is a retained
volume (a Worker-filesystem source the provider reports Terminal under
retained continuity) or a Repository input. The realization itself knows
only its own supervision today: a cold Worker-filesystem source stays pending
until the incarnation's exact exit or `Superseded` receipt reaches the
derivation. The landed Rebuild receipt
becomes the materialization `origin`; `materializing_effect_fence` returns the
in-flight or landed Create or Rebuild.

## Inventory of Environment decisions outside the aggregate (main `fdd3553173`, updated after the session-application and realization migrations)

D = decision, E = effect, O = observation mapping, P = read projection.

| Site | Class | Moves to |
|---|---|---|
| session-application `continuation.rs:212` `advance_environment_continuation` arm selection | D+O | **moved**: `AdvanceContinuation`; the application carries out the decided instruction |
| `continuation.rs:392` `ensure_environment_resident` state dispatch | D+O | **moved**: `RequireResident` |
| `continuation.rs:64` memory preflight; `application.rs:24` topology guard | D (guards) | **moved**: facts `suspend_memory_admitted`, `continuation_topology_supported`; layout validation reads the same `supports_idle_retention` |
| `realization.rs:32` `environment_admits_realization_effects` (6 callers) | D | **moved**: `SessionEnvironmentState::admits_realization_effects` |
| `realization/control.rs:245` renew gate | D | **moved**: `RenewRealization` |
| `realization/control.rs:335` `begin_session_realization_inner` lease grant and reuse, Resource-generation admission, the new lease, MCP claim selection and claim ids | D (lease axis) | **moved**: `SessionAssignmentOperations::begin` admits through `BeginRealization`, arbitrates through the unchanged `session_realization_assignment` (now beside it in `awaken-session-contract`, re-exported by the execution contract) and claims; its refusals keep their control failures (`From<SessionRealizationBeginRejection>`). The application loads, pins credentials, commits under the root CAS and drives `next_action` |
| `realization/control.rs:726` failure expiry; `terminal_cleanup/claims.rs:135` terminal cleanup claim and `:251` rollback | D (lease axis) | stay as lease-axis commands (`assignment().expire`, `claim`, `restore_predecessor`); not Environment steps |
| `application.rs:1077` binding command | O | **moved**: admission and receipt decide `AdmitMaterialization` / `Materialized` in the aggregate |
| `resource_reconciliation.rs:635/727` terminal cleanup authorization and disposal records | O | **moved**: `PersistedSession::authorize_terminal_cleanup_effect`, `authorize_terminal_cleanup_disposal_effect` and `record_terminal_cleanup_disposal` decide `RetireForCleanup` / `CleanedUp` after their exact lease-generation check; the application classifies every cleanup refusal through the one `terminal_cleanup_authorization_failure` |
| application missing-root admission; realization publication ordinary-thread admission | D (absence observation) | **moved**: `SessionEnvironmentEffectAuthorization::admit_unowned` decides `AdmitMaterialization` over explicit absent-root facts; no lease or Session instruction is fabricated |
| `resource_reconciliation/terminal_cleanup.rs:103` cleanup path (Completed / NormalizeLegacy / WaitForExternalWorker / DriveLocal) | scheduling | stays: which actor drives; the effect orchestration is the driver's |
| realization `projection_synchronizer.rs:88` legacy claim merge (writes a projection copy) | D | **moved**: `SessionEnvironmentState::deliver_legacy_binding` decides `LegacyBindingDelivered` |
| store `row_codec.rs:187` legacy `environment_binding` column | decode | **moved**: `SessionEnvironmentState::from_legacy_binding` |
| realization `bound_environment.rs`, `terminal_effects/preparation.rs`, `session_environment.rs` `TerminalEffectMode` (six writers plus `for_terminal_source`) | D (effect mode) | **moved**: writers record a `TerminalSourceObservation`; `TerminalEffectMode::decide(observation, SourceReleasePredecessor)` is the one pure decision (table HT0) |
| realization `environment_lifecycle/*` owner, adoption and restore on the process-local owner | D (local) | stay: process-local, not durable state |
| realization `environment_continuation.rs` and runtime-host `lib.rs:939` adapters | E | unchanged |
| driver `terminal_cleanup_driver.rs:76` | O + retry policy | unchanged; the action comes from the aggregate |
| executor terminal reconciliation and cold assignment sweep | orchestration | **moved**: driver `terminal_cleanup_recovery`; executor implements only the named `TerminalRecoveryProjections` local effects; T1–T3 retain the original captured generation |
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
| 6 | Rebuild admission was always pending, yet a Rebuild receipt still applied | superseded 2026-11-01: admission decides on the realization's source evidence (see Rebuild admission); a recorded Rebuild replays; a receipt lands only for the admitted pending Rebuild |
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

## Final admission and recovery ownership

- **Absent root.** `AdmitMaterialization` has an explicit missing-root row.
  The contract's `admit_unowned` verifies the exact intent and projects that
  absence once. Every ordinary effect kind remains eligible without a
  realization assertion; an asserted lease is `RealizationStale`. Absence
  issues no durable Session instruction and admits no other Session event.
  Application and realization use the same row. Managed projections still
  fail closed without their binding authority or when a lookup loses the root;
  an ordinary Run retains its original Dispatch claim and sandbox binding.
- **Rejected evidence.** Realization carries the aggregate's receipt or
  transition refusal unchanged and reads its `recovery_action`. A stale lease
  retries; inconsistent evidence or an invalid transition stays terminal for
  the same evidence. No adapter reclassifies by error name or text.
- **Legacy cleanup.** `LegacyCleanupNormalized` expresses the historical
  one-stage receipt's no-lease retirement. The aggregate first verifies exact
  historical evidence; the row requires terminality and refuses stale
  ownership. It authorizes no new physical effect and cannot become a current
  cleanup receipt.
- **Recovery.** The driver alone selects, bounds and reconciles cold terminal
  assignments, handles NotFound, and decides retirement or revocation.
  Executor projections perform one named process-local effect on the original
  captured generation; the aggregate and existing cleanup driver still own
  durable admission, provider effects and receipts. No cursor or retry phase is
  persisted outside the root.
- **Intervention signal.** The existing lifecycle supervisor retains a
  disposable map of Session identity to an unrepairable continuation refusal.
  Its existing owned task scans to EOF, then signals a first or changed refusal
  once; successful scans forget disappeared/recovered roots. Failed or
  cancelled prefixes neither consume nor clear a signal. The map moves into
  and back from the same sequential recovery task, so concurrent status scans
  cannot replace its view. Retry-classified refusals remain quiet and no
  refused continuation is executed. A process restart may signal the still
  blocked root again; this diagnostic grants no authority and persists nothing.

## Resource reconciliation decision

The Resource sub-state follows the same pattern. `SessionResourceReconciliationInstruction::decide(&PersistedSession)`
projects eight facts (terminal, external Worker Environment, Idle, pending
generation, Repository retirements, Releasing record, installed inputs,
activation records) and calls the one pure `next_resource_reconciliation`
(`state/resource_activation/reconciliation.rs`). It returns a closed
`SessionResourceReconciliationInstruction`:

| Facts | Instruction |
|---|---|
| terminal | `ReleaseTerminal` (terminal cleanup) |
| live, external, no pending, retirements | `RetireDetachedGeneration` |
| live, external, otherwise | `Hold` |
| live, local, not Idle | `Hold` (changes deferred while a Run owns execution) |
| live, local, Idle, pending | `ApplyLocally(InstallPending)` |
| … no pending, Releasing record | `ApplyLocally(RefuseReleasingWithoutPending)` |
| … installed inputs or activation records | `ApplyLocally(ReapplyActive { adopt_legacy })`, `adopt_legacy` when there are no records |
| … only retirements | `ApplyLocally(RetireDetachedGeneration)` |
| … nothing | `Hold` |

| Site (main `f34b9ac81f`) | Class | Moves to |
|---|---|---|
| session-application `resource_reconciliation.rs` `reconcile_persisted_resources` stack-saving pre-check (not Idle; "the exact no-op branch below") | D | **moved**: returns on `Hold` before the activation/retirement future is built; every other step passes the same value on |
| `reconcile_prepared_resources` external-Worker branch, `ordinary_reconciliation_gate.rs` and the pending/Releasing/installed case analysis | D | **moved**: one `match` on the instruction; the application reads the aggregate only for effect data (manifests, revisions) and post-commit settlement checks |
| recovery sweep candidate filter (`needs_resource_reconciliation`, external without retirements) | scheduling | stays: which roots the sweep visits; an external root with a pending generation and retirements is still visited for credential pinning |

The Kani harness `terminal_session_never_enters_ordinary_resource_reconciliation`
moved with the decision; the contract decision table (rows R1–R17, sweep
S1–S4 over all 256 fact combinations) covers every branch. Behaviour is
unchanged on every production path. Only two rows differ, and only when the
full path is entered without the pre-check (a test does this): a non-Idle
local root, and an Idle local root with nothing installed or retained. Both
are now `Hold`. Before, the first reached terminal cleanup and the second
asked for the effects role.

## Provider retry hints through the one driver

The realization contract owns `SessionRealizationEffectFailure`: the unchanged
neutral `RunError` and the provider's optional relative retry hint. Realization
captures that hint from its typed error before the existing neutral projection;
it does not sample another wall clock or reinterpret the provider's frozen
observation. Both existing entry points carry the same value: Worker projection
synchronization and the local `RealizationRuntime::install_session_projection`.
The latter shares one adoption/context body with the Environment role, retaining
the hint before any executor error projection can flatten it.

The existing failure-report helper sends the original failure to Control and
invalidates its exact lease projection. It charges only its own monotonic elapsed
wait to the original hint with saturating subtraction; exhausted hints remain
`Some(0)`. Control/invalidation failure never replaces the original effect, and
cancellation drops the same owned future. Dispatch reads the original error's
one classification: only Defer carries the hint into its existing backoff;
Retry and Terminal cannot acquire a new delay or budget from it.

`SessionRealizationDriveError` is the sole failure algebra for both topologies.
The mirrored `awaken_session_application::SessionRealizationError` and its
three-arm reconstruction are removed; callers import the contract owner directly,
without an alias. Existing application report/update prefixes remain presentation
only, and Managed still projects the original neutral fault/code/message. The
realization-role macro spells the owning contract path directly; its existing
invocations already depend on that owner. No wire or durable schema changes.

## Verification

- `awaken-session-contract` `state::environment::transition::tests` T1–T13,
  `environment::legacy_binding::tests` L1–L4, and
  `session_repo::environment_facts_tests` (topology, Memory, continuation lease,
  the aggregate wrappers W1–W10, cleanup laws W11–W12, admission facts S1–S3);
  `session_repo::controlled_aggregate_tests::rebuild_admission_reads_realization_source_evidence_and_lands_write_ahead`
  (A1–A8); `effect_intent::tests::rebuild_source_evidence_is_admission_data_not_identity`
  (historical rows, H1–H3).
- `awaken-session-realization` `rebuild_source::tests` (RS1–RS4, RA1–RA3).
- `awaken-session-application`
  `tests::authority::first_materialization::rebuild_admission_maps_each_source_decision_to_one_run_error`
  (M1–M3); `awaken-protocol-managed` classifier row R15.
- `awaken-session-realization` `terminal_effects::tests` HT0 (exhaustive mode
  table) and HT1.
- `awaken-session-application` `tests::recovery_sweep` H2 (continuation hint).
- `awaken-session-execution-contract`
  `environment_rejections_project_onto_one_control_failure` (R1–R4).
- `awaken-session-application` `tests::continuation` (existing saga tables and
  O1–O3 for the adopted fail-closed rows).
- Kani `environment::transition::proofs`, including
  `rebuild_requires_a_closed_source_with_access`. Obligations are under
  `formal/obligations/session_environment/`.

- `awaken-session-contract` `state::environment::effect_intent::tests`
  U1–U3 and `absent_root_facts_never_admit_another_session_event` U4;
  the complete finite sweep includes both root-presence facts.
- `awaken-session-realization-driver` `terminal_cleanup_recovery::tests`
  R1–R9/T1–T3 and S1–S7 (absence, generation selection and bounded sweep).
- `awaken-session-application` `tests::recovery_sweep` D1–D12 (changed
  refusal, successful absence, failed later page, retry EOF, aggregate cause).

- `awaken-session-execution-contract` E1–E4 exercise the exact neutral
  payload and hint partitions; the pure Kani `driver_wait_never_extends_or_discards_the_original_retry_hint`
  checks the complete Duration domain, without a clock/transport liveness claim.
- `awaken-session-realization` H1–H4 keep the provider's original hint through
  its existing single Run projection.
- `awaken-session-realization-driver` D1–D5 cover report/invalidation waits,
  their failures, exhausted hints and caller cancellation.
- `awaken-session-executor` L3–L6 use the real Local application/install/R/provider
  adapter chain over the canonical Root fixture; observation, Create, adoption,
  disposal, retained binding and absent context are all asserted. A failed
  observation remains indeterminate even for a capacity-typed error. C3 drives
  an actual Provider Create capacity refusal through the ordinary claimed
  resolver and original SQLite Queue to Defer/None, preserving its exact claim
  and dispatch budget; it does not assert a Managed failure/refund CAS.
- `awaken-dispatch-execution` W1–W4 keep hints only on the existing Defer
  branch; application L1–L2 and P1–P3 cover local budget and message projection.
