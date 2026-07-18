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
| builtin tool contribution such as `agent_run` or `send_message` | yes when it changes agent-visible behavior or runtime effects | this document plus builtin tool tests |
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
    task_tools.rs
  support/

crates/awaken-runtime-contract/tests/
crates/awaken-agent-contract/tests/
```

`awaken-runtime` scenario tests own neutral mechanism behavior: commit staging,
resume validation, permission/capability gates, dispatch handoff, idempotency,
append fences, and recovery after lost wake or crash points.

`awaken-ext-builtin-tools` scenario tests own concrete first-party tool
contributions: `agent_run`, `send_message`, cancellation, and recovery tools.
Those tests must prove tools produce runtime effects through registered seams and
never mutate runtime state directly.

Contract-crate tests own stable wire shapes, serde, descriptor fingerprints,
capability values, command/result envelopes, and compatibility fixtures.

Do not create a `background_task` test directory or scenario namespace. The
runtime mechanism is `scheduled_work` or `deferred_work`; `BackgroundTask`
remains a banned umbrella term outside negative tests.

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
| RS-MSG-001 | internal cross-thread message | a parent run can call `send_message` to another agent thread | the tool stages a message to a different thread | sender commit records an outbox entry, relay appends target pending input once by message id, and duplicate relay is idempotent |
| RS-MSG-002 | same-thread message | sender and target are the same thread and share one commit source | the tool stages a same-thread message | the message can be included in the sender checkpoint; otherwise implementation falls back to outbox semantics |
| RS-MSG-003 | external inbound message uses same lifecycle | a product adapter receives an external message with public ids and auth data | the adapter translates it into neutral target-thread pending input | public DTO/status/auth names stay outside runtime; the message follows the same pending/freeze/commit lifecycle as internal `send_message` |
| RS-MSG-004 | pending mutation before freeze | a pending message has not yet been frozen into a run activation | a caller edits, retracts, or reorders it with the expected pending revision | the pending store applies the mutation and bumps revision; no committed message log entry is rewritten |
| RS-MSG-005 | pending mutation after freeze | a pending message has already been frozen into a run input snapshot | a caller tries to edit, retract, or reorder the original pending record | the mutation is rejected or represented as new pending input/facts; the frozen activation is not changed |
| RS-MSG-006 | lost cross-thread delivery ack | a sender outbox entry was committed and target pending append succeeded, but the delivery ack was lost | relay/recovery retries the same message id | the target append returns idempotent success and the sender outbox can be marked delivered without duplicating pending input |
| RS-MSG-007 | lost wake after pending append | target pending input is committed but the wake notification is lost | pending-thread recovery scans durable pending input | recovery recreates an activation opportunity without changing committed message truth |
| RS-SCH-001 | scheduled resume | a committed `ScheduledAction` awaits the run in `ResumeTicket` | dispatch wakes and posts a result | runtime validates correlation, run/thread binding, snapshot, fingerprint, and idempotency before committing the resumed outcome |
| RS-SCH-002 | duplicate or stale resume | a resume result is duplicated, expired, or has the wrong fingerprint | dispatch or an adapter retries delivery | runtime rejects or ignores it without mutating committed facts |
| RS-SCH-003 | uncommitted scheduled request is not wakeable | a tool or hook produced a scheduled-action candidate but `ThreadCommit` failed | dispatch/server scans for wakeable work | no durable wake is created because only committed `ScheduledAction` records are dispatch truth |
| RS-SCH-004 | unknown scheduled result rejected | a resume result arrives with no matching committed `ScheduledAction` request | ingress attempts to resume the run | runtime rejects the result before it can update awaiting state or committed facts |
| RS-SCH-005 | scheduled action kind from selected plugin only | a plugin package is installed but not selected for the run | a tool/hook tries to stage a scheduled action kind owned by that plugin | validation fails closed because the action kind is absent from the resolved environment |
| RS-SCH-006 | no BackgroundTask recovery object | an implementation wants to recover delayed work after crash | recovery scans runtime/server durable state | recovery uses committed `ScheduledAction`, resume ticket, pending input, dispatch lease, or outbox records; no `BackgroundTask` object or queue is required or accepted |
| RS-SCH-007 | uncommitted deferred effect is not recoverable | a hook/tool produced a deferred-work candidate but the thread commit failed before persistence | process restarts and recovery scans durable state | no work is recovered because no committed request, resume ticket, pending input, or outbox exists |
| RS-CTRL-001 | cancel while waiting on scheduled work | a run is awaiting in `ResumeTicket` with a committed `ScheduledAction` request | cancel enters through `RunIngress.control` | runtime commits a typed terminal cancel result; a later scheduled result for the same correlation is rejected or ignored without mutating committed facts |
| RS-CTRL-002 | stop policy wins before resumed result | a stop policy commits a terminal stop reason for a run that previously requested deferred work | the deferred result later arrives through ingress/resume | runtime observes the terminal run state and fails closed; no resumed outcome or extra thread messages are committed |
| RS-CTRL-003 | resume after terminal state rejected | a run is terminal because of cancel, stop, max-attempt, or natural finish | a stale resume command arrives with an otherwise valid correlation id | resume validation rejects it because the run can no longer consume the result |
| RS-REC-001 | crash after commit before wake | a scheduled request or send-message outbox is committed, but wake delivery is lost | recovery scans committed requests/outboxes | recovery recreates the activation opportunity or target append without duplicating committed output |
| RS-ING-001 | direct ingress unsupported path | direct ingress is selected | a durable-only scheduled wake or recovery operation is requested | the operation fails closed with a typed unsupported error before runtime execution |
| RS-ING-002 | durable ingress freezes pending at boundary | multiple pending inputs exist for a target thread | durable ingress claims ownership and prepares a run activation | one owner freezes an eligible pending set into the activation; later pending changes do not mutate that activation |
| RS-ING-003 | public ingress has no delivery intent taxonomy | a product route submits input or control intent | server code calls `RunIngress` | the public command is `submit` or `control`; live-vs-durable routing, batching, and fallback remain private durable-ingress policy |
| RS-ING-004 | concurrent pending append is idempotent | two nodes receive the same message id for one target thread | both attempt pending append with revision/CAS enforcement | one append creates the pending record and the other observes idempotent success or retryable revision conflict without duplication |
| RS-ING-005 | single owner executes one thread | two durable ingress nodes observe the same activation opportunity | both attempt to claim, freeze, and execute the target thread | only one owner succeeds; committed message order and run projection remain serializable |
| RS-ING-006 | wake is advisory only | a wake record is duplicated, delayed, or lost | durable ingress reconciles dispatch state | pending input, committed facts, leases, and outboxes decide recovery; wake records never become message or run truth |
| RS-EXT-001 | service-backed tool | a `RawTool` adapter reaches an external service inside its in-process `invoke` | the call returns success, typed failure, or indeterminate | the adapter maps the result to neutral tool output/resume data; it never writes runtime state directly |
| RS-EXT-002 | unselected builtin extension | `awaken-ext-builtin-tools` is installed but not selected for the run | resolution builds model-visible tools | builtin descriptors and hooks are absent, so agent behavior is unchanged |
| RS-EXT-003 | model capability fails closed | a resolved run requires model-serving support for a tool, continuation, decision, or modality feature | the selected `BackendProfile` does not advertise that feature | execution fails before runtime starts rather than silently degrading behavior |
| RS-TOOL-001 | builtin tool ids live outside core | runtime core is built without `awaken-ext-builtin-tools` selected | resolution builds model-visible descriptors | concrete builtin tool ids such as `bash`, `send_message`, and `agent_run` are absent; core contributes only neutral tool mechanisms |
| RS-TOOL-002 | builtin task recovery is ops scoped | `recover_failed_messages` exists in the builtin task toolset | a default agent profile resolves its descriptors | the recovery tool is hidden unless an explicit operations/recovery role or policy selects it |
| RS-PLG-001 | hook cannot bypass commit | a selected plugin hook wants to mutate runtime state or schedule work | the hook runs during a runtime phase | it returns `StateCommand` or effects for validation/staging; it cannot write durable state or dispatch work directly |
| RS-PLG-002 | installed plugin is inert until selected | a plugin package is installed but absent from the resolved run config | runtime resolves hooks, tools, state keys, and action kinds | the plugin contributes no descriptors, hooks, state keys, scheduled-action kinds, or behavior |
| RS-GOAL-001 | goal continuation is not background work | `awaken-ext-goal` is selected and a run reaches a natural-end continuation point | `ContinuationGuard` evaluates the selected goal policy | runtime records an opaque verdict or terminal conclusion; no `BackgroundTask` aggregate or background-task queue is created |
| RS-GOAL-002 | goal replay reuses verdict | a committed run already has a recorded goal continuation verdict | replay or resume rebuilds execution state | runtime reuses the recorded opaque verdict and does not re-grade the goal or reinterpret product outcome names |
| RS-GOAL-003 | async goal evaluation uses existing deferred mechanism | a goal extension needs delayed or external grading | the extension requests work outside the current phase | it uses `ScheduledAction` or an external wait/resume channel with correlation/idempotency; the goal extension still owns goal semantics and runtime core only validates resume/commit |
| RS-GOAL-004 | cancel invalidates async goal result | async goal evaluation has a committed `ScheduledAction` or wait/resume ticket for one run | cancel enters through `RunIngress.control` and commits a terminal cancel result | the resume ticket or scheduled request is marked cancelled/superseded for that run; backend cancellation is attempted only if supported, and any later goal result is rejected as stale |
| RS-GOAL-005 | resume is new intent after goal cancel | a run was terminally cancelled while async goal evaluation was outstanding | a user later wants goal evaluation to continue | runtime does not resume the cancelled run; caller must submit a new run or new goal command with a new correlation/idempotency identity |
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
