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

The named harnesses in the strict gate invoke production pure functions directly:

- `awaken-acp-contract`
  - ACP capability detection is authorized exactly by a live `Verified`
    observation; missing, unavailable, and failed probes remain false.
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
  - all seven Session realization Control failures map to the exact closed
    disposition table; Runtime Worker effects map NotReady to defer, Retryable
    to relinquish, and Terminal to absorbing settlement, while renewal retires
    exactly terminal truth.
  - awaiting outcomes cannot be terminal or failed;
  - ended outcomes carry the only failure authority and no pending tool.
  - only queued WorkQueue items are claimable;
  - only active WorkQueue items accept lease extension;
  - stopping a WorkQueue item is absorbing.
  - the first heartbeat receipt is authorized once and every later heartbeat
    requires the matching receipt.
  - stopped Session Work is revived only by a claimed nonterminal Run after an
    initial acquisition found no lease; realization renewal remains inert.
  - Session cleanup advances only through
    `NotRequested -> Fenced -> Requested -> Completed`, and every identity,
    and canonical artifact-completion axis is mandatory. Adapter-returned
    booleans are deliberately not accepted as independent proof evidence.
  - a first Delete request always hides, terminalizes when needed, and requests
    cleanup together; Deleting/Deleted replays are inert.
  - a Session tombstone is admitted only for a hidden, terminal aggregate with
    verified cleanup completion.
- `awaken-runtime-contract`
  - terminal tool calls never re-enter execution;
  - only a matching approval ticket enters execution;
  - every tool-call transition has its unique documented precondition;
  - terminal calls accept result staging without reopening execution;
  - ending a Run seals exactly the non-terminal calls.
  - a child result is consumable only from `Ready`;
  - consumed or discarded delivery phases never reopen.
  - a rejected live-inbox reorder leaves the exact original queue, while an
    accepted reorder equals the complete requested permutation;
  - live-inbox identity advances strictly or reports finite-space exhaustion,
    and therefore never wraps into a retired identity.
- `awaken-tenancy`
  - successful resolution cannot widen authority;
  - every uncovered selector fails closed;
  - selector ordering cannot authorize disagreement.
- `awaken-provisioning-contract`
  - reap causes obey the fixed fail-closed priority.
  - sandbox admission preserves the isolation floor and every requested
    capability;
  - fail-closed sandbox policy never authorizes a downgrade.
- `awaken-data-subject-application`
  - any withdrawal vetoes full-content capture;
  - purpose upsert retains exactly one incoming-purpose row;
  - erasure withdrawal is absorbing and idempotent.
  - revision advance is strict or explicitly exhausted; it never saturates.
- `awaken-credential-vault`
  - disabled, cooling, and exhausted pool members remain ineligible;
  - a pool with no eligible member fails closed.
  - Managed Vault child insertion requires an active same-Workspace parent,
    remaining aggregate capacity, and no duplicate active environment key;
  - Managed Vault replacement requires the exact observed revision and its
    strict successor, so stale writers cannot silently overwrite each other.
  - Managed Vault delete selectors require a stable monotonic root identity,
    an absorbing completed tombstone, and an absorbing child delete shape.
    The multi-step workflow is model-linked below; these Kani selectors do not
    prove its database, SecretStore, or rollout adapters.
- `awaken-credential-contract`
  - an issued recipient-bound envelope is accepted exactly when its reference,
    payload fingerprint, recipient, and plaintext boundary all match the
    complete selected request.
  - Environment credential custody selects ACP, installed hosted Cloud, or
    self-hosted Native profile from the exact complete input relation.
- `awaken-run-ingress-contract`
  - a Worker replaces a frozen Session Resource manifest exactly when it is a
    non-replay, same-Workspace generation with a strictly greater revision;
    stale, conflicting, and cross-Workspace generations are rejected.
- `awaken-credential-materializer`
  - every Vault source revision is positive, and an exact material reference is
    accepted only when its pin equals that source revision; an unpinned legacy
    reference cannot bypass invalid persisted revision state.
- `awaken-executable-agent-contract`
  - an explicitly requested Agent profile is accepted only when both revisions
    are positive and the returned immutable profile revision exactly matches;
    a custom profile source cannot substitute mutable current selection.
- `awaken-session-contract`
  - a resolved Environment snapshot must retain the selected identity and a
    positive revision; a publication-pinned resolution must also return the
    exact requested revision before the snapshot can be frozen.
  - a Worker accepts a frozen Agent publication only when Agent identity,
    revision, and runtime identity all match; missing/conflicting pins fail closed.
- `awaken-file-store`
  - object-store allocation accepts exactly a nonempty normalized bucket/prefix
    shape with a nonblank S3 region or with no S3-only coordinates for GCS.
- `awaken-protocol-acp`
  - MCP alias permission consensus is a conservative, commutative, associative,
    and idempotent semilattice with deny absorption and allow identity.
- `awaken-sandbox-container`
  - every retained writable root shares exactly one claim slot while receiving
    a distinct subpath slot; the ephemeral fallback assigns distinct volume
    slots instead.
  - PVC allocation and final Pod projection share one exact selector, so an
    ephemeral filesystem never allocates or references a continuation claim.
- `awaken-worker-contract`
  - non-ready workers never accept work and accepted protocol versions are in range;
  - `NeverReplace` rejects every replacement;
  - sandbox-continuity replacement is authorized exactly when a binding exists;
  - the same incarnation never spends replacement authority.
  - sandbox-tool recovery is a hard claim axis, manifest recovery matches the
    installed executor, readiness is probe-independent, and unpublished dynamic
    evidence can only restrict admission.
- `awaken-authorization-contract`
  - hosted admin, workspace member, and runtime member grants equal their exact
    closed sets; legacy workspace migration is idempotent and authority exact.
- `awaken-protocol-managed` / `awaken-runtime-host`
  - only terminal cleanup bypasses a retired Agent publication, and MCP
    credential realization preserves the authored target exactly.
  - a Session Hand can become ready only through a tracked `Starting` phase;
    cancellation of the waiting request does not transfer process ownership.
  - a container read-only tree is publishable only after its safe staging tree
    is complete and restricted; the external filesystem rename and rollback
    behavior remains an adapter assumption.
- `awaken-provider-genai`
  - neutral content categories project exactly per provider dialect: Anthropic
    retains each opaque thinking text/signature pair in order while other
    dialects use normalized reasoning. A reasoning-like-only row is not
    replayable, reasoning plus a complete tool/public part remains one turn,
    and scalar reasoning is prepended exactly once by the same streaming and
    non-streaming table. SDK serialization, signature authenticity, transport
    completeness, payload meaning, and provider acceptance remain outside this
    proof.
- `awaken-store-schema`
  - migration versions are dense and strictly increasing;
  - each step advances at most one version and never rolls back;
  - replaying a fully applied plan is a no-op.
- `awaken-ext-compact`
  - folding preserves the requested suffix;
  - the fold trigger is exact and total.
- `awaken-ext-goal`
  - every accepted Grade selects exactly the decision- and budget-authorized phase;
  - terminal Outcome states are absorbing.

The Outcome controller and Thread-state codec belong to the Outcome Runtime
Extension. Runtime Host adapters are not part of the formal domain transition
kernel. The committed-terminal observer slice additionally requires executable
tests for after-commit ordering, Awaiting exclusion, redelivery, and stable
intent/receipt idempotency; it must not be modeled as a Step hook or continuation
decision.

The relationship and tool-call harnesses verify the same transition kernels
used by `DelegationRegistry` and `ToolBatch`; they are not copies of the
production logic.

## TLA+ specifications

- `RunIngressKernel.tla` is the single parameterized Dispatch transition
  authority. `RunIngress.tla` is its standalone exhaustive instance:
  reservation publication (relative TTL is converted by the store authority),
  claim/lease-epoch fencing, crash reclaim, pending-input delivery,
  cancellation, settlement, dead-letter, requeue, and supersession. It never
  mutates Run disposition or tickets. `SessionRunProtocol.tla` directly
  instances that same kernel for a root Run and two child Runs. It also
  instances `SessionActivityKernel.tla` and `WorkQueueKernel.tla`, adding only
  cross-authority ordering for overlapping Session activities, one physical
  Work owner, per-Thread execution exclusion, realization, committed Thread
  observation, and settlement. A Work handoff may leave a physically running
  stale attempt, but that attempt cannot produce an authoritative observation.
- `ThreadState.tla` covers atomic typed-state batch admission, exact Run-scope
  binding, crash/rejection stuttering, version advancement, deterministic
  materialization, and replay equality. The Rust state selectors are the same
  functions consumed by `ThreadCommit::validate`, `ThreadCommit::assemble`, and
  `Store::rebuild`; Kani and a real in-memory commit test link those boundaries.
- `WorkQueueKernel.tla` is the single Managed environment queue transition
  authority: transactional single-active claim, durable owner/epoch/expiry,
  exact-boundary reclaim, heartbeat, acknowledgement, exact-epoch stop, and
  environment removal. `WorkQueue.tla` is its standalone bounded instance.
- `SessionActivityKernel.tla` owns stable operation-to-activity receipts and
  exact settlement. `SessionRootKernel.tla` owns atomic complete-root creation,
  exact create replay classification, realization fencing, terminal absorption,
  and disposable Work projection. Their standalone instances have independent
  TLC and TLAPS gates.
- `SessionEventProtocol.tla` composes the activity and Dispatch kernels for one
  initial Event without collapsing the initial wake activity into the distinct
  Run activity. `SessionStartProtocol.tla` composes Session root, WorkQueue,
  realization, Event, Run admission, observed ThreadCommit, Session settlement,
  Dispatch settlement, and exact Work release. Its ordinary configuration
  explores faults and replays; its reachability configuration proves the
  fault-free full path can reach Idle under weak fairness.
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
- `RemoteTool.tla`, `SessionOwnership.tla`, `CircuitBreaker.tla`, `AggregateCAS.tla`,
  and `DeploymentCAS.tla` cover durable operation identity, atomic ownership,
  generation-fenced permits, optimistic concurrency, transactionally bounded
  scheduled capacity, and revision-fenced scheduled-occurrence claims.
- `RemoteAttempt.tla` covers the A2A root-attempt boundary: stable replay message
  identity across the external-send/local-commit crash window, durable task and
  endpoint pinning, reattachment without resend after the reference commit,
  input/resume continuity, fail-closed endpoint mismatch, cancellation of the
  pinned task, and terminal reference retirement. Remote peer deduplication is
  deliberately retained as an external idempotency assumption.
- `AuthzKernel.tla`, `LiveInbox.tla`, and `CheckpointRecovery.tla` cover total
  request classification, ordered editable process-local input (including
  atomic exact-permutation reorder, replace, remove, drain, and close), and
  crash-safe streaming watermark/checkpoint behavior. `LiveInboxProof.tla`
  proves that reorder has no partially applied outcome and every invalid
  request is a stuttering transition.
- `ServiceLifecycle.tla` covers the process-wide recurring-task registry:
  atomic registration fencing, one active drain owner, concurrent shutdown
  followers that cannot return early, cooperative completion, deadline abort,
  and repeated shutdown after the active drain. `ServiceLifecycleProof.tla`
  proves the shutdown admission and return barriers directly.

### Orthogonal state regions

The implementation and proofs do not use one product-wide state enum. The
Thread aggregate commits orthogonal regions in one `ThreadCommit`: Run
disposition and optional exact resume ticket; ordered messages/state/audit;
`ActiveToolBatch`; and delegation relationships. Their coupling is expressed as
commit invariants (for example, Awaiting requires its exact ticket, executor
entry requires a committed Executing call), not by multiplying every region
into a second mega-state machine.

A tool call is likewise decomposed into durable call lifecycle, approval/input
availability, and replay policy. `ToolBatch.tla` checks their legal product and
whole-batch publication barrier; `RustCommitSystem.tla` plus the executable Rust
trace bridge checks the committed Thread projection. `RunIngress.tla` does not
repeat either region: it treats ThreadCommit disposition as an external fact and
models only dispatch ownership and settlement. This is assume-guarantee
composition, so external tool effects still require a stable operation id and
downstream idempotency; they are not claimed exactly once by these proofs.
- `ObservationReconcile.tla` covers the concurrent heartbeat and periodic
  observation-refresh paths sharing one async mutex. It gives every semantic
  evidence change a monotonic epoch (including A -> B -> A), preserves the
  epoch for unchanged heartbeats, and proves that failures cannot publish a
  fence while every skipped epoch was already published or superseded.
  `ObservationReconcileProof.tla` proves the complete inductive safety invariant.
- `ExecutableProjectionRefresh.tla` covers two concurrent Runtime-admitting
  requests refreshing the Agent projection and then the Environment projection.
  Each domain has one mutex owner; replay installs a clone before the infallible
  cursor advance, failures publish neither projection nor cursor, and a handler
  is admitted only after both independently captured high-water marks are
  covered. `ExecutableProjectionRefreshProof.tla` proves the unbounded inductive
  invariant, including the short install-before-cursor visibility window.
- `WebhookOutbox.tla`, `RegistrationIntent.tla`, `ErasureSaga.tla`, and
  `CredentialCreation.tla` cover atomic lifecycle/outbox commit, atomic
  Environment revision+intent commit with crash-window replay and
  revision-fenced idempotent projection, revision-CAS checkpointed erasure
  under two competing replicas with idempotent target replay, and durable
  credential-intent recovery.
- `MemoryCAS.tla`, `SkillVersionPin.tla`, `ToolResultProtocol.tla`,
  `WorkerDrain.tla`, and `WorkerCredentialLiveness.tla` cover memory
  generation/rename/conditional-delete safety,
  immutable Skill pin retention and validation, cross-protocol result
  correlation, the local-admission-before-remote-registry drain fence, and
  sequence-fenced credential observations that cannot roll back after rotation.
- `AuditCommit.tla` and `ConfigActivation.tla` cover transactional durable audit,
  replay fencing, and generation-fenced publication installation.
- `AcpBoundary.tla`, `CredentialEffectBoundary.tla`, and
  `SessionRealizationMutex.tla` cover handshake-before-prompt and conservative
  permission projection, exact-claim materialization/publication, and exclusive
  Session realization phase driving. The mutex model has an explicit finite
  completion bound for TLC; the one-driver invariant itself is independent of
  the chosen bound.
- `CredentialRotationWorkflow.tla`, `DeploymentExecutionWorkflow.tla`,
  `ResourceLifecycleWorkflow.tla`, and `SessionRunProtocol.tla` compose the
  corresponding already-modeled kernels at product boundaries. The Session
  protocol additionally checks that reservation precedes activity, Reserved
  never executes, recovery claims grant admission work only, and execution
  requires one Worker to own the exact Dispatch, WorkQueue, and realization
  authorities. Their normal configurations check safety; the dedicated
  reachability configurations witness the credential completion, deployment
  settlement, and resource reclamation goals without promoting environmental
  fairness into a theorem.
- `InferenceAccessPublication.tla` covers immutable access publication and
  dispatch-time pinning across route change, revocation, and fallback.
- `McpServer.tla` covers request/notification response cardinality,
  cancellation, progress finality, and sessionless stream lifecycle.
- `ManagementAuditIntent.tla`, `CredentialInventory.tla`,
  `ManagedCredentialCreation.tla`, `RolloutEventIdentity.tla`, and
  `ResourceBindingEffect.tla` cover audit-before-write admission for stores that
  cannot share the config transaction, ownership-fenced orphan detection,
  atomic Managed Source/child publication with original-epoch publication and
  abort-only expired-Writing takeover, exact event-id replay/collision
  decisions, and the durable
  external-effect journal used for resource bindings.
  Current-format Managed production commands construct an attempt-suffixed
  physical SecretRef. The Kani harness proves the bounded admission selector
  requires a declared owner and namespace match; it does not prove suffix
  uniqueness, UUID entropy, or SecretStore conditional writes. A takeover
  rejects stale durable transitions and can move an expired `Writing` fact only
  toward abort cleanup. The model admits a delayed old external write only as
  an unreachable orphan, never as durable readiness or publication. When
  attempt namespaces are distinct, that orphan is isolated from later attempts
  and reported by inventory inspection. Physical GC requires a backend-specific
  durable orphan claim before deletion. The
  bounded model proves publication ownership, not SecretStore conditional-write
  semantics, database clock accuracy, or an upper bound on external-call delay.
- `AgentInputRevision.tla` covers the resource-plane revision protocol for an
  Agent's input configuration: a changed configuration must be the exact next
  revision, an identical current revision is an idempotent replay, and every
  stale, skipped, zero, or conflicting revision is rejected without mutation.
  Authorization is deliberately outside this state machine and remains an edge
  admission concern.
- `OutcomeLifecycle.tla` covers the zero-based Worker/Grader loop, stable logical
  Run identity under replay, the evaluation budget, the exactly-once ungraded
  acknowledgment, interruption, infrastructure failure, and terminal absorption.
- `SessionResourceActivation.tla` covers durable prepare-before-IO activation,
  exact revision issuance, commit/rollback, terminal release, and the rule that
  a terminated Session can never reactivate a pending resource generation.
- `ResourceDispatch.tla` covers one-time configuration pinning, mutable content,
  worker capability admission, Workspace equality, explicit-empty revocation,
  and the live lifecycle deny evaluated at each activation attempt.
- `ResourceReclamation.tla` covers logical delete, durable claim recovery,
  physical-identity fencing, reference/lease exclusion, idempotent purge, and
  receipt generation fencing. A receipt proves that one purge occurred while
  fenced and unreferenced; it does not assert permanent absence of a shared
  content-addressed File blob that a later owner may recreate.
- `WorkerReplacement.tla` composes authored route resolution, dispatch-time
  candidate pinning, worker claims, credential materialization, execution and
  settlement with concurrent route change, rotation, revocation, worker crash,
  lease expiry and retry. It checks that durable state remains secret-free,
  bindings and authority never widen, unavailable credentials cannot execute,
  secret materialization is claim-fenced, and stale claims cannot publish output.
- `RemoteWorkerProtocol.tla` composes Worker incarnation and drain state,
  dispatch claim/renew/expiry/reclaim, claim-authorized recovery snapshots,
  execution admission, coordinator-owned optimistic commits, durable operation
  receipts, response loss/retry, Worker-local projection advancement, pending
  input/cancellation consumption, and settlement. It checks the P0 protocol
  invariants across the old-Worker/new-Worker overlap that the component models
  intentionally do not compose.

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
- Session activity receipt safety: 26/26.
- Session root creation and realization safety: 41/41.
- LiveInbox reorder atomicity: 3/3.
- Service lifecycle shutdown barriers: 4/4.
- Shared mount reference safety: 29/29.
- Observation reconciliation fencing: 69/69.
- Executable projection refresh safety: 120/120.

## TLC exhaustive finite checks

The checked configurations currently reach the following complete finite state
graphs with zero invariant violations and zero states left on the queue:

| Model | Generated | Distinct | Max depth |
| --- | ---: | ---: | ---: |
| RunIngress | 3,124 | 115 | 9 |
| ThreadState | 15,001 | 3,031 | 5 |
| SessionRunProtocol | 20,224,559 | 5,103,191 | 32 |
| WorkQueue | 26,521 | 4,206 | 15 |
| SessionActivity | 436 | 79 | 7 |
| SessionRoot | 58,273 | 8,073 | 13 |
| SessionEventProtocol | 257 | 104 | 13 |
| SessionStartProtocol | 71,212 | 20,379 | 30 |
| SessionStartProtocol reachability | 121 | 109 | 20 |
| Delegation | 5,835 | 1,097 | 14 |
| ToolBatch | 1,414 | 979 | 12 |
| RuntimeSystem | 110,923 | 12,896 | 13 |
| RuntimeImplementation | 1,323,147 | 619,008 | 24 |
| RustCommitSystem | 4,943 | 1,397 | 9 |
| RemoteTool | 421 | 200 | 10 |
| RemoteAttempt | 701 | 356 | 16 |
| AuthzKernel | 180 | 18 | 1 |
| SessionOwnership | 768 | 169 | 7 |
| SessionDeletion | 67 | 27 | 8 |
| CircuitBreaker | 1,573 | 478 | 10 |
| AggregateCAS | 1,669 | 417 | 11 |
| DeploymentCAS | 2,565,587 | 225,992 | 18 |
| LiveInbox | 3,025 | 81 | 7 |
| ServiceLifecycle | 226 | 131 | 9 |
| ObservationReconcile | 10,055 | 2,835 | 16 |
| ExecutableProjectionRefresh | 111,151 | 39,600 | 37 |
| CheckpointRecovery | 462 | 141 | 8 |
| WebhookOutbox | 10 | 6 | 4 |
| RegistrationIntent | 1,454 | 487 | 17 |
| ErasureSaga | 2,469 | 588 | 11 |
| CredentialCreation | 14 | 8 | 5 |
| MemoryCAS | 3,511 | 563 | 11 |
| ToolResultProtocol | 273,949 | 18,744 | 14 |
| WorkerDrain | 44 | 26 | 11 |
| WorkerCredentialLiveness | 753,391 | 17,784 | 16 |
| AuditCommit | 10 | 6 | 4 |
| ConfigActivation | 85 | 35 | 9 |
| InferenceAccessPublication | 466 | 234 | 9 |
| ResourceBindingEffect | 21 | 10 | 6 |
| AgentInputRevision | 297 | 65 | 9 |
| OutcomeLifecycle | 62 | 31 | 9 |
| SessionResourceActivation | 61 | 39 | 9 |
| ResourceDispatch | 66,535 | 11,952 | 15 |
| ResourceReclamation | 11,156 | 2,514 | 17 |
| ManagementAuditIntent | 15 | 8 | 5 |
| CredentialInventory | 7 | 4 | 3 |
| ManagedCredentialCreation | 5,702 | 1,250 | 11 |
| ManagedCredentialRollout | 234,903 | 28,492 | 20 |
| RolloutEventIdentity | 42 | 17 | 6 |
| ManagedVaultDeletion | 682,436 | 36,840 | 20 |
| SkillVersionPin | 747 | 184 | 8 |
| McpServer | 15 | 15 | 6 |
| WorkerReplacement | 452,881 | 98,160 | 16 |
| RemoteWorkerProtocol | 2,155,183 | 258,524 | 30 |
| ACP boundary | 5 | 5 | 4 |
| Credential effect boundary | 11 | 10 | 4 |
| Session realization mutex | 23 | 20 | 12 |
| Credential rotation workflow | 1,547 | 548 | 12 |
| Credential rotation reachability | 6 | 6 | 6 |
| Deployment execution workflow | 70 | 60 | 8 |
| Deployment execution reachability | 15 | 15 | 5 |
| Resource lifecycle workflow | 50 | 22 | 9 |
| Resource lifecycle reachability | 6 | 6 | 6 |

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

Bootstrap the repository-pinned Kani toolchain, install TLAPS and Java 11+,
obtain `tla2tools.jar`, then run:

```sh
scripts/ci/bootstrap_kani.sh
TLA2TOOLS_JAR=/path/to/tla2tools.jar \
TLAPM_BIN=/path/to/tlapm \
scripts/ci/check_formal.sh --require-tools
```

The bootstrap is pinned to a reviewed Kani source revision using Rust 1.97
nightly because the published Kani 0.67 bundle embeds Rust 1.93, while the
workspace's IAM dependencies require Rust 1.96. It retains Cargo's
`rust-version` enforcement instead of masking an incompatible compiler with
`--ignore-rust-version`; subsequent formal runs discover the cached source
build automatically. `AWAKEN_KANI_CACHE_DIR`, `AWAKEN_KANI_SOURCE_DIR`, and
`AWAKEN_KANI_BACKEND_DIR` allow CI to provide an equivalent prebuilt cache.

The script names every Kani harness and uses isolated TLC state directories so
fast sequential models cannot collide on TLC's timestamp-based default path. It
also regenerates all executable traces from the current Rust source before TLC
checks them; no checked-in hand-authored trace can become stale.

The repository-wide CI entry point runs the strict formal-verification gate.
`check-all.sh` passes `--require-tools`, so missing Kani,
TLAPS, Java, or `tla2tools.jar` fails instead of producing a false green.

`formal/coverage.json` is the versioned, claim-oriented obligation ledger. The
CI gate verifies that every evidence path exists and that at least 70% of
formalizable safety obligations have checked formal evidence. At this review
checkpoint the ledger is 363/363 formalizable obligations model-linked or
kernel-proved, plus 11 explicitly external obligations, for 100% formal
evidence coverage. The evidence dimensions are reported independently: 223
model-checked, 30 model-proved, 175 Kani-kernel-proved, and 6 linked to the
executable Runtime trace refinement bridge. These dimensions overlap and must
not be summed. No formalizable row remains executable-only.
Environmental properties are listed separately and never
silently omitted or mislabeled as model-linked merely to raise the percentage.

`model_linked` deliberately means that a checked formal model and concrete Rust
evidence are traceably associated; it is not a claim that every execution of
that Rust file refines the model. Direct implementation evidence is counted only
when a named Kani harness invokes the production kernel or the real Runtime
emits a trace checked by `RustCommitSystem!TraceIsRefinement`.

The current denominator of 363 formalizable obligations
is not derived from all source code: it is the number of manually enumerated
rows marked `formalizable` in that ledger. To prevent that
curated denominator from hiding an unenumerated module,
`scripts/ci/check_formal_surface.py` independently scans production Rust for
authorization decisions, state machines, synchronization, durable fences and
transactions, recovery/retry protocols, and plaintext credential boundaries.
The formal gate prints both denominators on every run. At this checkpoint the
source-oriented inventory finds 720 candidate modules: all 720 are classified,
387 have checked formal evidence associated with the production file, 242 have
a direct Kani/trace proof link, and 265 are linked to an explicit product
requirement boundary. This deliberately over-approximating inventory is not a
claim that every signal in every listed file
is itself a distinct proof obligation. A module may leave the uncovered set
only through a ledger link or a reviewed `formal/surface-exclusions.json`
boundary with a concrete reason. Both strict targets are now enforced: zero
uncovered source surfaces and zero executable-only formalizable obligations.
The source tree and strict CI name the same 233 unique Kani harnesses; repeated
ledger references are allowed only when one production proof supports more than
one precisely stated obligation.

The Web inventory is a separate denominator: 79/79 signal-detected TypeScript
or TSX candidate files are classified, with zero uncovered, across 26 product
requirements. All 79 are deliberately marked `product_boundary_only`; this is
an ownership and residual-risk inventory, not a claim that any browser module
has been formally proved.

`formal/proof-boundaries.json` and `formal/VERIFICATION_BOUNDARIES.md` keep the
remaining product limits honest. Every requirement-only surface, plus any
feature explicitly marked as retaining an external tail after its kernel proof, has one exact
external, semantic, unbounded-input, effect-adapter, or UI/human boundary plus
an architecture change, target proof method, and acceptance gate. The boundary
checker currently requires all 43 residual requirements and rejects missing or
invented rows. This documents how typestate permits, bounded protocol algebras,
event-sourced reducers, commit receipts, and durable inbox/outbox identities can
move additional adapter behavior under proof without claiming that networks,
LLM semantics, database engines, kernels, or humans were verified.

`formal/features.json` supplies the independent product denominator. Every row
in the canonical functional coverage matrix has exactly one machine-readable
feature, entrypoint list, criticality, and requirement classification. The
feature gate rejects orphaned formal obligations and external assumptions, so a
high claim-oriented ratio cannot hide a product area that was omitted from the
denominator. `formal/assumptions.json` records the environmental contracts that
repository-owned proofs consume, including their integration and runtime
evidence. An `evidenced` external assumption means its boundary has executable
or operational detection evidence; it does not promote third-party correctness
into a repository theorem. Run `scripts/ci/check_feature_coverage.py
--require-complete` to make
unverified product requirements or open external assumptions fatal; the normal
formal gate always validates the inventory and reports those remaining gaps.

## Loom concurrency exploration

The strict gate runs both the production `MemoryWorkerDirectory` and the
reference WorkQueue's production `LeaseBook` with Loom's instrumented mutex. It
exhaustively explores heartbeat-versus-drain,
stale-heartbeat-versus-incarnation-replacement, concurrent Work reclaim/release,
and authority observation during a replacement claim. A heartbeat cannot reopen
a draining worker, an old incarnation cannot mutate its replacement, and a Work
authority snapshot cannot combine an owner, epoch, or expiry from different
claims. SQLite/PostgreSQL queue mutations instead use database row transactions;
their interleavings remain in `WorkQueue.tla` plus backend conformance and
contention tests.

## Honest boundary

The executable bridge machine-checks the covered real async executions, but is
not a compiler theorem that every possible Rust scheduler, adapter, network, or
database execution refines the TLA+ model. `RemoteWorkerProtocol` checks bounded
progress only while the corresponding claim, snapshot, commit, settle, or
quiesce action remains enabled under its explicit weak-fairness assumptions.
The current proof does not establish unbounded liveness, eventual network
recovery, eventual Worker availability, eventual external input, or eventual
tool-result arrival. External tool side effects, remote
protocol implementations, database engines, provider-side key-revocation
propagation, container/kernel and Kubernetes isolation, performance/long-run
stability, external telemetry delivery and retention, and unbounded state spaces
remain outside the state-machine proof. They require idempotency contracts,
adapter integration tests, fault injection, real-runtime isolation tests,
k6/soak tests, and operational reconciliation; a larger finite TLC bound alone
cannot prove them.

`McpCredentialDelivery` proves the local post-adoption boundary: once a
revocation transition removes the accepting attachment generation, no later MCP
call can use that revision, and a replacement generation must rebuild the ACP
process before it is accepted. It does not prove that a provider revocation is
delivered promptly, or that an already-busy third-party ACP call is interrupted
and its process terminated. Immediate busy-call revocation propagation is not
implemented as a repository-guaranteed protocol; it remains an explicit
external operational boundary rather than an overclaimed formal theorem.

`ObservationReconcile` proves the in-process fence and mutex protocol assuming
the Worker directory accepts heartbeats in strictly increasing sequence order
and returns snapshots produced by those accepted transitions. It does not prove
eventual heartbeat/network delivery, scheduler fairness, or the correctness of
the external credential/ACP sources. Those remain explicit environment and
adapter obligations; a failed source refresh is modeled only as a safe retryable
failure that cannot advance the published fence.

`ExecutableProjectionRefresh` proves per-domain serialization, clone-install and
cursor ordering, fail-closed middleware admission, and Agent-before-Environment
refresh for concurrent requests. It assumes the append-only command stores
return a stable bounded range through the captured high-water and that the
database engine honors the query and transaction contracts. The two domain
high-water marks are captured independently, so the proof intentionally does
not claim a transactionally atomic cross-domain snapshot or eventual command
delivery.

The `host-executor/v1` capability pins the declared model identity and fails
closed on a replacement that does not install that identity. Proving that two
separately built worker images implement the same model identity with equivalent
code/configuration remains a deployment-supply-chain assumption; image digest,
SBOM/signature, and fleet-conformance checks must enforce it.

The transaction-hardening batch closes the three formerly narrower boundaries.
`WebhookOutbox` now models the session repository's atomic lifecycle+outbox
commit. `CredentialCreation` models the secret-free durable intent, atomic source
publication/intent retirement, and restart/periodic compensation. `AuditCommit` models a
durable pending audit followed by the config store's atomic business commit and
pending→committed transition; stable committed-call replay is a no-op. Tracing is
an observability projection rather than the durable audit authority.

`ManagedCredentialCreation` adds a bounded abstract Managed Agents creation
protocol.
One stable intent identity admits duplicate replay and rejects a conflicting
begin. A durable writer token plus monotonic epoch abstracts the Rust owner
fence; within the model an unexpired live lease cannot be claimed, while
cancellation/crash and deadline expiry enable one atomic recovery takeover.
Only the original live epoch-1 owner may put material or mark `Ready`. A
successful claim of expired `Writing` work moves `None`, `Partial`, and
`Complete` material alike only to durable `ReclaimingAbort`; it can never put,
mark ready, or publish. A `Ready` fact produced by the original owner remains
recoverable without changing its already-durable token, and only that epoch-1
fact may commit the Source/child pair. A late old external write after takeover
can create only an unreachable orphan flag; stale ready/commit transitions
cannot mutate durable material, phase, or either half of the pair. Crash/restart
can interleave before retryable external deletion, and only exact cleanup
completion reaches the clean `Aborted` state. An abandoned `Writing` intent
eventually aborts under the explicit expiry, restart, claim, abort, and cleanup
fairness assumptions. Source plus Vault child publish in one abstract commit.

Create enters `Writing` and produces no rollout. An update enters `Writing`
only when it writes new material; metadata-only update, archive, and delete
enter `Ready`. `ManagedCredentialRollout` begins after a non-create pair commit
and abstracts one configured consumer, extending that boundary across
update/archive/delete:
the Source/child pair and exact rollout event appear atomically, stale writer
CAS is rejected, and Archived may progress to Deleted. Attempt, adoption, and
ack commit are separate states, including failure, crash after adoption but
before ack, duplicate delivery, and reordered stale delivery that reselects
current durable truth. Acknowledgement requires an equal-or-newer adopted fence.
`RolloutEventIdentity` separately checks that an exact replay is harmless, a
different payload with the same event id cannot replace the durable row, and a
colliding acknowledgement cannot delete it. Eventual acknowledgement in the
rollout model is conditional on restart, the one abstract target becoming
available, and commit/delivery/adoption/ack fairness. The production adapter's
Session query, idle transition, repeated supervisor scheduling, concurrent
Session discovery, and fleet-wide fan-out are not established by that property.

`ManagedVaultDeletion` uses two bounded child slots whose `Absent` values also
cover empty and one-child Vaults. It admits deletion from an Active or Archived
root and tracks Active, Archived, and already-Deleted children, exact
Source/child successor fences, a prepared stale writer, root revision CAS and
stable delete identity, and separate rollout attempt, adoption, and ack sets.
Completion universally requires every real child to be Deleted and every
required exact rollout to be adopted and acknowledged. Crash clears only
process-local attempts; durable adoption, ack, child tombstones, and the root
request survive. Under explicit restart, stale-writer rejection,
reconciliation, adoption, acknowledgement, and final-CAS fairness assumptions a
requested root eventually reaches its absorbing tombstone.

These are bounded protocol models: creation checks one live owner and bounded
recovery-owner epochs after an abstract durable lease expiry, rollout checks
one consumer and revisions through three, and root deletion checks two child
slots and revisions through four. They do not prove
that the Rust adapters refine the abstract atomic commits. Storage engines,
SecretStore durability, lease-clock truth/renewal, Session-idle fairness,
provider revocation, multi-consumer delivery, and third-party effects remain
external; liveness holds only under the fairness assumptions named above.

These models start from well-formed durable facts. SQLite/PostgreSQL recovery
scans separately retain and isolate undecodable or unsupported-version rows so
one poison record cannot starve healthy work, but schema migration and repair of
the isolated record remain operational obligations rather than model claims.

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
advance the `first`/`matching last_heartbeat` condition. Stop consumes the exact
owner-and-epoch lease receipt, so a paused same-incarnation predecessor cannot
retire a higher-epoch successor. The in-memory, SQLite,
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
