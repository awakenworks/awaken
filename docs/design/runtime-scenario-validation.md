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

## Quality Gate Ownership

`scripts/ci/check-all.sh` is the only complete executable test manifest. Its
named groups are a scheduling interface, not independent definitions of release
quality. `.github/workflows/quality.yml` supplies an exact candidate revision,
the required substrate-capable runner, dependencies, and retained evidence, then
invokes that manifest once without selecting a subset. Pull requests, local-main
pushes, and the tag-only release workflow all enter the same reusable workflow.

The workflow must not repeat package, scenario, backend, or group lists. The
test-orchestration checker rejects a second workflow invoking `check-all.sh`, a
partial `--group` workflow, a release-local test list, missing exact-SHA binding,
successful failure suppression, or missing always-uploaded timing evidence.
External provider behavior and physical substrate behavior remain labelled
evidence boundaries; they are not promoted from fixtures or static manifests.

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
| RS-BOOT-001 | Runtime startup rejects incomplete dependencies | AllInOne or Coordinator has otherwise valid selected ports and stores | one required Directory, observation, transport, Agent or Environment port is absent | startup returns a named error before Host assembly; no Session is written; an initially empty lifecycle drains immediately |
| RS-BOOT-002 | Runtime startup retains exact role ownership | the Runtime startup entry receives selected store groups and services | the role lacks Coordinator ownership, a required store group is absent, split Coordinator receives Control stores, or its Control service is absent | startup returns an error before recovery and component construction; it never constructs another role's fallback authority |
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
| RS-REC-002 | context assembly fails after Environment acquisition | a Session has acquired its lifecycle-owned Environment and has no complete Runtime cache | a later configuration, tool export, or executor assembly step fails | no incomplete Runtime is installed; returned export leases are released; a retry reuses the exact Environment rather than creating another owner |
| RS-REC-003 | context construction is cancelled during export | an ACP Session has acquired its Environment and an export future owns a partial transport lease | the caller drops construction while export is suspended | the partial lease and lifecycle lock are released; no Runtime cache is installed; retry completes using the same Environment |
| RS-REC-004 | complete context is reused | context assembly and installation succeeded for the same frozen inputs and purpose | the caller opens the Session again | the complete cached context is reused without another Environment acquisition or tool export |
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
| RS-TOOL-002 | one Agent-message tool owner | the closed builtin catalog and Managed/native projections are assembled | descriptors and executors are selected | native exposes `agent_run`; a Managed primary with a published delegate or Advisor exposes exactly `list_agents`/`send_message`, a Managed single-Agent Session exposes no coordination tools, and a Managed child exposes neither family; no generic Task sender or recovery command competes with Coordination |
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

## High-risk regression review

The October 2026 crate review maps high-risk behavior to existing conformance
suites and adds tests only for identified gaps. A passing selected suite is
execution evidence for its configurations, not a whole-workspace line/branch
coverage percentage or proof of all platform/backend combinations.

| Risk | Required dimensions and state oracle | Evidence owner |
|---|---|---|
| Environment revision/lifecycle | maximum revision: change/no-op/invalid request; archive followed by update/bind; error leaves revision, definition and execution registration unchanged | `awaken-environment-contract::revision_boundary_rejects_changes_atomically_and_admits_noop_replay`; `awaken-environment-application::archive_is_terminal_for_every_environment_mutation_path` |
| Paged Worker demand | empty index, multiple empty pages, repeated/rewound cursor, later read failure and quarantined root; incomplete authority never reports zero demand | `awaken-session-contract`, `src/repository/tests/demand.rs` |
| Lost acknowledgement | scheduled effect chain with normal/lost finish response; retry the exact ACK without repeating either effect | `awaken-dispatch::a_chain_of_scheduled_actions_is_performed_to_completion_in_one_drive` |
| Process crash before settle | kill after claim/input delivery and before settlement; reopen SQLite, recover the same input, consume it once, reject stale ownership | `awaken-dispatch::sqlite_process_crash_before_settle_replays_and_consumes_exact_input_once` |
| Stale fence and occupied physical attempt | stale/current settlement, lease replacement while predecessor remains occupied, duplicate begin/finish and release; stale owner cannot settle or execute successor work | `awaken-dispatch-testkit`, `src/conformance/reliability.rs` |
| Cancellation/replay | direct versus durable ownership, queued/dead-letter/active work, stale generation and remote cancellation; committed cancellation survives retry without driving cancelled work | `awaken-session-executor`, `src/host/tests/run_boundary.rs`; existing ingress conformance |
| Terminal cleanup/recovery | committed terminal versus Awaiting, cold reopen, lost observer response, durable binding and stale cleanup fence; exact resource owner and stable receipts decide recovery | Host `cold_terminal_cleanup`, `terminal_disposal`, `terminal_memory`, `realization_renewal` and `resource_staging_lease` suites |
| Context admission and construction | identity match/mismatch × coordination/execution request × cached purpose, followed by phase failure/cancellation/retry rules below | Host cache admission decision table and construction suite |

For each added fault case, review the independent causes, decision rules,
observable positive/negative effects, and retry oracle in the test. Small finite
admission kernels enumerate their complete domains; asynchronous paths force
the actual ordering boundary and inspect authoritative state/owned resources.
This is systematic fault-oriented design. It does not claim measured MC/DC,
mutation adequacy, or fresh model-checker proof runs.

The verification report must name feature configurations and excluded tests.
Windows local tests and SQLite process-crash tests do not qualify real
PostgreSQL, NATS, Linux namespaces, containers or Kubernetes. Those combinations
remain separate deployment-qualification obligations until their actual suites
run against isolated backends. Source-surface and formal-ledger linkage checks
are separate from executable tests and from proof solving.

The stricter `check_formal_coverage.py --require-verified-profile` review on
October 8, 2026 reports 32 of 446 P0 repository-safety obligations without a
direct production proof/refinement grade. The ledger is authoritative for the
current count; [the verified-profile discussion](../formal-verification-expansion.md#verified-profile-the-31-direct-proof-gaps)
explains why modeled admission decisions alone do not prove backend durability,
external process behavior, or full asynchronous linearization. Passing executable
regressions and the required evidence-linkage gate does not close those gaps.

## Construction regression design

The Host context decomposition keeps the existing admission kernels, Session
lifecycle lock, Environment owner, and installation order. It introduces no new
authority. The following cause/effect rules complement the existing cache,
reservation, claimed-dispatch, publication, and terminal-recovery matrices.

The production-function length ratchet added on `origin/main` during this work
records the existing constructor at 1,568 lines. Its implementation now moves
into six phase functions, with the largest at 542 lines. The length baseline
follows those existing bodies at their actual smaller lengths; the 150-line
limit for new functions remains unchanged. Further phase decomposition remains
visible as recorded debt rather than being hidden by an exclusion.

| Rule | Independently controlled condition | Observable oracle | Executable test |
|---|---|---|---|
| HC1 | compact configuration fails after acquisition and identical request retries | no cached Runtime on either failure; exact Environment retained | `failed_context_assembly_retains_one_environment_for_retry` |
| HC2 | export fails before returning a lease | no cached Runtime; retry reuses the Environment | `acp_context_failure_releases_exports_and_reuses_environment` |
| HC3 | executor assembly fails after successful export | HC2 plus returned lease is dropped | `acp_context_failure_releases_exports_and_reuses_environment` |
| HC4 | caller cancels while export owns a partial lease | no cached Runtime; partial lease dropped; lifecycle lock released; exact-owner retry succeeds | `cancelled_context_construction_releases_lock_and_partial_export` |
| HC5 | identical successful open repeats | same context identity; export count unchanged; successful lease retained | `acp_context_failure_releases_exports_and_reuses_environment` |

These tests reference RS-REC-002 through RS-REC-004 and enter through the real
Session constructor. Faults are injected at existing ports. The cancellation
case uses a retained oneshot barrier with bounded waits, rather than elapsed
sleep or an assumed scheduler order. Environment identity and cache state are
the ownership oracles; fixture counters only check transport lease lifetime.
Publication immutability, stale fences, and terminal disposal retain their
existing separate tests.

An error from authoritative terminal recovery after installation may leave a
**complete** context cached, as before. HC1-HC4 deliberately target failures
before installation; they do not claim that every constructor error evicts a
successfully assembled context.

## CLI startup regression design

The Runtime process now checks all required dependencies before selecting a
scenario authority, restoring Deployment state, or assembling Host. Canonical
`ProcessRole::owns_startup_component` decides ownership; the private prepared
value only carries already selected non-optional ports.

| Rule | Independently controlled condition | Observable oracle | Executable test |
|---|---|---|---|
| CB1 | AllInOne/Coordinator × exactly one of five required ports absent (10 rows) | named error; zero Deployment recovery calls; Host customizer never reached; no Session or quarantine writes; immediate drain | `missing_runtime_dependencies_fail_before_host_or_tasks` |
| CB2 | either Runtime role has no Coordinator stores (2 rows) | CB1's failure and state oracles | `runtime_store_and_service_ownership_fails_closed` |
| CB3 | AllInOne lacks Control stores; split Coordinator has surplus Control stores or lacks Control service (3 rows) | named ownership error; no fallback component | `runtime_store_and_service_ownership_fails_closed` |
| CB4 | Control/Worker enters with otherwise complete Runtime inputs (2 rows) | ordinary error independent of debug assertions; CB1's state oracles | `non_runtime_roles_cannot_enter_runtime_assembly` |
| CB5 | valid split Coordinator, including unusable local sandbox with registered-worker placement | public/private route presence, authentication and exclusion; no local sandbox acquisition | existing `service_roles_expose_only_their_owned_api` |
| CB6 | valid AllInOne startup | canonical Management/Session/Resource surfaces and protected input behavior | existing CLI and Managed protocol integration suites |

The 17 added negative rows use the actual startup entry and isolated local
stores. A refusing Deployment port verifies that recovery is not reached;
moving recovery ahead of preflight changes both the error and the call oracle.
A zero-budget drain is a cleanup oracle; it does not inspect or count
private task registrations. The existing missing-Environment test keeps its
identity and now checks the entry's error instead of an obsolete panic helper.

Only the canonical Coordinator component registers Runtime background drain.
Resource reclamation retains the same guards, worker and clock, and starts after
the complete protected process routers have been assembled. This ordering does
not claim that every late component failure is globally free of effects:
canonical components may already own lifecycle tasks, and the outer lifecycle
remains responsible for their failure/cancellation cleanup. The matrix above
targets dependency preflight; it does not qualify real PostgreSQL or containers.

The CLI integration suites require the real `awaken-sandbox` companion built
with `hand` in the same target/profile directory as `awaken`. Its `hand --check`
must succeed before running startup cases. This verifies companion selection and
feature availability; Windows executions still do not qualify Linux namespace
isolation.

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
