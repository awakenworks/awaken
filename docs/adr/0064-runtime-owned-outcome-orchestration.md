# ADR-0064: Runtime-Extension-Owned Outcome over the Unified Run Boundary

- Status: Accepted
- Date: 2026-07-22
- Builds on: [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md)
  (Thread truth), [ADR-0040](0040-server-durable-ingress-integration.md)
  (durable Run execution), [ADR-0055](0055-typed-state-kernel-loop-actions-as-state.md)
  (typed state), [ADR-0057](0057-unified-agent-configuration.md)
  (executable snapshots), and
  [ADR-0059](0059-neutral-core-and-leaf-evolution.md) (neutral core boundaries)
- Supersedes:
  - the Native-only `GoalPlugin` / `GoalGuard` Outcome loop;
  - the intermediate design in which `awaken-runtime-host` owns the normative
    Outcome state machine;
  - `AgentToolGrader` and the Judge-specific `RawTool` bridge;
  - the in-memory `consumed_rounds` cursor and Thread-derived Outcome id.

## Context

Managed Agents exposes `user.define_outcome`: an Agent works toward a described
deliverable, a separately contextualized Grader evaluates it against a rubric,
and feedback drives bounded revisions. The former implementation compressed this
workflow into forced continuations inside one Native Run. A later implementation
moved Worker and Grader work onto ordinary backend-neutral Runs, but placed the
normative controller and persistence codec in `awaken-runtime-host`.

The second implementation improved execution parity and recovery, but assigned
domain ownership to the composition application. That prevents Outcome from
being used by an embedded/local Runtime without the Server Host and encourages
the Host to accumulate Memory, Compact, and Outcome lifecycle rules.

The design must preserve the completed recovery, isolation, and backend-neutral
Run work while restoring the extension boundary: Runtime Core supplies neutral
execution mechanisms; Runtime Extensions own their semantics and lifecycle;
applications compose those extensions and adapt external protocols.

## Decision

### D1: Runtime vocabulary is Run, Thread, and Step

Runtime Core uses only:

- **Thread** — the durable message, state, and Run-history boundary;
- **Run** — one stable, resumable Agent execution;
- **Step** — one inference/tool/state cycle inside a Run;
- `RunInput`, `RunState`, `RunResult`, `RunActivation`, and `RunExecutor`.

`Resume` continues the same Run. One Run may contain several Steps. The word
`turn` is not Runtime vocabulary; a public protocol may retain that word only in
its DTO/adapter and must translate it to a Run before crossing the runtime edge.

Outcome uses its own bounded-context vocabulary: `Outcome`, `Description`,
`Rubric`, `Iteration`, `Evaluation`, `Grader`, `Grade`, `Revision`,
`Deliverable`, and `Artifact`. An Outcome Iteration is not a Runtime Step: it
normally consists of a Worker Run followed by a Grader Run.

External products that call the intent a Goal translate that term at their
adapter into `outcome::Definition`. Runtime Core never gains Goal, Outcome,
Rubric, Grader, Judge, Memory, or Compact vocabulary.

### D2: Runtime Extension is broader than Plugin

The extension lifecycle has three distinct roles:

```text
PhaseHook
  observes/reacts within one Step

ContinuationGuard
  decides Complete/Continue at natural end, before terminal commit

RunTerminalObserver
  reacts after a terminal Run fact is committed and cannot change RunResult
```

They are not collapsed into a universal Hook because their timing, authority,
failure, and replay semantics differ.

- Memory Recall and Compact are in-Run Plugins using `BeforeInference`.
- Outcome is a cross-Run workflow extension, not a single-Run Plugin.
- Memory Extraction is a committed-terminal observer, not `StepEnd`, a
  continuation guard, or a Stop hook.
- `cancel_run` and `stop_run` are control commands. They converge on the same
  terminal commit boundary and do not define separate lifecycle hooks.

No `PostRunHook`, `AfterRunHook`, `StopHook`, or `CancelHook` family is added.
The one missing seam is the narrowly named `RunTerminalObserver`.

### D3: Outcome is owned by `awaken-ext-goal`

There is one normative Outcome state machine. Its domain model, controller,
Grader application port, Agent-backed Grader, prompt/parser, stable identities,
and Thread-state codec belong to `awaken-ext-goal`.

The controller is an application service **inside the Outcome bounded context**.
It coordinates ordinary Runs and state ports; it does not depend on
`SharedHost`, Managed Session DTOs, routes, ACP protocol objects, or a concrete
store.

```text
Defined
  -> RunningWorker(iteration=0, Initial)
  -> Evaluating(iteration=0)
       -> Completed(Satisfied | Failed)
       -> RunningWorker(iteration+1, Revision)
       -> Acknowledging -> Completed(MaxIterationsReached)

Any non-terminal phase --interrupt--> Completed(Interrupted)
Any infrastructure fault -----------> Errored(failure)
```

`Failed` is the business result for a rubric that does not apply or a decisive
permanent blocker. Worker/Grader execution, parsing, and persistence faults are
typed infrastructure failures and never collapse into that business result.

`iteration` is the zero-based revision counter: zero is the Evaluation after the
initial Worker Run, and one is the Evaluation after the first revision Run.
`max_iterations` is the maximum number of graded Iterations and remains in
`1..=20`. If the last permitted Evaluation still needs revision, the result is
`max_iterations_reached`, one ungraded acknowledgment Worker Run executes, and
the Outcome completes. Thus `max_iterations = 3` permits Evaluations 0, 1, and 2,
two ordinary revision Runs, and one final acknowledgment Run.

### D4: One ordinary Run boundary; no business Run services

Worker, Judge, Memory Selector, Memory Extractor, and Compactor Agents execute
through the same neutral Run boundary. Existing `RunActivation`, `RunExecutor`,
`RuntimeRunContext`, stable-id Runtime entries, `ThreadReader`, and
`CommitCoordinator` are reused.

The design does not introduce `AgentRunService`, `JudgeRunService`,
`MemoryRunService`, or `CompactRunService`. The generic part of the current Host
snapshot execution helper moves to, or is expressed through, the neutral Runtime
execution seam; Host code retains only backend selection, durable ingress, live
context construction, and composition.

Business-valued `RunPurpose::{OutcomeWorker,OutcomeGrader,Memory,Compact}` is
removed. An extension narrows a Run through neutral constraints such as fresh or
continued context and tool/network/workspace/delegation access. The effective
authority remains the intersection of snapshot-declared capability, platform
policy, and request narrowing. A narrowing can never grant capability.

### D5: Outcome state uses existing Worker Thread state

The Worker Thread is the consistency boundary. Exactly one Outcome may be active
on it. `awaken-ext-goal` owns typed serialization and version-guarded commits over
the existing `ThreadReader` and `CommitCoordinator`; there is no physical
`OutcomeStore` and no repository trait invented for a single implementation.

```text
outcome/active
outcome/{outcome_id}/definition
outcome/{outcome_id}/binding
outcome/{outcome_id}/state
outcome/{outcome_id}/evaluation/{iteration}
```

Definition and Worker/Grader snapshot bindings are immutable, the mutable head
is small and versioned, and Evaluations are append-only. Transitions require an
expected version and expected Run id. External IO never occurs while Thread
state is locked. Correctness rests on committed state and version guards, not a
Managed Session mutex or an in-process projection cursor.

### D6: Stable identities make cross-Thread recovery idempotent

```text
outcome/{outcome_id}/worker/{iteration}
outcome/{outcome_id}/grader/{iteration}
outcome/{outcome_id}/grader/{iteration}/run
outcome/{outcome_id}/ack
```

A Grader Thread is semantically fresh but durably addressable and inherits no
prior Judge conversation. If a process dies after a Run commits but before the
Outcome head advances, recovery observes the same terminal Run and applies the
missing version-guarded transition without another inference. No distributed
transaction between Worker and Grader Threads is required.

### D7: Grading is direct ordinary Run execution

The `Grader` application port accepts the immutable Judge
`ExecutableAgentSnapshot` stored in the Outcome binding plus `GradingInput`, and
returns:

```text
GradeDecision = Satisfied | NeedsRevision | Failed
Grade = decision + explanation
```

The controller always supplies the persisted snapshot rather than current
configuration. The Agent-backed implementation executes that Judge snapshot on
the stable fresh Grader Thread through the ordinary Run boundary. There are no
Native/ACP-specific Graders, configuration fallback Graders, or Judge `RawTool`
bridge.

`GradingInput` contains description, rubric, committed transcript, evaluated
message range, Worker state, and prepared `DeliverableEvidence`. The Judge has no
writable Worker workspace, shell, network, delegation, Memory write, or ordinary
MCP tools. These restrictions are enforced as neutral capability narrowing, not
only through prompt wording.

### D8: Memory Extraction observes committed terminal Runs

Memory Recall remains a `BeforeInference` Plugin and writes request-only context
to Run-scoped `ContextMessages`. Its query is derived from the current
`RunInput`, not guessed by scanning the last User message in the full Thread.

Memory Extraction moves out of the Host callback into `awaken-ext-memory` as a
`RunTerminalObserver`. Observation occurs only after `RunState::Ended` is
committed. It is delivered at least once and must not change the already
committed `RunResult`.

```text
terminal Run commit
  -> observe terminal fact
  -> CAS create memory-extraction/{thread_id}/{run_id} intent
  -> Extractor Agent Run
  -> CAS apply Memory mutations
  -> commit receipt
```

Recovery redelivers terminal observations and resumes pending intents. Duplicate
delivery is harmless because intent and receipt identities are stable. Awaiting
is not terminal and does not trigger extraction.

### D9: Local extensions and external injection are separate concerns

An embedded/local Runtime can install and use Memory Recall, Compact, Memory
Extraction, and Outcome without a Managed application.

An external Runtime such as an ACP process cannot load Awaken's Rust Plugins.
The service application therefore adapts an extension's prepared context into
external Run input and returns committed Run observations to the extension. ACP
and Runtime Core do not learn Memory, Compact, or Outcome vocabulary, and the
service does not reimplement their selection, fold, extraction, or iteration
rules.

External injection is an adapter concern and does not alter the local extension
lifecycle.

### D10: Protocol projection and cache remain outside correctness

`SessionRuntime::define_outcome` remains the Managed-facing port. The adapter
validates wire input and maps extension records to Managed events and reports. It
does not own iteration, Judge choice, recovery, or capability policy.

Worker history remains append-only on one Thread. Grader instructions and output
schema form a stable prefix, while each Evaluation uses a fresh semantic Thread.
Exact stable-id/input-hash reuse provides idempotency. Provider prompt/session
cache hits are best-effort optimizations and never replace committed truth.

## Consequences

- Outcome, Memory, and Compact can be used by an embedded Runtime without the
  Managed application.
- Native/ACP Worker and Grader combinations retain one ordinary Run lifecycle.
- Runtime Core and ACP remain free of extension and product vocabulary.
- Existing Thread truth, stable identities, recovery, E2E, and formal models are
  retained.
- One terminal observer seam replaces duplicated Host callbacks; it does not
  create a universal extension framework.
- External ACP Memory/Compact context projection remains an adapter capability
  and can be delivered independently of local extension correctness.

## Implementation slices

1. **P1 — documentation and vocabulary:** freeze this ownership model and use
   Run/Thread/Step throughout Runtime documentation and touched code.
2. **P2 — terminal lifecycle seam:** add `RunTerminalObserver`, one committed
   terminal notification path, at-least-once/idempotency tests, and no separate
   Stop/PostRun hooks.
3. **P3 — Memory ownership:** move extraction activation, cursor, intent, receipt,
   and recovery rules into `awaken-ext-memory`; leave Host as composition only.
4. **P4 — neutral Run execution:** remove business `RunPurpose`, no-op
   continuity, and Host-only auxiliary execution duplication while preserving
   capability narrowing.
5. **P5 — Outcome ownership:** move the controller, state codec, Agent Grader,
   and prompts into `awaken-ext-goal`; keep concrete backend/store adapters in the
   Host.
6. **P6 — cleanup and projection:** remove Host-owned Outcome/Memory lifecycle,
   legacy Goal guard/tool paths, duplicate names/DTOs, and update Managed
   projection, public API snapshots, formal models, and E2E coverage.

Every slice adds or migrates its tests and is committed only after those tests
pass.

## Verification

Executable cause/effect inventories, decision tables, backend combinations,
fault injection, and expected event sequences are owned only by comments beside
the Rust and official-SDK tests that execute them. This ADR does not restate
that executable design.

The [`OutcomeLifecycle`](../../formal/tla/OutcomeLifecycle.tla) model checks the
Outcome phase/result, stable-identity, budget, and acknowledgment invariants.
Formal obligation mapping and changed-line E2E coverage thresholds, reachability
waivers, and stale-waiver rejection remain owned by the repository's formal and
coverage CI scripts.

## Amendment (2026-08-22): Durable continuation and failure containment

This amendment clarifies D3, D5, D6, D9, and D10 without changing their
ownership.

`RunState::Awaiting` is an external-input boundary, not an Outcome business
result or infrastructure failure. The active aggregate remains committed on the
Worker Thread. An allow, deny, or client-tool result must resume the exact
pending ordinary Run; after that input commits, the controller reloads
`outcome/active` and drives the same aggregate again. The resumed Run may await
again or end and proceed to grading. With no active aggregate,
`resume_active` returns `None` and never reconstructs a definition. No protocol
continuation registry or process-local cursor participates in this path.

The crate boundary is a compile-time ownership fence:
`awaken-ext-goal` depends on neutral Run, Thread, and Grader ports, not on
Managed DTOs, Runtime Host, HTTP, or a concrete store. Runtime Host and Session
application compose those ports; the Managed adapter validates input and
projects committed facts. Lifecycle projection owns ordinary messages, tools,
and resume correlation. Outcome projection adds evaluation facts only and must
not emit those lifecycle facts a second time. A retained HTTP receipt is
acceptance, not completion; committed Thread state and terminal projection are
the oracle, never model prose or an in-process cache.

The following FMECA records residual risk after the controls in this ADR.
Severity (`S`), occurrence (`O`), and detection difficulty (`D`) use 1–5;
`RPN = S × O × D`.

| ID | Failure mode and end effect | S/O/D · RPN | Authoritative containment |
| --- | --- | --- | --- |
| OF1 | Awaiting is treated as request/runtime failure, so valid HITL emits `session.error` | 4/2/3 · 24 | D3's typed external boundary; project only committed lifecycle truth |
| OF2 | Resumed permission input commits but the controller is not driven again, leaving the aggregate live forever | 4/2/4 · 32 | one `resume_active` path reloads the durable active pointer after ordinary Run resume |
| OF3 | Lifecycle and Outcome projection both emit messages or tools, duplicating UI facts and drifting resume identity | 4/2/3 · 24 | D10's projection split: lifecycle facts once, Outcome evaluation facts only |
| OF4 | The extension requires Server composition and cannot run embedded | 4/2/2 · 16 | D3/D9 neutral-port dependency fence; Server layers remain optional adapters |
| OF5 | Continue without an active aggregate guesses a definition and creates a ghost workflow | 4/1/3 · 12 | absence returns `None`; only explicit definition creates an aggregate |
| OF6 | Repeated protected calls or deny results lose correlation, skip grading, or create another aggregate | 5/2/3 · 30 | exact pending Run identity and the same durable active pointer govern every boundary |
| OF7 | Replacement after Worker commit repeats inference or grades mutable current configuration | 5/1/3 · 15 | D6/D7 stable Run identities, immutable binding, version guard, and committed-state replay |

All containment is fail-closed. Resume mismatches are rejected before side
effects; persistence, execution, and parsing faults remain typed infrastructure
errors; and no mitigation introduces a second status, transcript, cursor,
Outcome repository, or fallback snapshot.
