# Formal verification

The verification stack covers the durable Runtime boundary, tool batches,
first-class delegated child Runs, durable Run ingress, and the separate Managed
self-hosted-environment `WorkQueue` aggregate.

The implementation has one durable source of truth. `RunDelegations` stores
parent/call/child identity, lineage, budgets, and cancellation intent;
`PendingChildRunResults` stores only child results not yet consumed by the parent;
`ActiveToolBatch` stores tool execution, approval, recovery, and result state.
All are Run-scoped cells written through the ordinary
`ThreadCommit.state` log. There is no `DelegationStore` or tool-specific state
repository. A delivered result is removed in the same parent commit that makes
the corresponding ToolBatch call terminal and completes its relationship.

## Kani production-kernel proofs

Twenty harnesses invoke production pure functions directly:

- `awaken-agent-contract`
  - an ended Run is absorbing;
  - legacy wire state/ticket pairs enter exactly the legal typed
    `RunDisposition` variants;
  - only open or cancellation-requested relationships occupy delegation
    parallel capacity;
  - every delegation relationship effect has its unique documented
    precondition.
  - delegation admission requires the parent lifecycle, depth, lineage,
    parallel, and total-budget guards simultaneously.
  - cancellation delivery is enabled only by a durable cancellation-requested
    relationship.
- `awaken-session-contract`
  - awaiting outcomes cannot be terminal or failed;
  - ended outcomes carry the only failure authority and no pending tool.
  - only queued WorkQueue items are claimable;
  - only active WorkQueue items accept lease extension;
  - stopping a WorkQueue item is absorbing.
  - the first heartbeat receipt is authorized once and every later heartbeat
    requires the matching receipt.
- `awaken-runtime-contract`
  - terminal tool calls never re-enter execution;
  - only a matching approval ticket enters execution;
  - every tool-call transition has its unique documented precondition;
  - terminal calls accept result staging without reopening execution;
  - ending a Run seals exactly the non-terminal calls.
  - a child result is consumable only from `Ready`;
  - consumed or discarded delivery phases never reopen.

The relationship and tool-call harnesses verify the same transition kernels
used by `DelegationRegistry` and `ToolBatch`; they are not copies of the
production logic.

## TLA+ specifications

- `RunIngress.tla` covers claim, lease-epoch fencing, crash reclaim,
  await/wake, cancellation, dead-letter, requeue, and supersession.
- `WorkQueue.tla` covers the distinct Managed environment queue: transactional
  single-active claim, durable owner/epoch/expiry, exact-boundary reclaim,
  heartbeat, acknowledgement, stop, and environment removal.
- `Delegation.tla` covers stable child identity, local/remote lifecycle
  equivalence, atomic child admission and input-delivery claims, independent
  owner/epoch recovery, depth/cycle/parallel/total
  budgets, the distinct child-ended/result-ready/parent-consumed boundaries,
  late-result discard, durable cancellation intent, and delivery-after-intent.
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

`WorkQueueProof.tla` separately proves the Managed queue's type safety,
single-active invariant, lease authority, positive active epoch, and terminal
lease clearing for every modeled transition.

At the current source revision TLAPS discharges all obligations:

- Runtime system safety: 143/143.
- Implementation refinement: 108/108.
- Rust commit projection safety: 49/49.
- Managed WorkQueue safety: 35/35.

## TLC exhaustive finite checks

The checked configurations currently reach the following complete finite state
graphs with zero invariant violations and zero states left on the queue:

| Model | Generated | Distinct | Max depth |
| --- | ---: | ---: | ---: |
| RunIngress | 339 | 31 | 7 |
| WorkQueue | 131,475 | 12,484 | 15 |
| Delegation | 7,867 | 1,413 | 16 |
| ToolBatch | 1,414 | 979 | 12 |
| RuntimeSystem | 110,923 | 12,896 | 13 |
| RuntimeImplementation | 1,323,147 | 619,008 | 24 |
| RustCommitSystem | 4,494 | 1,277 | 9 |

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
- two terminal child Runs whose relationships/executor entries commit together
  before their concurrent execution and ordered result publication;
- permission suspension and correlated approval resume;
- delegated child dispatch, durable wait, and parent cancellation;
- crash recovery of a delegated `DurableRequest`, reusing and completing the
  same committed child relationship;
- a retryable child-owner crash, reconnecting the same child Run and incrementing
  the committed executor attempt before re-entry;
- a crash after durable child-result delivery but before parent consumption,
  proving recovery consumes the result without invoking the child again;
- crash recovery of an `Executing` replay-safe call, including the incremented
  attempt committed before executor re-entry;
- crash recovery of an `Executing` never-replay call, proving fail-closed
  `Indeterminate` completion without executor re-entry.

The production bridge remains layered around that direct trace check:

1. Kani universally checks the finite production Rust transition kernels.
2. TLAPS proves the commit projection's safety for every modeled transition,
   not only the nine captured traces.
3. Runtime tests cover the typed `RunDelegations`, `PendingChildRunResults`, and
   `ActiveToolBatch` cells
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

The repository-wide CI entry point runs the strict formal-verification gate by
default. A deliberately reduced local run may skip it explicitly:

```sh
AWAKEN_SKIP_FORMAL=1 scripts/ci/check-all.sh
```

The default is strict: `check-all.sh` passes `--require-tools`, so missing Kani,
TLAPS, Java, or `tla2tools.jar` fails instead of producing a false green.

`formal/coverage.json` is the versioned obligation ledger. The CI gate verifies
that every evidence path exists and that at least 70% of formalizable safety
obligations have a machine-checked production link. The current ledger is
28/28, or 100%. Environmental properties are listed separately and never
silently omitted or mislabeled as machine-linked merely to raise the percentage.

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

`RunIngress.tla` models claim fencing as one atomic state transition. Production
now keeps the exact epoch guard live across the actual `ThreadCommit` for the
local durable backends: PostgreSQL holds a locked dispatch-row transaction and
SQLite serializes the singleton store's dispatch authority through the commit.
Database-less workers use one claimed-commit HTTP operation carrying the full
`RunClaim` (`run_id`, owner, epoch). The store-owning server holds that exact
claim stable while applying the real `ThreadCommit`; remote adapters cannot
degrade to a check-then-commit sequence. Custom remote queues fail closed unless
they supply the same atomic `ClaimedRunCommit` capability.

Likewise, `Delegation.tla` specifies the target semantics for independently
owned local and remote child Runs. The executable bridge now proves stable child
identity, durable relationship recovery, durable result delivery, exactly-once
parent consumption, terminal publication, and cancellation intent for the
current runtime path. Terminal-only local children emitted in one tool batch run
concurrently after their relationship/executor-entry commit; children that may
ask the parent for input stay on the single-ticket path so correlation is never
collapsed. Remote cancellation stores its opaque task reference with the
relationship, delivers only after the parent terminal commit, and is redelivered
when a process rebuilds or re-enters the session. Eventual success across an
unavailable network remains an environmental liveness property, not a safety
claim. Native children now enter the same durable dispatch queue as ordinary
Runs with their own stable identity, exact-target claim, lease epoch, and crash
reclaim rules. A session-thread route returns recovery to the parent session's
runtime and commit capabilities without changing the child's first-class Thread
identity. Store conformance and a durable host integration test link this path
to the independently owned child state modeled by `Delegation.tla`.

The Managed WorkQueue proof covers queue state, transactional single-active
claim, lease epoch/expiry, heartbeat compare-and-set, exact-boundary reclaim,
and terminal authority clearing. The official worker header is persisted as
the claim owner, including before the first heartbeat; only that worker may
advance the `first`/`matching last_heartbeat` condition. The in-memory, SQLite,
and PostgreSQL conformance paths exercise the same ownership rule. Stop remains
a control-plane operation rather than worker authority, and exactly-once
external work effects remain outside the queue proof.

`CancelRequested` is the terminal parent commit's durable, idempotent
relationship fact. It intentionally has no later `Cancelled` acknowledgement:
the commit stores reject every post-terminal mutation. Live cancellation uses
the one-way child token; recovery uses the same relationship as an outbox and
the A2A adapter maps its persisted task reference to `tasks:cancel`. A child Run's own
`EndCause::Cancelled` remains its ordinary Run lifecycle outcome and is not a
delegation relationship status.
