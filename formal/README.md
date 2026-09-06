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
  - Work mutations require a present row, exact owner/epoch authority, and the
    matching command condition through one kernel used by all three backends;
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
  - Session realization reuses only one exact live owner/incarnation; every
    restart or authorized reassignment advances the epoch exactly once and
    exhaustion fails closed.
- `awaken-dispatch-contract`
  - the physical-attempt slot installs only for an empty current claim, clears
    only for its exact owner/epoch, never normalizes a torn persisted pair, and
    is the reducer called by Memory, SQLite, and PostgreSQL.
- `awaken-worker-contract`
  - Worker replacement and heartbeat share one non-resurrection boundary: a
    replaceable slot cannot accept a late heartbeat, and an applied heartbeat
    requires exact live incarnation plus a newer sequence.
- `awaken-runtime-contract`
  - an anchored compaction window grows with the transcript and therefore never
    moves past the prefix represented by summary plus bridge;
  - a legacy unanchored window fails open to the full transcript instead of
    silently dropping messages whose coverage cannot be reconstructed.
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
- `awaken-dispatch-contract`
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
- `awaken-protocol-managed` / `awaken-session-executor`
  - only non-interactive collection projection and terminal cleanup may use a
    frozen baseline when an Agent publication is unavailable; interactive
    recovery remains fail-closed. MCP
  credential realization preserves the authored target exactly.
  - a Session Hand can become ready only through a tracked `Starting` phase;
    cancellation of the waiting request does not transfer process ownership.
  - a container read-only tree is publishable only after its safe staging tree
    is complete and restricted; the external filesystem rename and rollback
    behavior remains an adapter assumption.
- `awaken-environment-realization-contract`
  - image-build claim/reclaim advances one exact non-wrapping attempt/lease
    epoch only when the current phase is eligible;
  - completion and failure require the exact current owner and epoch, and an
    invalid completion payload cannot publish Ready;
  - memory, SQLite, and PostgreSQL consume the same aggregate transition; live
    PostgreSQL concurrency and reconnect tests supply adapter evidence.
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

## Durable observation assurance

Wall time is evidence attached to the existing Session root or Thread commit,
not transition authority or event order. The Session UTC codec owns the
four-digit calendar display domain; raw durable clocks retain the signed SQL
millisecond domain. Out-of-calendar values saturate in presentation only.

The direct Kani harnesses cover the production UTC clamp, all generation
identity/expiry axes, the first processed-event observation/rejection boundary,
and event ordering independent of time. Collection/string bounds are recorded
in `coverage.json`; these are not proofs of database engines or arbitrary I/O.
`SessionRoot` and `SessionEventProtocol` retain their existing shared kernels and
add nonmonotonic observation values. `StoreHistory` adds commit-time receipts,
legacy unknown metadata, crash-discarded caches and exact recovered prefixes.
`SessionRealizationMutex` retains the same phase driver and adds the old+new
Stage receipt partition. Existing TLAPS root-kernel proofs do not automatically
prove these new TLC observer axes.

The existing Rust-to-TLA runner consumes `session-observations-v3` from
`runtime-refinement-manifest.json`. The manifest requires every named decision
rule, not merely each scenario file. Measured cardinality, predecessor sets,
lease expiries, clock partitions and crash boundaries bind the relevant labels
to actual conditions. Deleting, duplicating, renaming or relabeling these rules
must fail the same gate. Independent root/Event/store observations still call
their existing model relations; they are not presented as continuous executions.
An injective increasing map preserves and reflects time equality/order/max,
without changing raw observations or conflating unknown metadata.

The v3 selected wire contract is closed per phase: all required fields and
nested identity coordinates have strict types, and missing required lease facts
cannot become optional/default facts. Directive, call and publication/drain
report references retain Session, attachment/generation, runtime incarnation,
epoch and expiry. Root claims authorize the frozen plans; completed effects
match those plans and reports. The model independently requires authority at
lease-bearing boundaries. Same-generation renewal retains the production
monotonic-expiry relation instead of requiring obsolete exact root expiry.

For MCP recovery, the existing Managed HTTP scenario now owns all physical crash
evidence. A `test-support` observation port in Session contract connects the
canonical application root boundaries and shared realization driver to a
scenario-host file adapter. Production admission, Runtime synchronization and
HTTP MCP effects remain the existing implementations. The weaker file-effect
publication fixture has been removed. Newline-committed observations and atomic
fault markers let the parent SIGKILL the real server at a precise phase.

Thirteen reachable rules cross one/two attachments with Stage, first Publish,
first Drain, all effects before ACK, and ACK before response; retirement to zero
attachments covers the last three applicable points. Each restarts the actual
server, checks durable identity/ACK, idempotent HTTP replay and recovered tool
use. Independent fixtures isolate each rule from other Sessions' background I/O.
The comments beside the cases own the cause/effect design and constraints.

`SessionRealizationMutex.RealizationTraceIsRefinement` checks the continuous
observed sequence from Begin through one crash to recovered completion: execution
identity, revision/time monotonicity, legal adjacent phases, generation authority,
physical effects before ACK and terminal cardinality. The existing fair scheduler
model remains the exhaustive bounded interleaving owner. Four independent-family
trace corruptions remain checked. The continuous suite now derives missing,
null, wrong-type and unknown-field faults from every emitted phase/field/array
position in all thirteen rules. It separately changes every asserted lease and
complete MCP reference coordinate at every directive/call/report occurrence,
and deletes/duplicates/reorders phase events. Every counterfactual must reject;
normal traces, shorter evidence under a renewed root, and permutations of
independent same-phase calls must pass. The shared TLC oracle requires exact
successful evaluation of the conjunction of all refutations; parser/tool errors
never count as kills. Generated counts and case IDs are execution artifacts,
not a fixed hand-selected mutant denominator.
These are finite observed-path checks plus bounded model checking, not a universal
Rust refinement theorem, external exactly-once guarantee, or power-loss proof.

PR, release and strict formal qualification reuse
`check-reliability.sh --session-observations`. The PR/release matrices schedule
Linux and native ARM/Intel macOS execution before merge/packaging. A configured
matrix is not an execution receipt: current-source native macOS results must
still be obtained on those runners.

`formal_run_evidence.py` also owns per-invocation diagnostic artifact bundles
under `target/test-timings`. Native runs retain raw observations, rendered
models/configs and the suite log on success and failure. The strict formal run
retains original Thread/Session traces, rendered inputs, trace TLC logs and the
counterfactual batch inputs/logs/case index. Each completed bundle records source
identity, platform, exit status and content hashes; a source change cannot
produce Passed and a new run cannot reuse an old bundle directory. The existing
PR/release artifact upload retains these materials even after failure. These
bundles support replay and diagnosis; `formal-run.json` remains the sole strict
execution receipt used for release qualification.


Executable decision tables remain beside their tests: application-root crashes,
CAS and ambiguous acknowledgements; shared backend conformance; every byte cut
of a filesystem log envelope; warm/cold/peer projection; generation-set renewal
and duplicate publication acknowledgements; and real browser layout. The
reliability gate owns production mutation execution, and the formal gate owns
model mutations. Neither a classified surface nor a compiled native artifact is
an executed proof/platform result. macOS native execution remains a release-CI
obligation when the development host is Linux.

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
  Work owner, realization, committed Thread observation, and settlement. Claim
  authority and physical execution are separate variables: two Workers may
  reclaim/race the former, while an exact predecessor slot remains occupied
  until quiescence and the model checks at most one physical attempt per Thread.
  The production Runtime cache carries a closed coordination-or-execution
  purpose. Its Kani-proved admission reducer forbids a coordination envelope
  from satisfying an execution request even when the frozen publication is
  identical; the claimed Worker therefore rebuilds the executable context.
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
- `SessionThreadToolComposition.tla` composes the canonical
  `ThreadCommitProjection` relation across a root Thread and two child Runs sharing a
  child Thread. Each Run owns two calls; TLC checks all interleavings of the
  per-Thread physical-attempt fence, an external wait, crash recovery through
  `NeverReplay -> Indeterminate`, and replay-safe retry to completion. It adds
  no second tool transition model.
- `RuntimeSystem.tla` composes dispatch, tool batch, approval, delegation,
  messages, cancellation, and terminal commit under shared variables.
- `RuntimeImplementation.tla` splits every durable abstract transition into a
  prepare/apply protocol. A crash may discard a prepared operation; prepare and
  crash refine to stuttering, while every apply/invoke step refines to one
  `RuntimeSystem` action.
- `ThreadCommitProjection.tla` is the durable projection reconstructible from actual
  production `ThreadCommit`s: Run state/ticket, `ActiveToolBatch`, and
  `RunDelegations`. Its binary `NextState(s, t)` relation is also the checker for
  executable implementation traces.
- `RemoteTool.tla`, `SessionOwnership.tla`, `CircuitBreaker.tla`, `AggregateCAS.tla`,
  `ManagedProjectionPublish.tla`, `AgentMessageProtocol.tla`, and
  `DeploymentCAS.tla` cover durable operation
  identity, atomic ownership, generation-fenced permits, optimistic concurrency,
  reverse-completing projection publication, Thread-owned Agent-message replay,
  transactionally bounded scheduled capacity, and revision-fenced
  scheduled-occurrence claims.
  `AgentMessageProtocol` keeps the parent tool receipt independent of the target
  Thread commit after durable admission: only a target Run boundary settles the
  Session activity. Its two reachability configurations prove both the ordinary
  receipt-before-target path and crash recovery where the child settles before
  the parent commits its missing receipt.
- `RemoteAttempt.tla` covers the A2A root-attempt boundary: stable replay message
  identity across the external-send/local-commit crash window, durable task and
  endpoint pinning, reattachment without resend after the reference commit,
  input/resume continuity, fail-closed endpoint mismatch, cancellation of the
  pinned task, and terminal reference retirement. Remote peer deduplication is
  deliberately retained as an external idempotency assumption.
- `AuthzKernel.tla`, `LiveInbox.tla`, and `CheckpointRecovery.tla` cover total
  request classification, ordered editable process-local input (including
  atomic exact-permutation reorder, replace, remove, drain, close, and the
  pause-before-input-before-idle safe-boundary decision), and crash-safe
  streaming watermark/checkpoint behavior. `LiveInbox` deliberately models the
  process-local fold as best-effort rather than duplicating durable ingress. It
  also checks the physical-attempt composition: an inbox exists only for a
  local, current-owner executor that declares a safe live-input boundary;
  attempt close or ownership loss empties it, and a replacement attempt starts
  with a fresh empty inbox rather than inheriting Session state.
  `LiveInboxProof.tla`
  proves that reorder has no partially applied outcome and every invalid
  request is a stuttering transition.
- `ServiceLifecycle.tla` covers the process-wide recurring-task registry:
  atomic registration fencing, one active drain owner, concurrent shutdown
  followers that cannot return early, cooperative completion, deadline abort,
  and repeated shutdown after the active drain. `ServiceLifecycleProof.tla`
  proves the shutdown admission and return barriers directly.
- `BackgroundTask.tla` covers the Thread-state-owned background aggregate:
  one fenced owner/epoch/lease, waiting and cancellation, exact heartbeat and
  finish fences, expiry reclaim that preserves Waiting/Cancelling phase and
  remote continuation identity, optional refreshable poll intervals that are
  positive when present,
  NeverReplay indeterminacy, bounded attempts, terminal owner clearing, and the
  retry/loss/stale-fence behavior of the one process-local completion projection
  before its next ordinary Thread commit.
  The supervisor remains a driver, not a second durable task or result store.
- `SandboxControlPublication.tla` covers the provider-neutral control-channel
  publication slot shared by local Namespace and container sandboxes: one live
  owner identity, stale-release fencing, finite identity exhaustion without
  wraparound, and absorbing close. Provider sockets, Kubernetes Pods, process
  lifetime, and kernel isolation remain executable or environmental evidence.
- `EvalEvidence.tla` covers only deterministic evaluation evidence handling:
  frozen dataset version, exactly-one observation validity, complete case
  classification before publication, and post-publication immutability.
  LLM-judge semantics and benchmark representativeness remain external and are
  not promoted to formal claims.

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
whole-batch publication barrier; `ThreadCommitProjection.tla` plus executable
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
  The `MemoryCASCopy.cfg` configuration reuses the same Repository transitions
  for two copy projections, frozen materialization heads, update/delete races,
  handle loss and restart before acknowledgement. It checks that recovery never
  refreshes a stale base and identical retries never add a generation. Production
  SQLite tests in `awaken-sandbox-memoryd/tests/copy_lifecycle.rs` cover both
  ordered linearizations and barrier-released teardown. `tests/writeback_faults.rs`
  adds 24 actual SQLite teardown rows for create/update/delete, before/after
  commit, error/cancellation, and with/without a committed independent prefix,
  followed by same-handle retry and durable reopen. This is bounded model-linked
  safety evidence, not Rust refinement, process fencing, multi-file atomicity,
  or eventual recovery after loss of the physical tree.
  The model additionally gates copy mutation on a complete-scan observation and
  the original projection root, and separates commit from acknowledgement;
  production tests distinguish a missing root from explicit unlink and reject
  invalid trees before write-back. Completeness of a quiescent OS scan is an
  external assumption, not proved filesystem atomicity. The strict gate mutates
  recovery to refresh the base, deletion to ignore an incomplete scan, and
  deletion to accept a substituted root; all three
  must produce their exact invariant counterexamples, not a tool error.
- `AuditCommit.tla` and `ConfigActivation.tla` cover transactional durable audit,
  replay fencing, and generation-fenced publication installation.
- `AcpBoundary.tla`, `CredentialEffectBoundary.tla`, and
  `SessionRealizationMutex.tla` cover handshake-before-prompt and conservative
  permission projection, exact-claim materialization/publication, and exclusive
  Session realization phase driving. The mutex model ranges over multiple
  Sessions and proves both one driver per Session and a bounded number of
  cross-Session drivers. It also models the authority-derived Control deadline:
  under weak process fairness every acquired capacity slot either receives a
  response or times out and is eventually released. Its explicit finite
  completion bound is only for TLC; the concurrency invariants are independent
  of the chosen bound.
- `CredentialRotationWorkflow.tla`, `DeploymentExecutionWorkflow.tla`,
  `ResourceLifecycleWorkflow.tla`, and `SessionRunProtocol.tla` compose the
  corresponding already-modeled kernels at product boundaries. The Session
  protocol additionally checks that reservation precedes activity, Reserved
  never executes, recovery claims grant admission work only, and execution
  requires one Worker to own the exact Dispatch, WorkQueue, realization, and
  physical-attempt authorities. Its normal configuration contains two Workers
  so reclaim and predecessor-quiescence combinations are explored. Their normal
  configurations check safety; the dedicated
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
  Read/decide/commit are separate actions for two concurrent writers; an abort
  releases admission without writing. Disabling serialization reproduces the
  PostgreSQL missing-row-lock bug and must violate the exact-successor invariant
  in the strict mutation gate. The production scalar validator has a separate
  full-i64 Kani proof; database transactions and structural content equality are
  not universally implementation-refined. Authorization remains edge admission.
- `OutcomeLifecycle.tla` covers the zero-based Worker/Grader loop, stable logical
  Run identity under replay, the evaluation budget, the exactly-once ungraded
  acknowledgment, interruption, infrastructure failure, and terminal absorption.
- `SessionResourceActivation.tla` covers durable prepare-before-IO activation,
  exact revision issuance, commit/rollback, terminal release, and the rule that
  a terminated Session can never reactivate a pending resource generation.
  Stage now carries an active or pending revision across root changes:
  retained-active reconstruction cannot consume pending replacement, commit
  uses the prepared revision, and a rejected stale Stage has no root effect.
  The bounded model does not prove manifest byte equality, lease identity, or
  physical IO. The production crossover/rollback/failure decision tables are
  in `awaken-session-application/src/tests/resource_resume.rs`; signed dispatch
  transport and contract serialization are separately exercised tests, not a
  Rust-to-TLA refinement proof.
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

`ThreadCommitProjectionProof.tla` proves the production-commit projection's `Safety`
invariant inductive for arbitrary constants satisfying its assumptions. This
includes ticket/call coherence, the publication barrier, attempt bounds,
delegation ownership, and terminal sealing.

`WorkQueueProof.tla` separately proves the Managed queue's type safety,
single-active invariant, lease authority, positive active epoch, and terminal
lease clearing for every modeled transition.

`SessionNetworkPolicyProof.tla` proves canonicalization idempotence and the
non-widening, nonblank, commutative, idempotent and associative meet of arbitrary
abstract host sets. The Session contract owns the sole production normalization
path, with Unicode property tests and SQLite frozen-baseline readback. These
tests and the finite TLC blank-host mutation oracle do not establish universal
Rust String/Vec refinement or physical network enforcement; the obligation
retains abstract-theorem grade.

TLAPS results belong to the exact strict invocation, not a hand-maintained
README counter. The canonical runner names the proof modules; its output records
each module's discharged obligations. See the exact-SHA quality artifact and
`target/test-timings/formal-run.json` for execution identity and completion.
The receipt is not itself a theorem or a substitute for the tool reports.

## TLC exhaustive finite checks

TLC's generated/distinct state counts and maximum depth depend on the selected
source and configuration. Read those values from the same strict invocation's
tool output; this README does not duplicate a table of historical measurements.
Require successful completion, no invariant violations and an empty state queue.
These are bounded exhaustive checks, not unbounded liveness proofs; the bounds
are explicit in the corresponding `.cfg` files.

For the current declared obligation counts and proof grades, use the existing
ledger-generated report:

```sh
python3 scripts/ci/check_formal_coverage.py --json
```

That command inventories evidence; only a completed strict run establishes that
the current source passed its registered proofs. A `running` or source-mismatched
receipt is not success evidence.

## Executable Rust refinement bridge

`formal_refinement.rs` drives the real async `Runtime` through a coordinator
that wraps the ordinary in-memory commit implementation. It records only
successful `ThreadCommit`s and projects each committed state from the same
public state keys used during recovery. At executor entry the tests additionally
assert that the corresponding `Executing` call is already committed; delegated
dispatch also asserts that its stable child relationship is already `Open`.

The renderer converts these production traces to TLA+ values. TLC then evaluates
`ThreadCommitProjection!TraceIsRefinement`, requiring the formal initial state, `Safety`
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
- an external effect applied before its response returns, followed by aborting
  and joining the real async owner and reconstructing a new Runtime: Idempotent
  recovery retains the same call identity and one fixture receipt, while
  NeverReplay records Indeterminate without another invocation. The fixture's
  receipt contract does not prove arbitrary external services.

The production bridge remains layered around that direct trace check:

1. Kani universally checks the finite production Rust transition kernels.
2. TLAPS proves the commit projection's safety for every modeled transition,
   not only the captured traces.
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

PR, main, manual, and tag release qualification reuse the single `quality.yml`
workflow, whose named self-hosted runner contract supplies the required tools
and substrates. It invokes the complete `check-all.sh` once at the exact
workflow SHA. The orchestration checker rejects a partial, conditional, or
suppressed qualification step. Browser tests are owned by the frontend group; the E2E
group delegates to the existing instrumented runner, which invokes the single
deterministic suite graph and enforces changed-line coverage. Both coverage
reports read their served-process exclusions from `e2e_unreachable.toml`.
The reliability gate mutates the production ToolBatch transition guards in
addition to SSE parsing. Its aggregate tests own the phase/command/identity
decision table; the stateful `tool_batch_transitions` fuzz target applies
bounded command histories to that same aggregate and checks rejected-command
stuttering, terminal absorption, stable identities, and recovery decoding.
Neither mutation detection nor a time-bounded fuzz run establishes exhaustive
implementation coverage. Mutation reports are retained as release artifacts.
Recorded tool identities and per-check timings are release evidence, not proof
that a check which did not execute passed.

`formal/coverage.json` is the versioned, claim-oriented obligation ledger. The
CI gate verifies that every evidence path exists and that at least 70% of
formalizable safety obligations have checked formal evidence. The gate prints
the current obligation total, coverage ratio, external-assumption total, and
the independent model-checked, model-proved, Kani-kernel-proved, and executable
trace-refinement dimensions directly from that ledger. Those dimensions
overlap and must not be summed. The generated gate output, rather than a second
checkpoint count in this document, is the authority for the current totals.
Environmental properties are listed separately and never
silently omitted or mislabeled as model-linked merely to raise the percentage.

`model_linked` deliberately means that a checked formal model and concrete Rust
evidence are traceably associated; it is not a claim that every execution of
that Rust file refines the model. Direct implementation evidence is counted only
when a named Kani harness invokes the production kernel or the real Runtime
emits a trace checked by `ThreadCommitProjection!TraceIsRefinement`.

Every obligation now also carries the non-overloaded proof_grade,
claim_scope, bounds, tcb_assumptions, and production_entrypoints contract.
abstract_model_checked and abstract_theorem_proved make claims about the
abstract transition system only. production_kernel_proved names a Kani harness
over code called by production; implementation_refined requires a checked
universal production refinement, which the current checker does not yet admit.
The executable Runtime-to-model trace bridge alone does not qualify for that
grade. external_tcb is an explicit dependency rather than a
proof, and semantic_nonclaim records properties such as LLM output quality for
which Awaken makes no completeness theorem.

--require-complete remains a compatibility alias for
--require-evidence-complete; both mean that every formalizable obligation has
at least linked checked evidence. They do not mean that the product is eligible
for awaken-verified-v1. The stronger
scripts/ci/check_formal_coverage.py --require-verified-profile gate selects
every repository-controlled P0 safety requirement through the canonical
feature prefixes and requires each obligation to be production_kernel_proved
or implementation_refined. Its generated gap list is the authority; abstract
evidence is never promoted to make the gate green.

Target guardrails reuse that same qualification policy through exact
feature/requirement references in `formal/features.json`. The JSON report emits
the per-guardrail denominator, direct-proof count, exact gaps, and deployment
assumptions. `--require-guardrail G42` (or `G43`) is the fail-closed promotion
check; it does not move a row between the status tables in `docs/INVARIANTS.md`
and does not claim that external deployment observations were executed.

The formalizable-obligation denominator is not derived from all source code: it
is the number of manually enumerated rows marked `formalizable` in that ledger.
To prevent that
curated denominator from hiding an unenumerated module,
`scripts/ci/check_formal_surface.py` independently scans production Rust for
authorization decisions, state machines, synchronization, durable fences and
transactions, recovery/retry protocols, and plaintext credential boundaries.
The formal gate prints both current denominators and every evidence dimension
on each run. This deliberately over-approximating inventory is not a claim that
every signal in every listed file is itself a distinct proof obligation. A
module may leave the uncovered set only through a ledger link or a reviewed
`formal/surface-exclusions.json` boundary with a concrete reason. The strict
targets remain zero uncovered source surfaces and zero executable-only
formalizable obligations. The gate also compares the unique Kani harnesses in
the source tree with the ledger; repeated ledger references are allowed only
when one production proof supports more than one precisely stated obligation.

Registered production evidence must also be structurally reachable from a
Cargo library or binary module root. The inventory reuses the crate-boundary
lexer for ordinary, inline, path-attributed, and literal-include module edges;
removing an edge while retaining its evidence file fails the gate. This is a
conservative union of non-test feature/platform branches. Actual target and cfg
selection, generated code, macro expansion, and production call reachability
remain compiler and implementation-review responsibilities.

The Web inventory is a separate generated denominator. Classified TypeScript
and TSX candidates use `product_boundary_only`; the gate reports any uncovered
candidate instead of treating it as proved or silently omitting it. This is an
ownership and residual-risk inventory, not a claim that any browser module has
been formally proved.

`formal/proof-boundaries.json` and `formal/VERIFICATION_BOUNDARIES.md` keep the
remaining product limits honest. Every requirement-only surface, plus any
feature explicitly marked as retaining an external tail after its kernel proof, has one exact
external, semantic, unbounded-input, effect-adapter, or UI/human boundary plus
an architecture change, target proof method, and acceptance gate. The boundary
checker requires every residual requirement and rejects missing or
invented rows. This documents how typestate permits, bounded protocol algebras,
event-sourced reducers, commit receipts, and durable inbox/outbox identities can
move additional adapter behavior under proof without claiming that networks,
LLM semantics, database engines, kernels, or humans were verified.

`formal/features.json` supplies the independent product denominator. Every row
in the canonical functional coverage matrix has exactly one machine-readable
feature, entrypoint list, criticality, and requirement classification. The
feature gate rejects orphaned formal obligations and external assumptions, so a
high claim-oriented ratio cannot hide a product area that was omitted from the
denominator. The same file owns the ordered product-assurance journeys that
compose those features into managed execution, multi-agent/background
recovery, Control publication, extension/protocol, and storage/evidence paths.
The gate requires every feature, formal obligation, and external assumption to
be reachable from at least one journey and requires every declared formal
anchor to be consumed by a journey stage. This is the product-wide composition
contract: local proofs remain small and independently checkable, while the
journey graph makes their end-to-end coverage and every environmental cut
explicit. It is not a claim that a single monolithic state space or theorem
proves third-party systems. `formal/assumptions.json` records the environmental contracts that
repository-owned proofs consume, including their integration and runtime
evidence. An `evidenced` external assumption means its boundary has executable
or operational detection evidence; it does not promote third-party correctness
into a repository theorem. Run `scripts/ci/check_feature_coverage.py
--require-complete` to make
unverified product requirements or open external assumptions fatal; the normal
formal gate always validates the inventory and reports those remaining gaps.

Each TCB row additionally owns its required version/configuration fingerprint,
dependency contract, evidence-validity window, and fail-closed behavior.
Consequently “zero external evidence gaps” is reported separately from “zero
external components proved”; the latter remains zero by definition.

scripts/release/verification_manifest.py generates the single proof-carrying
release manifest from these ledgers. It binds the source commit, archive digest,
proof-input digest, proof toolchain contract, bounds, exclusions, and every TCB
contract. The existing release artifact attestation signs the generated file
with the other dist subjects. AWAKEN_RELEASE_PROFILE=awaken-verified-v1 turns
package generation into the strong gate; ordinary releases use standard and
carry the exact non-eligibility gap set without displaying a Verified identity.
Manifest schema 3 additionally binds `proof_execution`, the completion receipt
emitted by the one strict `check_formal.sh` invocation. It replaces any previous
receipt with `running` before checks and publishes `passed` only after every
tool stage succeeds. Packaging consumes the exact quality invocation artifact;
Verified generation rejects absent/incomplete execution, changed source or
toolchain contracts, and dirty candidate trees. Standard local packages may
explicitly carry null execution evidence. The receipt is CI testimony signed
through the existing release attestation, not an independently checkable proof
certificate; tool and runner trust remain explicit.
Executed manifests hash the committed Git blobs for source ledgers and proof
inputs, sharing the runner's source-identity mechanism. LF/CRLF checkouts of
the same clean commit therefore reproduce the same manifest; semantic edits
still invalidate the receipt. `source_digest_basis` distinguishes `git-blob`
release identity from `working-tree` local diagnostics. Artifact hashes always
cover exact physical archive bytes. Earlier manifest schemas are rejected.
The Management package staging and final release jobs both invoke the canonical
validator with `--expected-profile` and `--require-proof-execution` before
extracting or attesting archives. Standard releases require executed proof
evidence too; local diagnostic packages with null receipts cannot pass these
consumer gates. Final archive/sidecar sets must be nonempty and paired. Tests
execute these local workflow blocks using synthetic packages; they do not
claim a successful registry publication or a live deployment attestation.
The Management image includes that exact original manifest as the read-only
`/usr/share/awaken/verification.json`, not a rewritten image-specific ledger.
One shared payload check compares executable/manifest bytes and OCI source,
revision and version for both fresh and reused images. Reuse also pins the
signature workflow commit. Temporary inspection containers and their anonymous
volumes are reclaimed on success and check failure; Docker availability remains
an external assumption. The admission model includes the optional exact-image
payload prerequisite before TCB admission. Carrying this signed image content
does not itself authenticate a deployment or freeze a Session's proof identity.
The release-side validator offers `validate --require-verified --tcb-evidence
evidence.json --runtime-fingerprint target.json`. The caller must independently
authenticate both inputs. Evidence owns `observations`; the target snapshot owns
`dependencies`, each with exactly `id`, `version`, and `configuration_digest`.
Both must contain every non-semantic TCB contract exactly once, and observation
version/configuration must equal the target. Missing/unknown/duplicate dependency
IDs, unknown fields, malformed digests and invalid clocks fail closed; the
maximum-age boundary is inclusive. This snapshot is not a second assumption
ledger and cannot be derived from the evidence it is meant to check.
`VerifiedProfileAdmission` composes the bounded release prerequisite and
point-in-time TCB decision, including artifact/profile/proof/source mismatch,
per-dependency mismatch and expiry mutants; Python decision-table tests exercise
the production validator and its CLI. Neither is universal implementation
refinement. Authenticating the observer/actual deployment, preventing snapshot
TOCTOU, runtime invalidation and Session recovery integration remain required.
Passing the offline validator is not a live runtime Verified identity.

`scripts/ci/formal_profile.py` owns the shared requirement resolution and P0
qualification used by feature coverage, formal coverage, and release packaging.
Captured Runtime traces remain a separately reported evidence dimension; their
rows retain abstract-model/theorem grades. `implementation_refined` is reserved
for a future checked universal production-refinement integration and is rejected
until that integration exists. A ledger's structural eligibility does not attest
that proof tools ran or that deployed TCB fingerprints were authenticated.

Formal metadata mutation checks require an unmodified green fixture before each
mutation, the named semantic rejection (not merely nonzero exit), and a green
fixture after exact restoration. Missing tools, timeouts, parser failures, and
unrelated missing evidence are infrastructure failures, never killed mutants.

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

Kani additionally exhausts the production-consumed Worker replacement/
heartbeat table, Work owner/epoch/condition table, Session realization
assignment table, and Dispatch physical-attempt slot reducer. These proofs own
the decisions, not the lock implementations: Loom explores the instrumented
in-memory critical sections, while database and Tokio behavior remains covered
by transaction/concurrency tests and explicit assumptions.

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
process before it is accepted. The Runtime implementation additionally closes
new local transport admission, cancels and waits for every busy generation call,
quiesces the active Session Run, and reaps the generation-owned process before
acknowledging `Removed`; executable concurrency tests cover that implementation
boundary. The model still does not prove prompt provider-side revocation,
network delivery, or reversal of a third-party side effect already accepted
before cancellation. Those remain external operational boundaries rather than
overclaimed formal theorems.

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

The transaction-hardening models cover three formerly narrower boundaries.
`WebhookOutbox` now models the session repository's atomic lifecycle+outbox
commit. `CredentialCreation` now projects the canonical material-attempt model
below: Source publication and external cleanup are not one atomic transaction.
It no longer maintains a parallel atomic compensation protocol. `AuditCommit` models a
durable pending audit followed by the config store's atomic business commit and
pending→committed transition across exact `(scope, tool, call_id)` identities;
delimiter-colliding tuples and conflicting summaries are distinct inputs, and
stable committed-call replay is a no-op. The config-domain admission kernel has
a bounded Kani proof, while live backend conformance and history-migration tests
exercise SQL behavior. These are not a universal SQL/driver refinement. Tracing is
an observability projection rather than the durable audit authority.

`ManagedCredentialCreation` owns the bounded abstract material-attempt protocol
shared by Source-only and Managed creation.
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
can interleave between retryable physical deletion and WAL completion; only
observed material absence permits the clean `Aborted` state. A false delete
acknowledgement or failed readback cannot cause retirement. An abandoned `Writing` intent
eventually aborts under the explicit expiry, restart, claim, abort, and cleanup
fairness assumptions, including eventual physical removal and observation.
Source plus Vault child publish in one abstract commit. Source-only projects
away the child bit. Tracked-material claims exclude the late unreachable orphan
flag and do not prove arbitrary SecretStore behavior or cross-store atomicity.

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
