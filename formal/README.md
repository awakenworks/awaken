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
  - delegation admission requires all lifecycle, lineage and budget guards;
    child invocation also requires its exact committed Open relationship.
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
  - distinct parent-Session affinity retains the child's own Run/Thread; claim
    advances one exact lease epoch and preserves the frozen dispatch; the physical
    slot admits exact authority. Memory, SQLite, and PostgreSQL share these reducers.
- `awaken-worker-contract`
  - Worker replacement and heartbeat share one non-resurrection boundary: a
    replaceable slot cannot accept a late heartbeat, and an applied heartbeat
    requires exact live incarnation plus a newer sequence.
- `awaken-runtime-contract`
  - committed terminal projection requires a present exact Run/Thread identity
    in `Ended`; missing and nonterminal truth stays ineligible, while identity
    disagreement fails closed;
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
  - revision advance is strict or explicitly exhausted; subject replacement requires the exact stored/caller revision and its non-wrapping successor through one admission rule shared by the application fixture and all three repository adapters.
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
- `awaken-authorization-contract` / `awaken-control` (Hosted Runtime roles)
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

The existing Rust-to-TLA runner consumes `session-observations-v4` from
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

- Session projection and opening-identity bounds are documented in
  [verification boundaries](VERIFICATION_BOUNDARIES.md#architecture-slices-implemented-at-this-checkpoint).
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
- `SessionEventProtocol.tla` composes activity and Dispatch with distinct wake/Run
  receipts. `SessionStartProtocol.tla` composes root, Work, realization, Event,
  Run, ThreadCommit observation and settlement; original TLC configs check faults
  and fault-free fair progress to Idle. `SessionStartRootProof.tla` reuses the
  canonical Root theorem for unbounded acceptance/queryability/projection safety,
  qualified by four mutants and four reachable witnesses. File-SQLite cold-reopen
  tests cover initial events, failed scan and replay; SQL/refinement stay external.
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
  no second tool transition model. Worker crash retains its physical slot;
  `ObserveProcessQuiescence` is an independent evidence boundary. `Spec` allows
  indefinite fencing, while `FairSpec` assumes eventual process-bound evidence
  and fair recovery. Neither implies termination of an opaque provider request.
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
  identity, root-kernel-derived ownership/read coherence (TLAPS), generation-fenced permits, optimistic concurrency,
  reverse-completing projection publication, Thread-owned Agent-message replay,
  transactionally bounded scheduled capacity, and revision-fenced
  scheduled-occurrence claims.
  `AgentMessageProtocol` keeps the parent tool receipt independent of the target
  Thread commit after durable admission: only a target Run boundary settles the
  Session activity. Its two reachability configurations prove both the ordinary
  receipt-before-target path and crash recovery where the child settles before
  the parent commits its missing receipt.
- `RemoteAttempt.tla` covers the A2A Prepared/send/Accepted protocol: exact
  task/endpoint binding, one send per operation, unknown-window quarantine,
  Accepted GET recovery, distinct Resume, pinned cancellation and retirement.
  Seven semantic mutants and seven witnesses check both fresh/resume crash
  windows and legal progress. Twelve production executor rows observe committed
  Prepared state and exact wire identity across commit rejection/lost replies,
  send loss and delivery; unknown recovery stays inert and new tickets execute.
  The ledger records finite bounds: these are model and executable evidence,
  not async Rust/ThreadCommit refinement, peer deduplication or delivery proof.
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
- `ServiceLifecycle.tla` and its TLAPS proof own the abstract recurring-task
  registry, one drain owner, waiting followers and empty-drain return barrier.
  Production uses the same `TaskRegistry` as its unique phase/result owner: only
  the first caller transfers tasks; `Draining` followers wait; the independent
  drain stores `Complete(result)` after watcher join/abort. A unit watch channel
  only wakes callers. Kani proves phase admission; native rows cross-check timeout,
  success, two followers and caller cancellation without claiming a non-yielding
  Future or opaque physical work has already stopped.
- `BackgroundTask.tla` covers the Thread-state-owned aggregate: one fenced
  owner/epoch/lease, waiting/cancellation, heartbeat/finish, expiry reclaim,
  remote continuation identity, refreshable poll intervals, NeverReplay,
  bounded attempts, terminal clearing and the process-local completion slot.
  The shared production heartbeat/finish mutation plan is checked directly
  across every phase, both stale-fence partitions, both commands and all scalar
  inputs; stale authority returns no plan before validation, so neither caller
  can mutate. Rust `String` allocation, Thread persistence, async scheduling and
  the read-to-effect interval remain outside the proof; Supervisor stays a driver.
- `SandboxControlPublication.tla` covers retained setup/serving cleanup across
  concurrent, cancelled and retried disposers. The shared production slot is
  checked directly without a mirror: its actual Mutex/Arc methods admit one live
  owner, fence stale and foreign releases, never reuse a still-live allocation,
  close absorbingly, retain cleanup across retries and finish it once. Native
  tables cover poison recovery and join-before-cleanup; seven mutants and four
  reachability witnesses guard the finite model. Conditional liveness still
  assumes eventual task exit and fair retry/completion. Mutex scheduling, Tokio
  progress, provider sockets/processes, Kubernetes Pods and kernel isolation
  remain executable or environmental evidence rather than direct proof.
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
  The contract-owned concrete pair has distinct Agent and Environment ports and
  is the only durable constructor of `ExecutableProjectionAdmission`. Its Kani
  harness exhausts both domain outcomes through the actual typed completion and
  `enter` path: only two ordered successes enter a handler. Strict async tables
  inject failures at three production consumers: Coordinator performs no source
  read, Session performs no durable mutation or dispatch, and Deployment performs
  no candidate mutation or launch. Tokio/DB behavior remains outside this kernel.
  The shared loader issues an opaque `ProjectionTarget` only after validating a
  batch through the durable high-water. Both production registrars pass it to
  `ProjectionCursor::install_then_reconcile`, so a free scalar cannot replace
  the captured authority coordinate. The same kernel covers monotonic
  installed-at-most-authority and adapter-only authority rollback: it lowers the
  private cursor before an older complete replay and restores the prior cursor
  on install error. Kani exhausts all prior/target `u64` pairs and both installer
  outcomes; the native table cross-checks lower, equal, and higher targets. The
  shared `ProjectionRefreshDomain` owns the actual Tokio permit plus cursor; its direct harness proves live-permit exclusion and release/reacquisition, while native Agent and Environment registrar tests capture a stable high-water,
  commit a higher command before the bounded range load, and require the first
  refresh to install only the captured prefix while the second refresh installs
  the later command. This binds the model's installed-at-most-authority invariant
  to both canonical replay/install paths without treating the in-memory source
  as proof of PostgreSQL isolation or cross-process behavior.
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
  Worker startup also checks the protocol declaration and positive registration TTL before any
  probe or Ready transition and sends incompatible peers through the same
  bounded withdrawal. The model checks one incarnation and up to three claims;
  semantic/liveness mutations and reachability witnesses qualify this path
  through the existing mutation runner. Protocol compatibility is an abstract
  input, not proof of HTTP decoding
  or mixed-version Control replica rollout; those require implementation tests.
  The `MemoryCASCopy.cfg` configuration reuses the same Repository transitions
  for two copy projections, frozen materialization heads, update/delete races,
  handle loss and restart before acknowledgement. A request can now remain
  in flight after caller cancellation and commit through the same Repository
  transitions; scan/root observations are captured at request admission rather
  than incorrectly re-read at commit. It checks that recovery never
  refreshes a stale base and identical retries never add a generation. Production
  SQLite tests in `awaken-sandbox-memoryd/tests/copy_lifecycle.rs` cover both
  ordered linearizations and barrier-released teardown. `tests/writeback_faults.rs`
  adds 24 actual SQLite teardown rows for create/update/delete, before/after
  commit, error/cancellation, and with/without a committed independent prefix,
  followed by same-handle retry and durable reopen. Eight additional rows pause
  an actual SQLite transaction before commit using an AFTER trigger, abort its
  caller and retry before/after its continuing blocking task commits, including
  retained conflict candidates. This is bounded model-linked
  safety evidence, not Rust refinement, process fencing, multi-file atomicity,
  or eventual recovery after loss of the physical tree.
  The model additionally gates copy mutation on a complete-scan observation and
  the original projection root, and separates commit from acknowledgement;
  production tests distinguish a missing root from explicit unlink and reject
  invalid trees before write-back. Completeness of a quiescent OS scan is an
  external assumption, not proved filesystem atomicity. The strict gate mutates
  recovery to refresh the base, admission to ignore an incomplete scan or a
  substituted root, cancellation to erase an in-flight request, and commit to
  follow an ID after its materialized path was renamed; all five
  must produce their exact invariant/action-property counterexamples, not a
  tool error. The model bounds each writer to one in-flight request and does
  not model SQLite internals, power loss, multi-file atomicity or OS process death.
  `MemoryCASRetention.cfg` extends this same model with retained proposals:
  acknowledgement/disposal require durability, and caller cancellation cannot
  erase the in-flight save. Three further mutations must fail those exact
  properties. It also checks eventual disposal with two writers, one caller
  crash each, eventual storage service and fair retry/recovery scheduling.
  That conditional bounded liveness is NOT proof under permanent cloud outage
  or arbitrary crashes. Twelve before/after-save error/cancellation rows,
  real SQLite/PG conformance, and terminal-only HTTP authorization are the
  corresponding implementation checks.
  `MemoryExtraction.tla` composes the durable intent stages with the stable
  auxiliary Run boundary and checks that in-flight crash recovery observes the
  same Run. The production controller now has one failure-action owner: Kani
  exhausts all failure classes, u32 attempt/maximum pairs, and closed statuses,
  proving that typed unknown execution selects the attention terminal and never
  Retry. `MemoryExtractionStatus` owns the exact recoverable set used by claim
  admission and both SQL adapters. Runtime observation truth, Future scheduling,
  repository CAS/lease and SQL semantics, durability, provider effects, operator
  resolution, and unconditional liveness remain outside the direct kernel proof.
  Copy location is checked inside the same repository transaction, before the
  same-content retry shortcut. Ordinary API/open-FUSE-fd updates still follow
  their ID. The shared Memory conformance suite covers SQLite/Postgres/volatile
  adapters; real HTTP and live/recovered Copy tests exercise both callers.
- `AuditCommit.tla` covers audit/replay fencing; `ConfigActivation.tla` covers ADR-0071 publication CAS,
  delayed registration, exact history and monotonic withdrawal. Direct Kani covers the production source fence and receipt gate; SQL/async/transport remain TCB.
- `AcpBoundary.tla`, `CredentialEffectBoundary.tla`, and
  `SessionRealizationMutex.tla` cover handshake-before-prompt and conservative
  permission projection, exact-claim materialization/publication, and exclusive
  Session realization phase driving. The mutex model ranges over multiple
  Sessions and proves both one driver per Session and a bounded number of
  cross-Session phase drivers; its fair deadline releases that abstract owner.
  This is not a mapping of request/completion permits onto a Session mutex.
  The same module checks seven actual adapter return/cancellation traces and
  three Host held-mutex frames on one eight-slot scheduler per sweep; twelve
  semantic trace mutants qualify the relation. Admission closes before waiting;
  cancellation retains the exact lease for the existing revocation retry.
  Arbitrary Tokio schedules, overlapping sweeps and unbounded fairness remain
  outside these finite observations and the original bounded phase-driver model.
  Credential effect and MCP delivery scope is maintained in the
  [verification boundaries](VERIFICATION_BOUNDARIES.md#credential-effect-and-mcp-delivery-evidence).
  `AcpBoundary` additionally checks two independent waiters, cancellation of an
  observer, TERM/KILL requests, lease cleanup timeout, actual process exit and
  ACK. Only observed exit permits ACK; eventual ACK assumes fair acknowledgement
  after exit, never eventual provider termination. Four strict-gate behavioral
  mutants restore timeout-as-drop, signal-as-ACK, crash-as-ACK and partial-target
  rollout adoption; only the named semantic counterexamples count as kills.
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
  Termination retains pending intent until the verified cleanup receipt;
  terminal assignment projects active -> desired, never active -> active when
  a replacement is pending. The strict mutation gate restores the active-only
  adapter bug and a premature receipt and requires their named counterexamples.
  Conditional cleanup liveness assumes fair recovery and an eventual verified
  physical receipt; it does not establish that any provider can produce one.
  The bounded model does not prove manifest byte equality, lease identity, or
  physical IO. The production crossover/rollback/failure decision tables are
  in `awaken-session-application/src/tests/resource_resume.rs`; terminal
  assignment coverage in `tests/realization.rs` checks fifteen SQLite close/reopen
  cases with independent history/manifest expectations. Provider absence and
  late-create fencing remain separate obligations. Signed dispatch
  transport and contract serialization are separately exercised tests, not a
  Rust-to-TLA refinement proof.
- `SessionUnmaterializedCleanup.tla` distinguishes current handle-free container
  uncertainty from the proposed physical-join contract. The ordinary config
  keeps unsupported cleanup pending. The Target config checks conditional
  convergence only under an exact physical join and fair adapter progress;
  that join is not yet implemented by handle-free container preparation.
  The mutation gate removes the join and finds an absent-query/late-create
  counterexample. Neither configuration proves Kubernetes absence, identity
  validation, API linearizability, or production adapter refinement.
- `ResourceDispatch.tla` covers one-time configuration pinning, mutable content,
  worker capability admission, Workspace equality, explicit-empty revocation,
  and the live lifecycle deny evaluated at each activation attempt.
- `ResourceReclamation.tla` separates File intent/tombstone/reference commits,
  crashes, guard/fence admission, shared references/leases and receipt identity.
  The [File lifecycle design](../docs/design/file-lifecycle.md) explains recovery.
  [coverage.json](coverage.json) owns proof bounds and assumptions; a receipt
  proves a safe purge operation, not permanent absence of shared content.
- `LocalProcessLifecycle.tla` models the sole `awaken-local-process` spawn
  boundary, two concurrent waiters, mutex acquisition/backoff, cancellation,
  signal requests, asynchronous OS exit, normal Drop and abrupt Worker death.
  It checks configured-before-spawn, no parked/cancelled waiter retaining the
  lock, stable exit observation, failed spawn without a handle and Drop's
  termination request. The crash action deliberately preserves surviving OS
  processes; a fourth mutation rejects introducing a false crash-cleanup
  assumption. A fifth mutation acknowledges a denied signal after leader exit;
  the signal-outcome invariant rejects it even when poll/wait already observed
  that exit. Unix signalling does not acquire the child mutex. The other three
  mutations remove spawn configuration, retain a lock across backoff, or retain
  it after cancellation. This finite Unix lifecycle model is
  linked to the real process tests, not a Rust refinement, unbounded liveness
  proof, process-tree quiescence certificate or Windows Job Object guarantee.
  The `LocalProcessScope.cfg` configuration reuses this model for one Linux
  process-scope owner: launch admission excludes close; accepted KILL is not observed
  emptiness; leader exit cannot substitute for descendant exit; owner crash
  still preserves OS processes in the model. The same configuration now models
  bootstrap join, ACK, atomic GO/admission release, execution and cancellation;
  execution requires membership and GO, and cancellation cannot invent GO.
  Final provider authorization is checked after ACK under the same synchronous
  guard retained through the nonblocking GO write. Acquisition and GO are
  separate model steps: a cross-thread revoker must contend on that guard.
  Backpressure releases it and requires fresh authorization. Revocation after
  GO does not undo a workload already authorized. Eight additional mutations
  must violate the exact final-authority, revoke/grant-exclusion, admission,
  subtree-empty, accepted-stop, premature-exec, premature-release and
  cancelled-GO invariants. Wall-clock lease expiry and a concrete Session
  authorization adapter are not proved by this model.
  `LocalProcessBoundary.cfg` composes that same admission/GO model with the
  systemd-free Linux helper, inherited control channel, fixed execution cutoff
  and namespace init states (absent/live/stopping/exited). Owner/helper death,
  disconnect and expiry request teardown; accepted termination is not observed
  init exit. Only init exit can authorize a tree-empty observation. The model
  preserves processes at the instant of owner crash and checks subsequent
  termination under explicit weak fairness of kernel progress. Seven exact
  mutations remove the init receipt guard, preserve a detached member, permit
  expired execution, or sever one of the four failure links. The deadline is
  finite-state and irreversible in that configuration.
  `LocalProcessRenewal.cfg` composes the same boundary and current-authority
  lock with cutoff values 1..3, clock 0..4, one queued frame and a stalled
  proper prefix. A complete live update preserves the exact absolute cutoff;
  stale frames cannot shorten it, expired frames cannot resurrect execution,
  and stalled input cannot stop clock progress. Its five exact mutants remove
  current authorization, overwrite with a stale cutoff, bypass expiry, add
  receive-time latency, or stop clock progress during a partial frame.
  These are bounded safety/liveness checks with explicit clock/kernel fairness,
  not a business lease model, Rust refinement, real elapsed-time bound, kernel
  implementation, hostile-host safety or production Session integration proof.
  `awaken-sandbox/tests/linux_scoped.rs` independently tests actual
  loader-time containment, binary stdio, private descriptor ownership, detached
  trees, owner/helper SIGKILL, cutoff, authorized/denied/stale cutoff updates,
  owner SIGSTOP after renewal and unaffected neighboring execution.
  The old delegated-systemd adapter/test is removed, not an alternative mode.
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
4. The lower-layer store conformance suite commits Run fact/ticket, message,
   state and event regions in one `ThreadCommit`, binds their recovery cursors
   to that exact commit, and verifies that a valid but stale post-terminal full
   shape publishes none of them. The same shared rules run against in-memory,
   filesystem, SQLite, and PostgreSQL adapters when the PostgreSQL test service
   is available; backend-specific tests retain crash and later-write rollback.

## Running locally and from CI

Bootstrap the repository-pinned Kani toolchain, install TLAPS, Z3 4.8.9 and Java 11+,
obtain `tla2tools.jar`, then run:

```sh
PATH="/path/to/z3/bin:$PATH" scripts/ci/bootstrap_kani.sh
TLA2TOOLS_JAR=/path/to/tla2tools.jar \
TLAPM_BIN=/path/to/tlapm \
PATH="/path/to/z3/bin:$PATH" \
scripts/ci/check_formal.sh --require-tools
```

Z3 is used for the full-width Memory timestamp caller proof; the other Kani selections retain their existing solvers. TLAPS bundles pinned Z3 under `lib/tlapm/backends/bin`; add it to `PATH` to reuse it.
The gate checks its version and binds it in the existing execution receipt.

The pinned Kani source uses Rust 1.97 nightly because the published bundle's Rust 1.93 cannot meet the workspace's Rust 1.96 floor; Cargo's `rust-version` remains enforced.
A SHA-256-pinned patch preserves full `u128` niche offsets; a high-offset `Option` regression qualifies bootstrap and formal runs. Receipts bind revision and patch digest; bootstrap rejects unrelated edits and preserves the old cache.
The same bootstrap owns a fresh CBMC build with the reviewed SMT type/model patch.
It requires CMake, Ninja, a C++ compiler, Flex, Bison and M4, and fetches the pinned CBMC source and its CaDiCaL dependency.
It qualifies fixed build options with upstream unit/Z3/native tests, two intentional counterexamples and the actual Kani fixture.
Matching GOTO tools reuse the existing Kani setup installation trust; no independent release-archive verification is claimed.
Immutable receipts bind the three actual tool paths/bytes, configuration, qualification contract/fixtures and Z3 identity.
`AWAKEN_KANI_CACHE_DIR` selects the shared cache; `AWAKEN_KANI_SOURCE_DIR` and `AWAKEN_KANI_BACKEND_DIR` select already qualified compiler and backend (`bin`) directories.
The gate freezes physical paths using bootstrap `--paths`, then calls its `--verify-source`; sealed artifacts retain the same runtime identity.
Tool qualification checks Boolean/scalar results and preserves typed unknown array elements in JSON traces; it does not provide a complete lambda-array model interpreter or concrete playback. It does not establish business properties whose proofs timed out or did not run.

The script names every Kani harness and uses isolated TLC state directories so
fast sequential models cannot collide on TLC's timestamp-based default path. It
also regenerates all executable traces from the current Rust source before TLC
checks them; no checked-in hand-authored trace can become stale.

The repository-wide CI entry point runs the strict formal-verification gate.
`check-all.sh` passes `--require-tools`, so missing Kani,
TLAPS, Z3, Java, or `tla2tools.jar` fails instead of producing a false green.

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

`formal/features.json` supplies the independent product denominator: each
machine-readable feature has entrypoints, criticality, requirements, and an
ordered assurance journey. The feature gate rejects orphaned obligations,
assumptions, features, and formal anchors, so the claim-oriented ledger cannot
hide an omitted product area. Local proofs remain independently checkable while
the journey graph exposes composition and environmental cuts; it does not claim
one theorem over third-party systems. `formal/assumptions.json` owns those
environmental contracts, their evidence, exact version/configuration
fingerprints, validity windows, and fail-closed behavior. `evidenced` means the
boundary has executable or operational detection, never that the dependency is
proved. `check_feature_coverage.py --require-complete` makes an open requirement
or assumption fatal and reports external evidence separately from proved
components; the latter count remains zero by definition.

`scripts/release/verification_manifest.py` is the single proof-carrying release
and TCB admission owner. Its schema binds source and archive digests, proof
inputs/toolchain/bounds/exclusions, every TCB contract, and the completed
`check_formal.sh` receipt. Executed manifests hash committed Git blobs, so
platform line endings reproduce while semantic edits, dirty source, changed
toolchains, missing proof execution, or an earlier schema reject. The receipt is
signed CI testimony rather than an independently checkable proof certificate.
`standard` packages carry exact gaps without claiming Verified; package
generation under `AWAKEN_RELEASE_PROFILE=awaken-verified-v1` additionally
requires the P0 direct-proof denominator to be empty.

The Python owner projects final release and per-dependency TCB facts into the
single `awaken-service-lifecycle::release_admission` kernel. Its pinned Rust
CLI fails closed on process, invocation, disposition, or fact rejection. Kani
exhausts six release Booleans, every scalar TCB Boolean, and the full `u64` age
relation. JSON projection, hashing, Git/receipt validation, compiler/subprocess
integrity and authentication remain boundaries, not Python/workflow refinement.

Management staging and final release both validate the requested profile and
completed proof before extraction or attestation. Requesting
`awaken-verified-v1` necessarily enables Verified admission, so omission of a
separate `--require-verified` flag cannot bypass fresh TCB evidence or the exact
runtime fingerprint. Standard release consumption remains unchanged. The
Management image carries the original manifest read-only, and the existing
payload check binds executable/manifest bytes and OCI source/revision/version
for fresh and reused images; carrying it does not authenticate a deployment.

Verified admission accepts the existing `--tcb-evidence evidence.json` and
`--runtime-fingerprint target.json` inputs. The former owns `observations`; the
latter independently owns `dependencies` with exact `id`, `version`, and
`configuration_digest`. Both must contain every non-semantic TCB contract once;
contract/version/configuration mismatch, missing or duplicate IDs, malformed
digests, invalid clocks, and expired observations fail closed. The repository
has no authority that produces or authenticates these files, so its release
workflow rejects the Verified identity until the deployment owner provides that
handoff. Observer authentication, target snapshot TOCTOU, runtime invalidation,
and Session recovery binding remain external work; passing this offline bounded
decision is not a live runtime Verified identity. Detailed model and adapter
bounds remain in `formal/VERIFICATION_BOUNDARIES.md`.

`scripts/ci/formal_profile.py` remains the shared requirement/P0 qualification
owner for feature coverage, formal coverage, and packaging. Structural
eligibility alone proves neither tool execution nor deployed TCB authenticity.

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

The [Kani catalog](#kani-production-kernel-proofs) owns pure decision proofs;
Loom owns lock interleavings. Database and Tokio remain integration obligations.

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

`ObservationReconcile` proves the in-process fence and mutex protocol assuming
the Worker directory accepts heartbeats in strictly increasing sequence order
and returns snapshots produced by those accepted transitions. It does not prove
eventual heartbeat/network delivery, scheduler fairness, or the correctness of
the external credential/ACP sources. Those remain explicit environment and
adapter obligations; a failed source refresh is modeled only as a safe retryable
failure that cannot advance the published fence.

`ExecutableProjectionRefresh` proves typed Agent-before-Environment admission, failed-refresh exclusion, per-domain permit topology, clone-install and cursor ordering. The direct capability kernel proves that both successes are required
to enter a handler; the actual shared Tokio mutex wrapper proves that one live
permit excludes another under Kani's sequential treatment of atomics. Native
two-task tables and TLAPS retain the independent concurrency evidence. Database
range and transaction truth, Tokio scheduling/cancellation/panic, and arbitrary
future consumer wiring remain outside these kernels. Independently captured
domain high-water marks do not establish an atomic cross-domain snapshot or
eventual command delivery.

The `host-executor/v1` capability pins the declared model identity and fails
closed on a replacement that does not install that identity. Proving that two
separately built worker images implement the same model identity with equivalent
code/configuration remains a deployment-supply-chain assumption; image digest,
SBOM/signature, and fleet-conformance checks must enforce it.

The transaction-hardening models cover three formerly narrower boundaries.
`WebhookOutbox` now models the session repository's atomic lifecycle+outbox
commit. SQLite and live PostgreSQL conformance inject an outbox-insert failure
after each create, replace, and delete root write and require the complete
transaction to leave no root, receipt, tombstone, or outbox change; this is
finite adapter evidence, not a universal database refinement.
`CredentialCreation` now projects the canonical material-attempt model
below: Source publication and external cleanup are not one atomic transaction.
It no longer maintains a parallel atomic compensation protocol. `AuditCommit` models a
durable pending audit followed by the config store's atomic business commit and
pending→committed transition across exact `(scope, tool, call_id)` identities;
delimiter-colliding tuples and conflicting summaries are distinct inputs, and
stable committed-call replay is a no-op. The config-domain admission kernel has
a bounded Kani proof over exact admission and shared Admin/HTTP entry/completion decisions, directly proving local absent/replay gates; backend/migration tests cover SQL.
Identity, database atomicity/durability, HTTP completion, crashes and async
execution remain model, executable or TCB boundaries; tracing is observational.

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
and abstracts two independently busy consumers, extending that boundary across
update/archive/delete:
the Source/child pair and exact rollout event appear atomically, stale writer
CAS is rejected, and Archived may progress to Deleted. Attempt, adoption, and
ack commit are separate states, including failure, crash after adoption but
before ack, duplicate delivery, and reordered stale delivery that reselects
current durable truth. Acknowledgement requires an equal-or-newer adopted fence.
`RolloutEventIdentity` separately checks that an exact replay is harmless, a
different payload with the same event id cannot replace the durable row, and a
colliding acknowledgement cannot delete it. Eventual acknowledgement in the
rollout model requires eventual stable process and target availability plus
commit/delivery/adoption/ack fairness. Intermittent availability alone can miss
every delivery window and does not establish progress. The production adapter's
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
two consumers and revisions through three, and root deletion checks two child
slots and revisions through four. They do not prove
that the Rust adapters refine the abstract atomic commits. Storage engines,
SecretStore durability, lease-clock truth/renewal, Session-idle fairness,
provider revocation, multi-consumer delivery, and third-party effects remain
external; liveness holds only under the fairness assumptions named above.

These models start from well-formed durable facts. SQLite/PostgreSQL recovery
scans separately retain and isolate undecodable or unsupported-version rows so
one poison record cannot starve healthy work, but schema migration and repair of
the isolated record remain operational obligations rather than model claims.

Run ownership, claimed commit, child delegation and cancellation evidence are
maintained in [verification boundaries](VERIFICATION_BOUNDARIES.md#run-ownership-and-delegation-evidence).
