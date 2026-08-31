# Runtime Scenario Validation

This document owns the Given/When/Then scenario text for runtime mechanism
validation. It is a coverage map, not a role owner: runtime roles, state
machines, and mechanism semantics remain in their owning design documents and
later in code/Rustdoc.

## Ownership

Scenario text lives here when it validates an interaction across more than one
runtime axis, such as delegation plus permission, message delivery plus commit,
or scheduled work plus ingress recovery.

Do not put canonical scenario text only in test comments, wiki pages, issue
descriptions, or fixtures. Tests may quote a short Given/When/Then comment for
readability, but each executable scenario must reference a scenario id from this
document. Fixtures are executable data; they are not the source of scenario
meaning.

Use the owning design document for mechanism changes before updating a scenario:

| Mechanism | Owner |
|---|---|
| run phases, state/effects, commit, resume, scheduled work | [runtime-behavior.md](runtime-behavior.md) |
| tool descriptors, capability checks, builtin tool placement | [tool-and-capability.md](tool-and-capability.md) |
| builtin toolsets and first-party extension contract | [builtin-tools-extension-contract.md](builtin-tools-extension-contract.md) |
| run ingress, durable dispatch, pending input, message delivery | [run-ingress-message-delivery.md](run-ingress-message-delivery.md) |
| package/import/vocabulary enforcement | [packaging-enforcement-matrix.md](packaging-enforcement-matrix.md) |

If a scenario requires a new role, state, port, durable record, or lifecycle
transition, update the mechanism owner first. This document can then add the
scenario id and coverage expectation.

## When To Write GWT Text

Write or update the textual GWT scenario before implementation when a change
crosses a runtime boundary or can fail through ordering, retry, replay, or
permission behavior. The text should explain observable domain behavior, not
test fixture mechanics.

Use this rule:

| Change type | Textual GWT required? | Where |
|---|---|---|
| single value/parser/serde rule | no; unit test name is enough | contract or unit test |
| new runtime role, state, port, durable record, or lifecycle transition | first update the owning design doc; add GWT only if behavior crosses boundaries | mechanism owner, then this document |
| multi-agent delegation behavior | yes | this document, `RS-MA-*` |
| internal or external message delivery behavior | yes | this document, `RS-MSG-*` |
| scheduled/deferred work, resume, wake, retry, or recovery | yes | this document, `RS-SCH-*` or `RS-REC-*` |
| builtin tool contribution such as `agent_run` or Managed `send_message` | yes when it changes agent-visible behavior or runtime effects | this document plus builtin tool tests |
| arch hook or packaging rule only | no GWT; use enforcement docs and hook tests | [packaging-enforcement-matrix.md](packaging-enforcement-matrix.md) |
| UI/protocol projection only | only if it changes runtime-visible semantics | protocol/product owner |

Add the scenario text when the design becomes implementation-ready, not after
bugs appear. A good trigger is: "Could two correct-looking implementations
disagree about the committed outcome?" If yes, add a GWT scenario id before
coding. After implementation, executable tests reference that id and may include
short Given/When/Then comments, but the long text remains here.

Do not add GWT text for every unit test. The purpose is to freeze boundary
behavior: which aggregate owns truth, when a result becomes durable, what fails
closed, what is idempotent, and how recovery repairs a crash point.

## Test Organization

Organize executable scenarios by boundary, not by product workflow or concrete
tool family:

```text
crates/awaken-runtime/tests/
  scenarios/
    mod.rs
    multi_agent.rs
    messaging.rs
    scheduled_work.rs
    cross_mechanism.rs
    recovery.rs
  support/
    fake_resolver.rs
    fake_ingress.rs
    fake_backend.rs
    fake_commit_store.rs

crates/awaken-ext-builtin-tools/tests/
  scenarios/
    delegation_tool.rs
    coordination_tools.rs
  support/

crates/awaken-runtime-contract/tests/
crates/awaken-agent-contract/tests/
```

`awaken-runtime` scenario tests own neutral mechanism behavior: commit staging,
resume validation, permission/capability gates, dispatch handoff, idempotency,
append fences, and recovery after lost wake or crash points.

`awaken-ext-builtin-tools` scenario tests own concrete first-party tool
contributions: native `agent_run` and Managed `list_agents`/`send_message`.
Those tests must prove tools produce runtime effects through registered seams and
never mutate runtime state directly. The closed-catalog tests also prove that no
parallel generic sender, cancellation, or recovery command family can re-enter.

Contract-crate tests own stable wire shapes, serde, descriptor fingerprints,
capability values, command/result envelopes, and compatibility fixtures.

Do not create a `background_task` scenario namespace in neutral Runtime or
contract tests. The core mechanism is `scheduled_work` or `deferred_work`;
`BackgroundTask` remains a banned umbrella term there. The independently
selectable `awaken-ext-background-task` extension owns its narrowly scoped
detached-tool scenarios and tests under that extension, as specified by
ADR-0076. Its Runtime integration test may prove only the generic State/commit
boundary; it must not add task-specific branches to Runtime.

## Scenario File Contract

Each scenario test file should carry a short coverage map at the top:

```rust
//! ## Coverage map
//!
//! | Scenario id | Test |
//! |---|---|
//! | RS-MA-001 | allowed_delegation_submits_child_run |
```

Each test should include a compact Given/When/Then comment or helper name, but
the long scenario text remains in this document. This keeps the executable test
readable without creating a second prose specification.

Prefer deterministic in-memory tests for the first slice: fake resolver, fake
tool/backend adapters, fake durable ingress, append-fence checks, and explicit
crash-point replay for outbox/scheduled requests. Store or distributed-adapter
tests may reuse the same scenario ids later.

## Scenario Matrix

| Id | Scenario | Given | When | Then |
|---|---|---|---|---|
| RS-MA-001 | allowed delegation | a resolved run with `builtin-delegation-tools` selected and a delegate roster containing `reviewer` | the model calls `agent_run { agent_id: "reviewer" }` | the gate passes, a child run is submitted through the selected backend/ingress, and the result returns through normal tool output or committed facts |
| RS-MA-002 | denied delegation | a resolved run with no roster entry for `reviewer` | the model calls `agent_run { agent_id: "reviewer" }` | execution fails before dispatch; no child run, pending input, or tool side effect is committed |
| RS-MA-003 | delegation tool hidden without roster | a resolved run has no delegate or multiagent targets | resolution builds model-visible descriptors | `agent_run` is absent from the descriptor set and the model has no delegation affordance |
| RS-MA-004 | no generated delegation tool ids | a resolved run has multiple delegate agents | descriptors are generated for model-visible tools | exactly one `agent_run` descriptor is present; no `agent_run_<agent_id>` descriptor is emitted |
| RS-MA-005 | delegation replay fingerprint | a resolved run exposes `agent_run` with a target-agent list | the run is replayed or resumed from its executable snapshot | replay proves the same descriptor fingerprint and target list were visible before accepting the delegated result |
| RS-MSG-001 | internal cross-thread Agent message | a parent Run can call `send_message` for another Agent Thread | the durable tool operation admits a deterministic target Run | source `ActiveToolBatch` owns request/recovery, target `RunActivation.input` owns the frozen message, and Dispatch contains no parallel PendingInput/outbox fact |
| RS-MSG-002 | internal follow-up replay | an Agent follow-up is retried after an ambiguous response | the same source operation and target Run id are admitted again | exact payload replay is a no-op; same-id/different-payload is rejected and no second target message is created |
| RS-MSG-003 | external inbound message lifecycle | a product adapter receives an external message with public ids and auth data | the adapter translates it into neutral target-thread pending input | public DTO/status/auth names stay outside runtime; accepted-before-Run input follows pending/freeze/commit without being conflated with Agent coordination |
| RS-MSG-004 | pending mutation before freeze | a pending message has not yet been frozen into a run activation | a caller edits, retracts, or reorders it with the expected pending revision | the pending store applies the mutation and bumps revision; no committed message log entry is rewritten |
| RS-MSG-005 | pending mutation after freeze | a pending message has already been frozen into a run input snapshot | a caller tries to edit, retract, or reorder the original pending record | the mutation is rejected or represented as new pending input/facts; the frozen activation is not changed |
| RS-MSG-006 | lost external-ingress delivery ack | an accepting service outbox entry was committed and target pending append succeeded, but the delivery ack was lost | relay/recovery retries the same message id | the target append returns idempotent success and the ingress outbox can be cleared without duplicating pending input |
| RS-MSG-007 | lost wake after pending append | target pending input is committed but the wake notification is lost | pending-thread recovery scans durable pending input | recovery recreates an activation opportunity without changing committed message truth |
| RS-SCH-001 | scheduled resume | a committed `ScheduledAction` awaits the run in `ResumeTicket` | dispatch wakes and posts a result | runtime validates correlation, run/thread binding, snapshot, fingerprint, and idempotency before committing the resumed outcome |
| RS-SCH-002 | duplicate or stale resume | a resume result is duplicated, expired, or has the wrong fingerprint | dispatch or an adapter retries delivery | runtime rejects or ignores it without mutating committed facts |
| RS-SCH-003 | uncommitted scheduled request is not wakeable | a tool or hook produced a scheduled-action candidate but `ThreadCommit` failed | dispatch/server scans for wakeable work | no durable wake is created because only committed `ScheduledAction` records are dispatch truth |
| RS-SCH-004 | unknown scheduled result rejected | a resume result arrives with no matching committed `ScheduledAction` request | ingress attempts to resume the run | runtime rejects the result before it can update awaiting state or committed facts |
| RS-SCH-005 | scheduled action kind from selected plugin only | a plugin package is installed but not selected for the run | a tool/hook tries to stage a scheduled action kind owned by that plugin | validation fails closed because the action kind is absent from the resolved environment |
| RS-SCH-006 | no core BackgroundTask recovery object | an implementation wants to recover delayed work after crash | recovery scans neutral runtime/server durable state | recovery uses committed `ScheduledAction`, resume ticket, pending input, dispatch lease, or outbox records; neutral Runtime does not introduce a `BackgroundTask` object or queue (the separately selected ADR-0076 extension is outside this scenario) |
| RS-SCH-007 | uncommitted deferred effect is not recoverable | a hook/tool produced a deferred-work candidate but the thread commit failed before persistence | process restarts and recovery scans durable state | no work is recovered because no committed request, resume ticket, pending input, or outbox exists |
| RS-CTRL-001 | cancel while waiting on scheduled work | a run is awaiting in `ResumeTicket` with a committed `ScheduledAction` request | cancel enters through durable dispatch control | runtime commits a typed terminal cancel result; a later scheduled result for the same correlation is rejected or ignored without mutating committed facts |
| RS-CTRL-002 | stop policy wins before resumed result | a stop policy commits a terminal stop reason for a run that previously requested deferred work | the deferred result later arrives through ingress/resume | runtime observes the terminal run state and fails closed; no resumed outcome or extra thread messages are committed |
| RS-CTRL-003 | resume after terminal state rejected | a run is terminal because of cancel, stop, max-attempt, or natural finish | a stale resume command arrives with an otherwise valid correlation id | resume validation rejects it because the run can no longer consume the result |
| RS-REC-001 | crash after commit before wake | a scheduled request or external-ingress outbox is committed, but wake delivery is lost | recovery scans committed requests/outboxes | recovery recreates the activation opportunity or target append without duplicating committed output |
| RS-ING-001 | direct attempt has no durable operation | direct execution is selected | code attempts to request scheduled wake or recovery from the direct driver | the operation is unrepresentable because `DirectAttemptDriver` exposes no durable-only method |
| RS-ING-002 | durable ingress freezes pending at boundary | multiple pending inputs exist for a target thread | durable ingress claims ownership and prepares a run activation | one owner freezes an eligible pending set into the activation; later pending changes do not mutate that activation |
| RS-ING-003 | public protocol does not expose delivery internals | a product route submits input or control intent | the host lowers it through its private direct/durable delivery sum | the public command remains protocol-neutral; dispatch batching and recovery remain private server policy |
| RS-ING-004 | concurrent pending append is idempotent | two nodes receive the same message id for one target thread | both attempt pending append with revision/CAS enforcement | one append creates the pending record and the other observes idempotent success or retryable revision conflict without duplication |
| RS-ING-005 | single owner executes one thread | two durable ingress nodes observe the same activation opportunity | both attempt to claim, freeze, and execute the target thread | only one owner succeeds; committed message order and run projection remain serializable |
| RS-ING-006 | wake is advisory only | a wake record is duplicated, delayed, or lost | durable ingress reconciles dispatch state | pending input, committed facts, leases, and outboxes decide recovery; wake records never become message or run truth |
| RS-EXT-001 | service-backed tool | a `RawTool` adapter reaches an external service inside its in-process `invoke` | the call returns success, typed failure, or indeterminate | the adapter maps the result to neutral tool output/resume data; it never writes runtime state directly |
| RS-EXT-002 | unselected builtin extension | `awaken-ext-builtin-tools` is installed but not selected for the run | resolution builds model-visible tools | builtin descriptors and hooks are absent, so agent behavior is unchanged |
| RS-EXT-003 | model capability fails closed | a resolved run requires model-serving support for a tool, continuation, decision, or modality feature | the selected `BackendProfile` does not advertise that feature | execution fails before runtime starts rather than silently degrading behavior |
| RS-TOOL-001 | builtin tool ids live outside core | runtime core is built without `awaken-ext-builtin-tools` selected | resolution builds model-visible descriptors | concrete builtin tool ids such as `bash`, `send_message`, and `agent_run` are absent; core contributes only neutral tool mechanisms |
| RS-TOOL-002 | one Agent-message tool owner | the closed builtin catalog and Managed/native projections are assembled | descriptors and executors are selected | native exposes `agent_run`; a Managed primary exposes exactly `list_agents`/`send_message`, while a Managed child exposes neither family; no generic Task sender or recovery command competes with Coordination |
| RS-PLG-001 | hook cannot bypass commit | a selected plugin hook wants to mutate runtime state or schedule work | the hook runs during a runtime phase | it returns `StateCommand` or effects for validation/staging; it cannot write durable state or dispatch work directly |
| RS-PLG-002 | installed plugin is inert until selected | a plugin package is installed but absent from the resolved run config | runtime resolves hooks, tools, state keys, and action kinds | the plugin contributes no descriptors, hooks, state keys, scheduled-action kinds, or behavior |
| RS-MEM-001 | recall uses current Run input | a Thread has historical User messages and a new Run input | Memory Recall builds its query | only the current Run input drives selection and recalled messages are request-only |
| RS-MEM-002 | extraction freezes terminal evidence | an `Ended` Run commits | the terminal observer creates an extraction intent | the intent records one immutable raw transcript snapshot and explicit ranges |
| RS-MEM-003 | extraction is asynchronous and durable | a terminal commit has created an extraction intent | the parent response returns and the reconciler runs | parent completion does not wait; restart resumes the same stable Extractor Run |
| RS-MEM-004 | extraction effect is idempotent | observation or storage delivery is duplicated | the controller claims and applies the intent | CAS/content hashes produce one MemoryStore effect and one terminal receipt |
| RS-MEM-005 | awaiting does not extract | a Run commits `Awaiting` | terminal settlement is evaluated | no extraction intent or MemoryStore mutation is created |
| RS-CMP-001 | soft compaction prefetch is non-blocking | estimated context crosses the soft but not hard threshold | `BeforeInference` schedules compaction | the parent inference proceeds while one stable Compactor Run is tracked |
| RS-CMP-002 | hard compaction joins or computes | context crosses the hard threshold | a matching artifact is ready, in flight, or absent | the hook reuses, joins, or computes the stable Run before inference |
| RS-CMP-003 | compaction never hides uncovered history | compaction has no valid artifact or fails | the inference window is built | `KeepAll` remains active and no raw committed message is lost |
| RS-CMP-004 | compact artifact bridges to current fold point | a cached artifact covers an older prefix | later messages extend the Thread before the hard fold | summary plus verbatim bridge covers the complete selected prefix |
| RS-WIN-001 | auxiliary windows are frozen ranges | Memory, Compact, or Outcome requests a transcript slice | the Thread advances concurrently | materialization remains bounded by the captured snapshot version and validated ranges |
| RS-BRN-001 | Session branch freezes source prefix | a same-Workspace create names a source Session and optional committed end | source advances after admission | target inference retains the admitted immutable prefix and target transcript contains no copied source messages |
| RS-BRN-002 | invalid branch source fails before create | source is missing, cross-Workspace, or end exceeds committed history | Managed create validates the extension | no target Session, Thread message, or realization effect is created |
| RS-BRN-003 | branch projection is placement-neutral | a target carrying a transcript-prefix reference realizes locally or on a remote Worker | frozen projection is installed | both placements receive the same request-only messages through `RuntimeRunContext`; neither creates a second transcript |
| RS-GOAL-001 | Outcome is a cross-Run extension | `awaken-ext-goal` receives an Outcome definition | its controller drives Worker and Grader Runs | every Agent execution uses the ordinary Run boundary; Runtime Core learns no Outcome vocabulary and no `BackgroundTask` aggregate is created |
| RS-GOAL-002 | Outcome recovery reuses stable Runs | a Worker or Grader Run committed before the Outcome head advanced | recovery reloads the Worker Thread Outcome state | the controller observes the stable terminal Run, applies the missing version-guarded transition, and does not re-run inference |
| RS-GOAL-003 | Outcome state uses Thread truth | an Outcome transition and Evaluation are ready | the extension persists them | state commits through the existing `CommitCoordinator`; no OutcomeStore, Host cursor, or product Session state becomes truth |
| RS-GOAL-004 | cancel rejects stale grading | a Worker or Grader Run is active for one Outcome phase | cancel commits the relevant terminal result and the Outcome head advances to Interrupted | later results carrying the old expected Run id/version are rejected as stale |
| RS-GOAL-005 | resume is new intent after Outcome cancel | an Outcome completed as Interrupted | a user wants work to continue | Runtime does not resume the terminal Run; the caller submits a new Run or Outcome command with a new stable identity |
| RS-TERM-001 | terminal observer runs after commit | a Run reaches any `Ended(EndCause)` | the terminal commit succeeds | observers receive the committed Run at least once and cannot change `RunResult` |
| RS-TERM-002 | Awaiting is not terminal | a Run commits `Awaiting` and later resumes | settlement notifications are dispatched | no terminal observer runs until the resumed Run commits `Ended` |
| RS-TERM-003 | terminal reaction survives crash and duplicates | a process dies after terminal commit or an observation is redelivered | the extension checks its stable intent/receipt | missing work is recovered and duplicate delivery creates no duplicate effect |
| RS-EVT-001 | live stream is not replay truth | a run emits live stream events and then a stream sink fails before or during commit | protocol replay reads runtime history later | replay derives from committed events/facts only; sink failure does not mutate runtime truth or create durable loss |

## Coverage Rules

1. Every scenario id must map to at least one executable test before the related
   implementation is considered complete.
2. A test may cover multiple scenario ids only when the interaction is genuinely
   cross-mechanism; otherwise keep tests focused.
3. Negative scenarios must assert the absence of committed side effects, not only
   the returned error.
4. Recovery scenarios must name the crash point and the durable truth used for
   repair.
5. Builtin-tool scenarios must assert that concrete tool behavior lives in
   `awaken-ext-builtin-tools`, while runtime core sees only registered
   descriptors, effects, commands, outputs, and resume data.
6. Scenario ids are stable. Rename only when the scenario meaning changes; keep
   the test function free to evolve.

## Guardrails

G1, G5, G6, G8, G9, G13, and G14 in [INVARIANTS](../INVARIANTS.md).
