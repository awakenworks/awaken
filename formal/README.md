# Formal verification

The verification stack covers the durable Runtime boundary, tool batches,
first-class delegated child Runs, and durable dispatch. WorkQueue remains a
separate aggregate and is outside these models.

The implementation has one durable source of truth. `RunDelegations` stores
parent/call/child identity, lineage, budgets, and cancellation intent;
`ActiveToolBatch` stores tool execution, approval, recovery, and result state.
Both are Run-scoped cells written atomically in the ordinary
`ThreadCommit.state` log. There is no `DelegationStore` or tool-specific state
repository.

## Kani production-kernel proofs

Eleven harnesses invoke production pure functions directly:

- `awaken-agent-contract`
  - an ended Run is absorbing;
  - legacy wire state/ticket pairs enter exactly the legal typed
    `RunDisposition` variants;
  - only open or cancellation-requested relationships occupy delegation
    parallel capacity;
  - every delegation relationship effect has its unique documented
    precondition.
- `awaken-session-contract`
  - awaiting outcomes cannot be terminal or failed;
  - ended outcomes carry the only failure authority and no pending tool.
- `awaken-runtime-contract`
  - terminal tool calls never re-enter execution;
  - only a matching approval ticket enters execution;
  - every tool-call transition has its unique documented precondition;
  - terminal calls accept result staging without reopening execution;
  - ending a Run seals exactly the non-terminal calls.

The relationship and tool-call harnesses verify the same transition kernels
used by `DelegationRegistry` and `ToolBatch`; they are not copies of the
production logic.

## TLA+ specifications

- `RunIngress.tla` covers claim, lease-epoch fencing, crash reclaim,
  await/wake, cancellation, dead-letter, requeue, and supersession.
- `Delegation.tla` covers stable child identity, local/remote lifecycle
  equivalence, independent owner/epoch recovery, depth/cycle/parallel/total
  budgets, completion, and durable cancellation intent.
- `ToolBatch.tla` covers parallel model-emitted calls, approval and supplied
  results, executor-entry ordering, replay-safe and fail-closed recovery,
  cancellation, and the whole-batch publication barrier.
- `RuntimeSystem.tla` composes dispatch, tool batch, approval, delegation,
  messages, cancellation, and terminal commit under shared variables.
- `RuntimeImplementation.tla` splits every durable abstract transition into a
  prepare/apply protocol. A crash may discard a prepared operation; prepare and
  crash refine to stuttering, while every apply/invoke step refines to one
  `RuntimeSystem` action.
- `RustCommitSystem.tla` is the durable projection reconstructible from actual
  production `ThreadCommit`s: Run state/ticket, `ActiveToolBatch`, and
  `RunDelegations`. Its binary `NextState(s, t)` relation is also the checker for
  executable Rust traces.

`RuntimeVocabulary.tla` is the shared closed vocabulary, preventing component
models from inventing incompatible aliases for the same lifecycle state.

## TLAPS proofs

`RuntimeSystemProof.tla` proves `Safety` inductive, including:

- Run/dispatch and Run/ticket coherence;
- committed-before-invoke and monotonic attempt accounting;
- approval cannot be bypassed and denied calls never execute;
- terminal call absorption and the whole-batch publication barrier;
- child completion is owned by the corresponding tool call;
- open child relationships name live children;
- parent termination durably records child cancellation intent;
- messages cannot manufacture approval;
- ended Runs are absorbing.

`RuntimeImplementationProof.tla` proves the temporal refinement
`RuntimeImplementation!Spec => RuntimeSystem!Spec` under the explicit
projection that forgets the prepared-operation buffer.

`RustCommitSystemProof.tla` proves the production-commit projection's `Safety`
invariant inductive for arbitrary constants satisfying its assumptions. This
includes ticket/call coherence, the publication barrier, attempt bounds,
delegation ownership, and terminal sealing.

At the current source revision TLAPS discharges all obligations:

- Runtime system safety: 143/143.
- Implementation refinement: 108/108.
- Rust commit projection safety: 46/46.

## TLC exhaustive finite checks

The checked configurations currently reach the following complete finite state
graphs with zero invariant violations and zero states left on the queue:

| Model | Generated | Distinct | Max depth |
| --- | ---: | ---: | ---: |
| RunIngress | 339 | 31 | 7 |
| Delegation | 2,593 | 512 | 14 |
| ToolBatch | 1,414 | 979 | 12 |
| RuntimeSystem | 110,923 | 12,896 | 13 |
| RuntimeImplementation | 1,323,147 | 619,008 | 24 |
| RustCommitSystem | 4,446 | 1,277 | 9 |

These are bounded exhaustive checks, not unbounded liveness proofs. The bounds
are explicit in the corresponding `.cfg` files.

## Executable Rust refinement bridge

`formal_refinement.rs` drives the real async `Runtime` through a coordinator
that wraps the ordinary in-memory commit implementation. It records only
successful `ThreadCommit`s and projects each committed state from the same
public state keys used during recovery. At executor entry the tests additionally
assert that the corresponding `Executing` call is already committed; delegated
dispatch also asserts that its stable child relationship is already `Open`.

The renderer converts these production traces to TLA+ values. TLC then evaluates
`RustCommitSystem!TraceIsRefinement`, requiring the formal initial state, `Safety`
at every point, and one exact `NextState` transition between every adjacent pair.
The checked scenarios are:

- a two-call model-emitted batch and its whole-batch publication barrier;
- permission suspension and correlated approval resume;
- delegated child dispatch, durable wait, and parent cancellation;
- crash recovery of a delegated `DurableRequest`, reusing and completing the
  same committed child relationship;
- crash recovery of an `Executing` replay-safe call, including the incremented
  attempt committed before executor re-entry;
- crash recovery of an `Executing` never-replay call, proving fail-closed
  `Indeterminate` completion without executor re-entry.

The production bridge remains layered around that direct trace check:

1. Kani universally checks the finite production Rust transition kernels.
2. TLAPS proves the commit projection's safety for every modeled transition,
   not only the six captured traces.
3. Runtime tests cover the typed `RunDelegations` and `ActiveToolBatch` cells
   through actual async dispatch, approval, resume, cancellation, and recovery.
4. The lower-layer store conformance suite commits those same state addresses
   in one `ThreadCommit`, verifies crash recovery and stale post-terminal
   fencing, and runs against in-memory, filesystem, SQLite, and PostgreSQL
   adapters when the PostgreSQL test service is available.

## Running locally and from CI

Install Kani, TLAPS, Java 11+, and obtain `tla2tools.jar`, then run:

```sh
TLA2TOOLS_JAR=/path/to/tla2tools.jar \
TLAPM_BIN=/path/to/tlapm \
scripts/ci/check_formal.sh --require-tools
```

The script names every Kani harness and uses isolated TLC state directories so
fast sequential models cannot collide on TLC's timestamp-based default path. It
also regenerates all executable traces from the current Rust source before TLC
checks them; no checked-in hand-authored trace can become stale.

The repository-wide CI entry point contains an explicit formal-verification
gate, but leaves it disabled by default because installing the three provers and
exploring the larger TLC graph are intentionally local/on-demand work. A
prover-equipped CI runner can opt in without changing the pipeline:

```sh
AWAKEN_RUN_FORMAL=1 scripts/ci/check-all.sh
```

Opt-in is strict: `check-all.sh` passes `--require-tools`, so a requested formal
run fails instead of silently skipping a missing prover. Ordinary
`scripts/ci/check-all.sh` prints the skipped gate and does not execute formal
verification.

## Honest boundary

The executable bridge machine-checks the covered real async executions, but is
not a compiler theorem that every possible Rust scheduler, adapter, network, or
database execution refines the TLA+ model. The current proof also does not
establish liveness, fairness, eventual network recovery, eventual external
input, or eventual tool-result arrival. External tool side effects, remote
protocol implementations, database engines, and unbounded state spaces remain
outside the state-machine proof. They require idempotency contracts, adapter
integration tests, fault injection, and operational reconciliation; a larger
finite TLC bound alone cannot prove them.

`CancelRequested` is the terminal parent commit's durable, idempotent
relationship fact. It intentionally has no later `Cancelled` acknowledgement:
the commit stores reject every post-terminal mutation. Actual in-flight parent
to child cancellation is delivered by the one-way child cancellation token;
the A2A adapter maps that token to `tasks:cancel`. A child Run's own
`EndCause::Cancelled` remains its ordinary Run lifecycle outcome and is not a
delegation relationship status.
