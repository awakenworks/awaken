# Formal verification boundaries and architecture roadmap

Formal verification can prove only a precisely stated model or a bounded piece
of implementation. It cannot, by itself, demonstrate every product behavior.
This repository therefore keeps three evidence levels separate:

1. **Direct implementation proof**: Kani, Loom, or checked Rust refinement calls
   the same decision/transition kernel consumed by production code.
2. **Model-linked evidence**: TLC/TLAPS proves a finite protocol model and the
   ledger names related production files. The link is an engineering claim, not
   an automatically proved Rust-to-TLA refinement.
3. **Product evidence**: tests, E2E scenarios, API snapshots, and architecture
   checks demonstrate executable behavior outside the formal model.

`coverage.json` must not relabel levels 2 or 3 as an implementation proof.
`proof-boundaries.json` is the machine-readable inventory of every product
requirement that still has requirement-only production surfaces.

The source denominator is deliberately split. `check_formal_surface.py` scans
Rust production modules, while `check_formal_web_surface.py` scans product-significant
TypeScript/TSX authority, route, storage, state, fetch, and mutation surfaces.
Every Web row is forced to `product_boundary_only`: browser tests and TypeScript
type checking are valuable executable evidence, but are not formal proofs.

## Architecture slices implemented at this checkpoint

- Management audit identity is the database tuple `(scope, tool, call_id)`, not
  delimiter concatenation. One V11 under the original config schema owner
  migrates both published histories from each record's coordinates; constraints
  bind index projections to the record and permit only pending/committed flags.
  Invalid history fails transactionally without a migration receipt. Both SQL
  adapters and the management test fake consume the config domain's single
  exact-intent admission; SQLite retries compare decoded records and concurrent
  begin uses the same unique-key arbitration contract as PostgreSQL. Kani checks
  presence/equality classes and all signed state values; the existing AuditCommit
  model now explores multiple scopes, delimiter-colliding tuples and conflicting
  summaries, with exact-intent and duplicate-write mutants. Shared backend tests
  cover tuple isolation and replay, with migration/rollback and cross-connection
  checks. SQL selection/locking, arbitrary heap equality, cross-store effects and
  universal implementation refinement remain outside this scalar proof: the
  existing audit obligations retain their abstract-model grades.
- BackgroundTask's detached context now supplies the existing Runtime ownership
  verifier at the common post-concurrency-admission effect boundary. It reads
  live local committed state and consumes the aggregate's exact request/policy/
  fence, current lease, phase and remote-target decision. Launch and continuation
  share one context constructor and receive the complete authoritative aggregate
  instead of independently assembled execution coordinates. Kani checks the
  admission decision for every phase pair and u64 clock/deadline; aggregate tests
  cover identity axes, and blocked-admission tests check start/poll/cancel after
  cancellation, reclaim, terminal commit, expiry or process replacement. Remote
  Worker projections remain unsupported authority, as already required at startup.
  This closes pre-effect stale-read admission, not the database-read/network-effect
  atomicity gap, in-flight revocation, third-party receipt fencing or universal
  driver refinement. The whole BackgroundTask obligations retain abstract grades.
- BackgroundTask's one process-local slot now carries owner/epoch in Active as
  well as Waiting/Completed. Exact-fence wait, completion, cancellation and
  retirement reject delayed callbacks; only a higher committed epoch replaces
  an existing slot without inventing cancellation intent. Retirement additionally
  requires a newer committed epoch, so an older Run snapshot cannot discard a
  newer completion. Three decision-table tests cover 81 combinations, and four
  Loom checks exercise the production mutex under selected two-thread schedules.
  The existing process-completion obligation remains abstract-model grade:
  these checks do not prove all Tokio internals, committed-observer freshness,
  same-epoch adapter callback semantics, database atomicity or journey refinement.
- BackgroundTask settlement preserves `Indeterminate` after cancel intent;
  neither a lost cancellation response nor a missing queryable handle proves
  remote termination. Reclaim keeps remote `Cancelling` and its exact handle
  for the post-commit Host observer; cancelled work without such a handle ends
  unknown even if its original execution policy allowed replay. The plugin's
  parallel post-reclaim `finish(Cancelled)` decision is removed: one aggregate
  transition produces one staged revision. BackgroundTask.tla now includes all
  completion variants and one canonical settlement projection; deleting its
  uncertainty guard must fail the named invariant. Aggregate and plugin tests
  cover unknown outcomes, stale fences, revision exhaustion and lost commits.
  The previous remote-continuation production grade is corrected to abstract:
  proving a reconnection predicate is not a proof that every wait/cancel/reclaim
  transition preserves the full handle. The production expired-attempt kernel
  is checked over all u64 epochs, u16 budgets and recovery/remote/cancel classes.
  It returns one closed Acquire/Indeterminate/AttemptsExhausted decision, consumed
  by aggregate materialization. Full-aggregate Kani experiments did not converge
  and are not accepted evidence: recursive heap/payload framing, revision/clock
  admission, async driver refinement and journey composition remain open.
- WorkQueue ordinary/token-bearing claims and Session acquisition all consume
  the existing `lease_epoch` transition, now backed by shared `StoredU64` rather
  than parallel SQL increments. Kani checks its full signed input domain and
  observe/advance decisions; SQL adapters validate before minting a token.
  A live SQLite regression caught MAX-epoch claims succeeding through integer
  promotion to REAL. SQLite/PostgreSQL tests now check rejection with unchanged
  queued state/no token hash and the exact MAX-1 -> MAX boundary. Schema checks
  already reject negative stored epochs. SQL locking and token entropy remain
  outside the numeric proof; expiry rules are unchanged.
- Config CAS uses one statement owner per SQL adapter for both ordinary and
  audited writes: expected zero inserts only, positive revisions update only,
  and the returned revision is the stored `RETURNING` value. Existing `StoredU64`
  checked arithmetic prevents signed-range exhaustion; SQLite's unconditional
  upsert also rejects exhaustion instead of promoting the integer to REAL.
  Shared SQLite/PostgreSQL decision, concurrent-writer, audit rollback and
  reopen tests cover the adapters. AggregateCAS permits arbitrary client
  revisions, including positive revisions against absence; removing the
  absent-row predicate or exhaustion guard must kill the corresponding model
  mutant. These SQL obligations remain abstract-model grade.
- Console initialization and route changes both reuse `workspaceFromPath`;
  no separate encoded-Workspace parser remains. Storage remains presentation
  only, and the server remains the authorization authority. Component tests and
  type checking are product evidence, not Rust/WASM or browser-scheduler proof.
- Agent input revisions use the existing config-resolver owner and one
  private scalar decision kernel. Kani checks all signed revision values and
  aggregate-equality classes: invalid attempts reject, positive exact replays
  do not write, successors cannot wrap, and corrupt stored revisions are not
  silently repaired. The public boolean validator is replaced by a closed
  Replay/Write admission with an opaque borrowed plan binding target, expected
  revision and exact proposal. SQL writes consume its insert-only/update-only
  predicate. One key codec rejects delimiter/NUL aliasing without changing valid
  historical keys; get/list reject key/content disagreement. The PostgreSQL
  adapter reuses the existing transaction
  advisory-lock implementation before reading, including absent-row creation;
  SQLite keeps its IMMEDIATE transaction. AgentInputRevision.tla now exposes
  two writers' read/decide/commit/abort and lost-response/exact-retry interleavings.
  Removing serialization from the abstract protocol must violate exact successor;
  production additionally fences its SQL statements. Controlled PostgreSQL
  overlap, SQLite/PostgreSQL write-failure triggers, zero-write replay, key
  rejection and reopen regressions cover adapters, not universal Rust/SQL refinement.
- Session authorization collapse directly proves all three IAM decision
  variants: only explicit Allow admits; Deny and RequireApproval both deny.
  Existing method property tests remain sampled tests, not a proof of arbitrary
  strings or the whole middleware/token/policy path.
- Worker credentials, ACP capabilities and their refresh generation now share
  one atomic observation snapshot. Initial and periodic heartbeats freeze that
  snapshot once before projecting wire fields; independent getters and the
  wrapping generation counter are removed. WorkerCredentialLiveness.tla checks
  producer-batch coherence alongside the existing heartbeat ordering protocol.
  Concurrent-reader and generation-exhaustion tests exercise the actual cache;
  std RwLock and Tokio integration remain executable/TCB evidence, not Kani or
  universal implementation refinement.
- Circuit admission and completion now consume one owner-local Rust reducer.
  Both Closed requests and HalfOpen probes carry phase/epoch fences. Late
  completions stutter, zero probe capacity rejects even the first probe, and
  exhausted generations never wrap. Kani checks the production reducer over
  the complete u32/u64 domains; CircuitBreaker.tla also explores ordinary calls,
  delayed results and probe budgets 0/1/2. The real-mutex tests retain concurrent
  permits across a barrier. Clock accuracy, scheduler fairness and provider
  behavior remain adapter/environment boundaries, not implementation proofs.
- The canonical product denominator now carries five end-to-end assurance
  journeys. Strict feature coverage requires every product feature to appear in
  at least one journey and every journey to cite formal anchors actually
  consumed by its stages. This connects the obligation families into complete
  managed-execution, multi-agent/background recovery, Control publication,
  extension/protocol, and storage/evidence paths without pretending that one
  monolithic model proves external systems. A mutation that disconnects a
  feature from every journey must fail the gate.
- Tool-call concurrency now has one production scheduler beside the canonical
  `ToolConcurrency` algebra. Runtime consumes its stable contiguous wave plan;
  Kani proves the production one-step extension guard and positive bound while
  `ToolSchedule.tla` exhausts every four-call claim assignment and bound to
  check exact stable partition, serial singleton behavior, pairwise
  compatibility, and the maximal-prefix/no-bypass composition rules.
- Product process Role, service-startup Role, migration ownership, and local
  Managed mounting no longer maintain separate role tables. CLI re-exports the
  lifecycle-owned `ProcessRole`, and Kani proves its exact projection including
  the Worker's absence of service-startup authority.
- Commit backends retain their existing ports and shared conformance suite. The
  new `StoreHistory` model composes dense commit order, terminal fencing,
  operation receipts, exact retry/rejection stuttering, and one CAS winner per
  Thread version. Memory, filesystem, SQLite, and PostgreSQL are linked to this
  one abstract history; database/driver durability and a mechanically checked
  Rust-to-model trace remain explicit adapter boundaries.
  Dispatch crash recovery additionally records one normalized predecessor/
  successor history through Memory, SQLite, and live PostgreSQL, including the
  database-owned lease clock and physical-attempt fence. The PostgreSQL release
  substrate finishes with a physical base-backup/WAL point-in-time restore and
  verifies the exact immutable object referenced by restored truth. These are
  executable adapter and recovery proofs; they do not turn PostgreSQL, S3, the
  host filesystem, or their cryptography into directly verified code.

- Workspace path projection now has one provider-independent logical root.
  `WorkspacePathProjection.tla` proves that only path-fidelitous providers may
  satisfy Repository admission and that read/edit/bash/Git observations cannot
  contain the provider host path. The real Linux Namespace/Hand conformance test
  exercises those four consumers against `/workspace`; mount namespace, kernel,
  Git, and filesystem correctness remain environmental evidence.
- The Session application has one internal acceptance point that returns only
  after the complete root and idempotency receipt are durable; the existing
  lifecycle supervisor then resumes realization from that root without a Job
  aggregate. `SessionStartProtocol.tla` proves that internal acceptance cannot
  precede the complete root. The standard Managed `/v1/sessions` adapter waits
  for creation and returns the official synchronous 200 shape; no public async
  status projection is claimed at this checkpoint. Database durability,
  scheduling fairness, and HTTP delivery remain outside the theorem.
- The committed-event Runtime projection has one closed pending-tool reducer.
  `SessionRuntimeProjection.tla` checks replacement, accepted-but-unprocessed
  reply classification, per-id processing, terminal clearing, and error
  evidence. Within the configured finite prefix domain (`0..3`), it also checks
  that a successful refresh advances the paired result and source checkpoint to
  the observed committed prefix, while a failed refresh stutters. Browser tests
  cover the production TypeScript
  projection and the list status mapping fails closed for unknown states. The
  separate `ManagedProjectionPublish.tla` model checks two reverse-completing
  refresh writers over independent Session, root/child Thread, lifecycle, and
  budget source coordinates, including a source advance between the first and
  second read. It proves mixed reads cannot build a publishable candidate,
  result/checkpoint pairing, local cache
  CAS, source non-invention, visible non-regression, failure/rejection
  stuttering, and that equal source coordinates plus equal rendered content
  change neither cache nor broadcast. Equal legacy coordinates with changed
  rendered content still publish rather than hiding an unversioned source.
  The Rust decision-table and concurrent-reader tests exercise the same publication
  branches. Together, the models prove that a retained reply no longer needs
  user action while remaining recoverable and that a stale refresher cannot
  roll the visible projection backwards. They do not prove database snapshot
  isolation, cold reconstruction of pre-anchor legacy history, browser delivery,
  or lifecycle-supervisor fairness.
- Internal Agent messaging has no message store or Dispatch-owned waiting
  state. `AgentMessageProtocol.tla` composes the canonical
  `SessionActivityKernel` and `RunIngressKernel`, adding only committed source
  request, admission certainty, target commit, and source receipt. It proves
  that unavailable admission and identity conflict cannot settle activity
  before target truth and stale claims remain fenced by the ingress kernel.
  After durable admission, source receipt and target commit are independent:
  either may win the crash/recovery race, while only the committed target
  boundary may settle Session activity. Internal delivery never creates pending
  input.
  Production refinement additionally validates the command against the source
  Run's committed `ActiveToolBatch`; backend conformance tests own atomic
  admission. The model does not prove model intent, Worker availability,
  scheduler fairness, or delivery to an external side effect.
- Tool permission defaults and exact overrides share the neutral
  `ToolExecutionPolicy`. `ToolPermissionPolicy.tla` exhaustively checks Agent
  default-allow, MCP default-ask, exact allow/ask/disabled configuration, and
  disabled fail-closed behavior. The existing Kani selector proves that an
  unregistered, mismatched, disabled, or widened execution request is rejected;
  provider documentation compatibility and the correctness of external tool
  effects remain evidence boundaries.
- Runtime output harvest publishes ordinary Files through the single
  Resources-owned `ArtifactPublication`/receipt path. Kani proves every receipt
  identity axis is mandatory. The generic harvester deliberately does not claim
  that an arbitrary output set is a complete patch handoff: the typed manifest,
  patch digest/base binding, external Git apply, tests, scans, and human approval
  are workflow/E2E evidence owned by the exporting command, not a hidden Bundle
  aggregate, checksum sidecar, or Resources completion state. SHA-256 strength,
  object-store durability, and download delivery remain effect boundaries.
- `SessionCleanupKernel.tla` remains the one four-phase cleanup algebra used by
  deletion. Repository publication reuses the same terminal cleanup operation;
  there is no parallel PatchBundle publication lifecycle or formal state model.

- Dream status writes now consume one closed, exhaustively checked lifecycle
  table. Terminal states are absorbing and completion/failure require Running;
  generated-content quality remains an explicitly semantic boundary.
- Resource Catalog publication consumes an exact non-wrapping version kernel;
  lifecycle timestamps and Kubernetes claim-deletion fences have exact checked
  projections. Filesystem, object-store, CSI, and Kubernetes durability remain
  environmental assumptions.
- Durable deployment admission and sandbox-support projection consume bounded
  evidence selectors. Actual PostgreSQL/filesystem persistence, NetworkPolicy,
  registry, builder, container, and kernel enforcement remain external.
- Quiescence, checkpoint, source-disposal, restore, and Session cleanup
  completions require every identity, generation, fence, and settlement axis.
  Session cleanup phases are exact and monotonic; physical deletion additionally
  requires hidden disposition, terminal execution, and verified completion in
  both store adapters. `SessionDeletion.tla` checks crash/retry, Work retirement,
  stable effect identity, single fact emission, and tombstone ordering. Database
  engine atomicity and the truth of remote cleanup/retirement effects remain
  adapter claims.
- Worker replacement and heartbeat now consume one non-resurrection kernel:
  an expired/quiesced/dead slot is replaceable and cannot simultaneously accept
  a heartbeat, while only an exact live incarnation with a newer sequence may
  mutate it. The real registry adapters share one transition and the in-memory
  adapter is explored by Loom. Database locking/durability, TLS, DNS, WebPKI,
  the remote process, and the orchestrator are not proved.
- Environment image-build claim, expiry reclaim, completion, and failure now
  consume one aggregate-owned decision kernel. Kani proves exact non-wrapping
  epoch/deadline advancement and exact owner/epoch mutation admission; memory,
  SQLite, and PostgreSQL reuse those methods, while the durable stores share one
  version-CAS loop. A live PostgreSQL race/restart test checks one winner at
  epochs 1 and 2 and rejects the predecessor. PostgreSQL locking/durability,
  builder/container effects, registries, clocks, and image availability remain
  adapter or environmental evidence rather than direct proofs.
- Settled Runtime steps and scoped tool-catalog membership now consume closed
  projection kernels: Running cannot cross the settled boundary and only the
  reserved scope can observe admin tools.
- HTTP idempotency keys now pass an exact byte/length admission kernel, keeping
  their original wire identity. Header libraries, proxies, TCP, and TLS remain
  outside the proof.
- Memory, SQLite, and PostgreSQL consume one Dispatch transition reducer for
  reservation activation/rejection/recovery, claim/reclaim, settlement,
  relinquishment, cancellation, retry exhaustion/requeue, and supersession.
  Initial and repair reservation commands carry a relative TTL; each adapter
  converts it with its store-owned clock, so a caller clock cannot pre-expire or
  extend admission authority. The shared conformance table checks this mapping;
  real database-clock accuracy remains environmental evidence.
  Their physical-attempt owner/epoch columns now consume the same proved slot
  reducer for begin and finish: only an empty current-claim slot installs, only
  the exact claim clears, a predecessor blocks/fences, and a torn nullable pair
  fails closed. Row locks/transactions and the Runtime's Tokio per-Thread mutex
  remain trusted implementation/environment boundaries rather than being
  mislabeled as Kani proofs.
  Runtime context reuse now consumes one closed purpose-aware reducer as well:
  a reservation/observation coordination envelope may never satisfy an
  execution request, regardless of matching publication identity. Kani proves
  the finite decision table; the asynchronous rebuild, Environment realization,
  and persistence effects remain integration evidence.
  SQL still owns row locking, compare-and-set predicates, pending-input writes,
  timestamps, and transaction durability; those adapter properties are checked
  by the shared backend conformance suite rather than proved by Kani.
- AG-UI and AI-SDK terminal emission consume one fail-closed absorbing kernel;
  their wire-category mappings are exhaustive. Unbounded tool reconciliation,
  network delivery, client interpretation, and rendering remain outside proof.
- A published Memory plugin remains satisfiable when recall is disabled, but
  the proved selector contributes no recall hook or state authority. Memory
  store durability and extraction effects remain external adapter boundaries.
- Session realization failures consume a closed seven-variant disposition
  table and one shared Worker-effect kernel. Initial/replacement ownership also
  consumes one proved assignment kernel: a live foreign owner cannot be shared,
  a restart/reassignment advances exactly one epoch, and exhaustion fails
  closed. The aggregate applies that decision through its existing root CAS;
  the Host retains one Tokio mutex per Session. Database commits, Tokio
  scheduling/fairness, transport, cross-process cleanup, and remote execution
  remain adapter/external claims.
- GenAI transcript replay consumes closed dialect, part-category, row-admission,
  opaque signed-thinking, and reasoning-fold kernels. Anthropic retains the
  ordered text/signature pair while other dialects use normalized reasoning;
  reasoning-only rows are omitted and streaming/non-streaming folds share one
  exact-once table. SDK capture/serialization, signature authenticity,
  unbounded payloads, network and provider/LLM behavior are not proved.

## What remains outside a direct proof

- LLM meaning, quality, usefulness, and truthfulness have no complete decidable
  specification. Structural schemas and finite rubrics can be proved; open
  natural-language correctness cannot.
- TLS/WebPKI, DNS, cryptographic strength, provider revocation, third-party
  peers, database engines, filesystems, kernels, containers, and orchestrators
  are environmental assumptions unless their implementations are brought into
  the verified computing base.
- Eventual delivery, real-clock behavior, scheduler fairness, performance,
  long-run stability, telemetry retention, browser rendering, accessibility,
  and human approval authenticity need explicit assumptions and executable
  evidence rather than blanket proof claims.
- Arbitrary JSON, strings, manifests, plugins, and user-authored workflows are
  unbounded. Kani proofs use finite equivalence classes or bounded collections;
  those bounds must appear in the obligation name or documentation.
- A model-linked TLA+ invariant does not automatically prove the asynchronous
  database/network adapter. A checked trace is stronger, but still covers the
  represented traces rather than every possible implementation execution.

## Architecture changes that expand the provable surface

### Canonical protocol algebra

Define one closed, versioned algebra for lifecycle, failure, permission, and
stream events. Generate ACP/A2A/MCP/HTTP/UI mappings and JSON schemas from it.
Kani then proves every generated mapping total, exact, and non-strengthening;
schema mutation and transport traces cover serialization and I/O.

### Compiled bounded manifests

Compile plugin, skill, tool, resource, and workflow manifests into bounded
closed representations before activation. Unknown identifiers and excessive
depth/size fail closed. Pure selectors and reducers become exhaustively
provable while parsing and storage remain narrow adapters.

### Generated authority and startup tables

Generate role grants, route guards, tool membership, and `StartupRoleWiring`
from closed decision tables. Production builders use typestate such as
`RuntimeBuilder<MissingAuthorization>` so a production runtime cannot exist
without its required authorization gate. Unrestricted construction is isolated
behind an explicit test-only API.

### Shared browser route-scope kernel

Move workspace scope precedence and route parsing out of TypeScript into a
small shared Rust contract compiled to WASM. The browser supplies route and
server context to this pure selector; local/session storage is never an
authority input. Kani can then prove exact precedence and fail-closed handling,
while browser tests retain responsibility for URI APIs, navigation, rendering,
and deployment integration.

### Decide -> CommitPlan -> CommitReceipt

Pure code decides an effect and emits a stable plan identity. An adapter performs
the effect and returns an authenticated receipt. Only an exact, live, fenced
receipt can mutate durable truth. TLA+ checks crash/retry behavior; Kani proves
plan/receipt admission; production traces check selected implementation refinements.

The dispatch/Thread cross-aggregate boundary now applies this rule to physical
claim release as well: CommitEpochBoundary is a production-consumed typestate,
Kani exhausts its release decision, and CommitEpochGuard.tla checks that an
early return is only a stutter. The PostgreSQL/SQLite lock implementation stays
an explicit database TCB; no model claim is relabeled as a driver proof.

Session Environment recovery similarly consumes one closed equality reducer
over effect identity, physical binding, and immutable generation. Kani proves
the complete decision table, while SessionEnvironmentProjection.tla proves
that exact and stale same-owner projection are identity stutters and cannot
rewrite a Resident or Retiring fence.

Closed physical observation after realization revocation consumes the captured
Retiring owner through the same exact confirmation transition; it does not
replace RealizationRevocation with RecoveryDiscard. The projection model also
checks exact/closed/policy admission and protected Terminal/Checkpoint rejection.
Owner tests retain stale-wrapper ABA, policy rejection and exact source identity
cases. This extension is abstract model evidence, not a production refinement
claim or a promise of recovery when external observation remains indeterminate.

### Proof-carrying release and deployment admission

The shared qualification implementation in `scripts/ci/formal_profile.py` owns
both checker and package selection. Scenario traces retain their model/theorem
grade and cannot claim universal implementation refinement. Formal mutation
oracles require a green complete fixture, the specific semantic rejection, and
a restored green fixture; infrastructure failure is not mutation evidence.
Kani linkage uses exact proof definitions with the existing Rust syntax lexer
and exact literal package/harness selections from the canonical runner. A shell
comment, function-name prefix, wrong Cargo package or duplicate selection earns
no credit. Cargo manifests own package identity. This is a static binding audit,
not a proof of arbitrary shell control flow, macro expansion or proof execution;
the actual strict invocation and its reports remain mandatory.

The existing strict formal runner emits one invocation receipt into quality's
existing artifact directory. Its start and completion bind the same source tree
and proof toolchain contract. Release manifest schema 2 consumes that receipt
and rejects incomplete, dirty or mismatched runs. Authenticity relies on the
trusted CI runner and the existing signed release provenance; the receipt does
not replace proof reports or establish unbounded Rust refinement.

The generated release manifest extends the existing signed release provenance;
it is not a second obligation or assumption ledger. A standard artifact records
all remaining direct-proof gaps. A Verified artifact can be generated only
after the P0 production-proof gate is empty, and deployment must additionally
present one fresh, exact fingerprint for every TCB contract. Artifact, ledger,
profile, proof-input, or TCB mismatch removes the Verified identity rather than
weakening the safety claim.

### Durable inbox/outbox and event-sourced reducers

Use durable inbox/outbox records for network and side-effect boundaries. Keep
session, run, memory, deployment, and outcome state as deterministic reducers
over immutable events. This makes safety independent of process crashes and
turns database adapters into conformance targets instead of hidden authorities.

### Linear permits and verified typestates

Represent credential materialization, mounts, tool effects, approval, and remote
transport as unforgeable, single-use permits bound to workspace, owner, epoch,
revision, hash, and expiry. Examples include `MaterializationPermit`,
`ContentAddressedSnapshot<Verified>`, `VerifiedTlsChannel`, and
`VerifiedPeerPrincipal`. Adapters issue them only after external validation;
finite kernels prove they cannot be widened or replayed.

### Bounded collections or induction

Where product bounds are real, encode and enforce them at admission. Where data
must remain unbounded, prove one transition preserves an invariant and use
mathematical induction/TLAPS rather than claiming a small Kani vector proves an
unbounded collection.

## Follow-up implementation sequence

1. Canonical protocol algebra and generated adapter tables.
2. Rust/WASM browser route-scope selector plus checked browser traces.
3. Compiled bounded plugin/tool/workflow manifests.
4. Generated role/route/startup authority and production typestate.
5. Complete `CommitPlan`/`CommitReceipt` coverage for remaining effects.
6. Durable inbox/outbox and stable operation identities.
7. Linear credential/resource/approval/transport permits.
8. Event-sourced reducers and backend refinement histories.

Each follow-up is complete only when production consumes the proved kernel, the
harness is named by strict CI, the product requirement links the obligation,
mutation tests can kill a weakened invariant where applicable, and remaining
external assumptions stay explicit.
