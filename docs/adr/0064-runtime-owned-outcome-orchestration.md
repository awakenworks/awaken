# ADR-0064: Runtime-Owned Outcome Orchestration over the Unified Run Boundary

- Status: Accepted
- Date: 2026-07-22
- Builds on: [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md)
  (thread truth), [ADR-0040](0040-server-durable-ingress-integration.md)
  (durable Run execution), [ADR-0055](0055-typed-state-kernel-loop-actions-as-state.md)
  (typed state), [ADR-0057](0057-unified-agent-configuration.md)
  (executable snapshots), and
  [ADR-0059](0059-neutral-core-and-leaf-evolution.md) (neutral core boundaries)
- Supersedes:
  - the Managed `define_outcome` implementation that constructs a Native-only
    `GoalPlugin` runtime;
  - `AgentToolGrader` and the Judge-specific `RawTool` bridge;
  - the in-memory `consumed_rounds` projection cursor and thread-derived Outcome id.

## Context

Managed Agents exposes `user.define_outcome`: an Agent works toward a described
deliverable, a separately-contextualized Grader evaluates it against a rubric,
and feedback drives bounded revisions. Awaken currently implements that behavior
inside one Native Run through `GoalGuard`. Ordinary Session turns, however, route
through the neutral `RunExecutor` boundary and may execute on Native, ACP, or A2A.
The current Outcome therefore bypasses the selected runtime, durable ingress,
normal tool policy, and the shared Agent lifecycle. Its Judge is called through a
`RawTool` adapter that ultimately constructs another Native runtime.

The design must support every Worker/Grader pairing, recover after process death,
preserve one source of thread truth, keep the generic Runtime unaware of Outcome
vocabulary, and avoid introducing parallel Run services or a second persistence
system.

## Decision

### D1: One Outcome vocabulary, layered from generic Run vocabulary

The generic execution vocabulary remains `Run`, `RunState`, `RunResult`,
`RunExecutor`, `Thread`, and `Session`. It does not gain Goal, Outcome, Rubric,
Grader, or Judge concepts.

The Outcome bounded context uses `Outcome`, `Description`, `Rubric`, `Iteration`,
`Evaluation`, `Grader`, `Grade`, `Revision`, `Deliverable`, and `Artifact`.
`Grader` is the application port; `AgentGrader` is the implementation that runs a
separately configured Judge Agent. External products that call the intent a Goal
translate it at their adapter into `OutcomeDefinition.description`. No bare Rust
`Outcome` type is introduced: the types are qualified as `outcome::Definition`,
`outcome::State`, `outcome::Phase`, and `outcome::Evaluation`, avoiding collision
with operational results such as `GateOutcome` and `DispatchOutcome`.

### D2: Outcome is a Runtime Host application state machine

There is one normative loop, owned by a concrete `OutcomeController` in
`awaken-runtime-host`. Native, ACP, and A2A execute individual Runs only. Managed
and other wire adapters submit a command and project neutral results/events only.
ACP gains no Outcome-specific method.

The state machine is:

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

`Failed` is the Managed business result for a rubric that does not apply to the
task/deliverable. Judge launch, execution, parsing, persistence, and Worker faults
are typed infrastructure failures and are never folded into that business result.

### D3: Managed iteration semantics are exact

`iteration` is the zero-based revision counter: zero is the first evaluation of
the initial Worker result; one is the evaluation after the first revision.
`max_iterations` is the maximum number of graded iteration loops and must be in
`1..=20`. When the last allowed evaluation still needs revision, the evaluation
result is `max_iterations_reached`, one ungraded acknowledgment Worker turn runs,
and the Session then becomes idle. Thus `max_iterations = 3` permits evaluations
0, 1, and 2, two ordinary revisions, and one final acknowledgment.

### D4: One snapshot execution entry, no second Run service

`SharedHost::execute_snapshot(SnapshotRunRequest)` is the sole application entry
for executing a pinned `ExecutableAgentSnapshot`. It constructs `RunActivation`
and `RuntimeRunContext`, selects the Native/ACP/A2A `RunExecutor`, applies the
purpose policy, drives the Run, and returns its committed message range, state,
and usage.

`SnapshotRunRequest` carries a stable Run id, Thread id, snapshot, input,
`Continuity::{Continue,Fresh}`, and
`RunPurpose::{UserTurn,OutcomeWorker,OutcomeGrader,Memory,Compact}`. This is a
concrete Host facility, not a new `AgentRunService` trait. The existing
`RunExecutor` remains the only backend execution port.

### D5: Outcome state is existing Worker Thread state

The Worker Thread is the consistency boundary. Exactly one Outcome may be active
on it. A concrete `ThreadOutcomeState` adapter owns typed serialization and CAS
over the existing `ThreadReader` and `CommitCoordinator`; there is no new physical
Outcome store and no speculative repository trait.

```text
outcome/active
outcome/{outcome_id}/definition
outcome/{outcome_id}/binding
outcome/{outcome_id}/state
outcome/{outcome_id}/evaluation/{iteration}
```

The definition, immutable Worker/Grader snapshot bindings, small mutable head, and
append-only evaluations are separate cells. Transitions carry an expected version
and expected Run id. External IO never occurs while the thread state lock is held.

### D6: Stable identities make cross-Thread recovery idempotent

Worker and Grader Runs use deterministic identities:

```text
outcome/{outcome_id}/worker/{iteration}
outcome/{outcome_id}/grader/{iteration}
outcome/{outcome_id}/grader/{iteration}/run
outcome/{outcome_id}/ack
```

A Grader Thread is semantically fresh but durably addressable. It inherits no
prior Judge conversation. If a process dies after a Run commits but before the
Worker Outcome head advances, recovery observes the same terminal Run and applies
the missing CAS transition without paying for another inference. No distributed
transaction between Worker and Grader Threads is required.

### D7: Grading is direct snapshot execution

The single `Grader` port accepts `GradingInput` and returns `Grade`:

```text
GradeDecision = Satisfied | NeedsRevision | Failed
Grade = decision + explanation
```

`AgentGrader` executes its pinned Judge snapshot through `execute_snapshot` with
fresh continuity and `RunPurpose::OutcomeGrader`. `KeywordGrader` remains the
deterministic implementation. There are no Native/ACP-specific Graders and no
Judge `RawTool` bridge.

`GradingInput` contains description, rubric, committed transcript, evaluated
message range, Worker state, and a list of prepared `DeliverableEvidence`. V1
always supplies transcript/tool-result evidence and may supply text/file metadata
already available to the Host. Complex binary semantic extraction is additive;
the Judge never receives a writable Worker workspace merely to discover evidence.

### D8: Purpose policy is a fail-closed intersection

The effective execution policy is:

```text
snapshot-declared capability
  intersect platform policy for RunPurpose
  intersect request narrowing
```

Outcome Worker Runs continue the original Worker Thread and retain ordinary
Worker policy; they are never unconditionally auto-approved. Outcome Grader Runs
have a fresh context, no writable Worker workspace, shell, network, delegation,
memory write, or ordinary MCP tools. Native and ACP enforce the same decision at
their execution/sandbox/permission boundaries, not only through prompt wording or
empty tool descriptors.

### D9: Cache is an optimization, never correctness state

Worker history remains append-only on one Thread, and ACP retains its stable
session home, enabling provider prefix/session reuse. Grader instructions and
output schema form a stable prefix while each evaluation uses a fresh semantic
Thread. Exact `(grader_run_id, input_hash)` reuse is idempotency; provider prompt
cache hits are best effort and never substitute for committed truth.

### D10: Protocol projection remains at the adapter

`SessionRuntime::define_outcome` remains the Managed-facing port. The adapter
validates the wire request and maps Runtime records to
`span.outcome_evaluation_start|ongoing|end`, Session running/idle, interrupt, and
`OutcomeReport`. It does not own iteration, Judge choice, recovery, or policy.

## Consequences

- Every Native/ACP Worker and Native/ACP Grader combination follows one lifecycle.
- Outcome survives restart using the same truth and durability as its Worker
  Thread.
- The generic Runtime and ACP protocol remain free of Outcome concepts.
- One extra application state machine and typed Thread-state adapter replace two
  special execution paths and an in-memory projection cursor.
- Binary artifact inspection requires deterministic evidence preparation before
  full fidelity can be claimed for formats such as spreadsheets; this does not
  change the Grader or Outcome contracts.

## Implementation slices

1. **P1** — add snapshot execution, backend routing, capability selection, and
   purpose policy; migrate ordinary turns to it.
2. **P2** — add the pure Outcome state machine and cause-effect/decision-table
   unit tests.
3. **P3** — add Thread-state persistence, CAS, stable identities, and recovery.
4. **P4** — add `GradingInput`, direct `AgentGrader`, schema parsing, and
   Native/ACP isolation tests.
5. **P5** — wire `OutcomeController`, Managed events, interruption,
   max-iteration acknowledgment, and recovery.
6. **P6** — remove GoalGuard/GoalPlugin, AgentToolGrader, Judge RawTool path,
   consumed-round cursor, duplicate DTO/router code, and retire the Native-only
   helper after all remaining consumers migrate.

Every slice adds its tests and is committed only after those tests pass.

## Verification

Required Worker/Grader matrix:

| Worker | Grader |
|---|---|
| Native | Native |
| Native | ACP |
| ACP | Native |
| ACP | ACP |

The decision-table suite covers satisfaction, revision, maximum budget and
acknowledgment, rubric mismatch, invalid Judge output, Worker/Grader failure,
interrupt in every live phase, stale CAS, duplicate commands, restart at every
external-IO boundary, snapshot pinning, message-range projection, usage
idempotency, and Grader capability denial.

The final delivery also adds a formal state-machine model and deterministic
TypeScript E2E scenarios using the official Managed client. E2E exercises the
complete `user.define_outcome` lifecycle, Native/ACP routes, recovery, and event
projection. Changed executable Rust lines introduced by this work must exceed
95% E2E line coverage under `scripts/ci/e2e-coverage.sh`; exclusions require an
explicit reachability justification and may not hide Outcome application code.
