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

- ACP crossing writes extend the existing AcpBoundary model with a bounded
  duplex schedule, FIFO read-ahead, cancellation and explicit overflow. Kani
  proves the production buffer admission arithmetic over usize inputs; it does
  not prove Tokio readiness, UTF-8 parsing, Kubernetes transport or CLI progress.
  The actual wire tests cover concurrent large writes, cancellation within a
  UTF-8 frame, EOF, malformed input and flooding; the duplex mutant must fail.
  Cleanup drains trailing bytes without turning EOF into exit evidence; a missing
  Kubernetes process status remains an error. These async/adapter transitions
  are covered by executable tests, not by the buffer-arithmetic proof.
- Cowork cancellation and credential rollout extend the existing Dispatch,
  Runtime context, ACP supervisor and Vault outbox owners. Lease-request timeout
  retains the owned cleanup observer; Native tool abandonment and ACP reap failure
  veto physical ACK without rewriting the business outcome. Tests distinguish
  cancellation admission, executor return, physical termination and settlement.
  Native opaque-tool cancellation without a terminal receipt can still block
  same-Thread resumption indefinitely; no recovery API or receipt is invented.
  Pure Native inference returns data, and its discarded response cannot invoke
  a governed tool. Without execution-termination evidence, abandoned inference remains
  Indeterminate and keeps its physical fence. Neither this test nor the model
  proves that provider computation/billing stopped.
  The same owned drive retries an observed-return ACK after transient write
  failure, response loss or request timeout, without rerunning execution; a
  permanent rejection remains an error. Tests inject all four outcomes into
  the existing Dispatch store fixture. Progress requires transport recovery
  while the observer remains alive; process death before ACK is not repaired by
  this local retry or proved away by scalar fencing.
  See ADR-0024 D5. Docker, Podman and Kubernetes share a real process oracle for
  Pending-wait cancellation, concurrent wait/poll/signal, TERM and KILL; these
  tests do not establish termination of an opaque remote provider operation.
  AcpBoundary and SessionThreadToolComposition explore bounded observer/crash
  schedules, and ManagedCredentialRollout explores independent busy/failing
  targets with exact all-target ACK. Its progress property assumes eventual
  stable process/target availability, not merely intermittent availability.
  Four semantic mutants reject timeout-as-drop, signal-as-receipt,
  crash-as-quiescence and first-target-as-convergence. Scalar Kani fences remain
  direct kernel proofs; whole async lifecycle obligations retain abstract-model
  grades. Test comments own cause/effect decision rules, without another case
  ledger or a claim of universal Rust-to-model refinement.
- Credential material reclamation has one phase/membership kernel and one
  SecretStore executor for both envelopes, replacing four repeated cleanup
  implementations. Kani checks the closed phase and set-membership decision;
  typed tests exercise reference sets, duplicated slots and live predecessor
  preservation. Actual recovery now retains WAL after a no-op delete success,
  unavailable readback or a missing result for another ref; only exact absence
  allows subsequent completion. Source creation reuses the canonical Managed
  attempt model, whose physical removal and WAL completion are separate crash
  boundaries, with an absence-guard mutant. Neither readback nor this bounded
  model proves cross-store atomicity or prevents arbitrary later legacy writes;
  late unreachable orphans remain excluded from the tracked-material invariant.
- Credential abort/retirement decisions now have one owner for both existing
  Source and Managed pair envelopes. All three repository implementations call
  it under their existing lock/transaction; their six parallel phase tables
  are removed. The in-memory Source abort replay previously skipped the current
  unpublished-state check, unlike SQL. Shared backend regression now rejects
  that stale replay and retains a newer same-key intent on late completion.
  Kani proves the closed phase/exact-authority decision, while exhaustive typed
  projection tests cover creation, in-place update, distinct replacement and
  independent Source/child mismatches. The PostgreSQL conformance fixture now
  fails rather than silently skips when explicitly configured but unavailable.
  Heap equality, database isolation, external SecretStore IO and universal
  recovery-driver refinement remain outside the scalar proof; the encompassing
  cleanup obligation retains its abstract-model grade.
- Management effect retirement now requires a domain-owned readback receipt
  carrying the exact Workspace and complete effect. The Control reconciler
  obtains it only after reading the existing resource repository; missing,
  different and failed observations leave the journal intact. SQL adapters
  compare the current typed payload and delete under one transaction fence,
  returning whether a row was actually retired. A late old receipt cannot
  delete a successor at the same logical key. Enqueue and index decisions have
  one config-domain owner; SQLite no longer compares serialized JSON text.
  Shared backend tables and an actual paused-readback reconciler test exercise
  the ABA schedule, and ResourceBindingEffect now models two workers and two
  successive revisions with a guard-removal mutant. Kani checks the positive
  signed revision/exact-equality confirmation kernel. Scope selection, heap
  equality, actual storage durability, SQL fencing and whole-driver refinement
  remain tested/assumed boundaries, not consequences of that scalar proof.
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
- `WorkerAuthorityEvidence.tla` checks independent registry/evidence lanes,
  conservative request-start freshness, absorbing expiry and conditional
  eventual closure when either environment proofs or registry renewals stop.
  Probe latency is explicitly retained in the evidence age; three mutation
  tests reject deletion of either expiry lane or request latency. Its window is four ticks and
  it assumes fair time advancement and atomic local fencing. Actual Tokio
  timers, provider policy truth, startup I/O and physical cancellation remain
  executable-test/TCB boundaries; no Rust-to-model refinement is claimed.
  `WorkerDrain.tla` additionally checks eventual acknowledgement under weak
  fairness of claim completion, local admission closure, registry publication
  and acknowledgement; permanent I/O failure is excluded. The same model tracks
  up to three active attempts through natural return, grace expiry, cancellation,
  bounded cleanup and exit. It checks that surviving work is never discarded
  before cancellation/cleanup and that no return is fabricated; eventual exit
  assumes fair timer/cleanup progress, not eventual opaque-provider return.
  Four mutants reject reopening admission, disabling drain acknowledgement,
  skipping cancellation and fabricating quiescence; an ACK witness guards
  against vacuity. Cleanup completes before the model's existing withdrawal
  owner returns the process, retaining the startup/withdrawal invariants. Actual Tokio HTTP polling during AllInOne shutdown is verified by
  a real dual-listener receipt test, not inferred from this abstract model.
  These models do not establish recovery of an abandoned external physical
  attempt or cold reconstruction of host-bind participants.
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
  evidence. Its aggregate/event composition keeps current errors visible under
  active aggregate authority without admitting messages; accepted-but-unprocessed
  replies block follow-up until their committed receipt arrives. Two additional
  mutants cover hidden errors and presentation accidentally releasing admission. Within the configured finite prefix domain (`0..3`), it also checks
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
  stable effect identity, single fact emission, and tombstone ordering. The
  `SessionDeletionUnconfirmed.cfg` configuration removes the eventual-return
  assumption and checks permanent-unknown safety; it asserts no automatic
  tombstone or external completion in that environment. Database
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

## Credential effect and MCP delivery evidence

Catalog Vault insert/ensure now share the actual workspace predicate across
InMemory, SQLite and PostgreSQL. Its Kani proof exhausts u8 identities and row
presence; shared backend rules V1-V3 cover all 12 operation/owner/presence cases,
including rejection without mutation and ensure replay preserving durable truth.
The child workspace harness calls `admit_managed_credential_insert` itself,
covering fixed a/b owners, v/x parent IDs, parent presence/archive/deletion,
all child lifecycle variants and full-width revision with unwind 4. Environment
auth and empty siblings are fixed; a unit success witness rules out a vacuous
all-reject fixture. The obsolete proof-only child predicate was removed.
These are bounded admission-kernel proofs, not arbitrary String/URL proofs or
universal refinement of SQL concurrency, sibling uniqueness and child-ID CAS.

Managed begin/commit and cleanup use the same typed `is_unpublished_at`
comparison under each adapter's existing transaction. Its creation-presence
Kani harness covers all four present/absent pairs; shared adapter rule M6
checks independent source and child ID collisions. A unit witness checks that
the fixed Create fixture passes production command validation. The rollout Kani harness
calls the actual event constructor with distinct fixed one-character IDs,
all operations and full-width signed/unsigned revisions (unwind 4). Neither
proves command-validation composition or SQL/outbox atomicity. The former
proof-only boolean predicates were removed; both complete transaction
obligations retain abstract-model grades until production composition closes.

The Source and Managed creation envelopes share `CredentialMaterialMutationFence`.
Production Kani harnesses check its actual String-bearing Ready/publication/abort
methods, preservation of all non-phase fields, and recovery dispatch at the
exact expiry boundary. Epochs and clocks use the full `u64` domain; identities
use independent empty/a/b strings with unwind 4. The concrete takeover test
checks successor abort-only behavior, invalid deadlines and epoch exhaustion.
The same production claim transition takes a lazy token factory behind its
unchanged UUID wrapper. Kani checks exact eligibility/error precedence,
nonwrapping epochs and field preservation with empty or two valid 32-byte UUID
tokens (unwind 36). Epoch one is reserved for the original writer: legacy zero
and modern one both advance to two, then every positive recovery epoch advances
by one. All accepted successors are abort-only even if tokens repeat. A concrete
valid legacy-fence regression reproduced the former zero-to-one token-collision
path into Ready. The shared SQL table persists wire rows with missing owner
fields and missing/retained attempt IDs across Writing, Ready and ReclaimingAbort,
then checks Source/Managed takeover, repeated takeover, stale-owner rejection,
abort and completion on SQLite/PostgreSQL. Root update witnesses also vary
explicit/omitted JSON defaults and SQLite whitespace while preserving the decoded
Source/child snapshots. They reproduce a second encoding comparison that rejected
otherwise admitted legacy mutations. Full decoded-row admission under the existing
FOR UPDATE / BEGIN IMMEDIATE lock is now the sole snapshot fence; keyed ownership,
insert conflicts and affected-row checks remain. These witnesses cover publication,
WAL completion and rollout acknowledgement, not general SQL serializability.
The existing InMemory legacy recovery and serde tests exercise the same fence.
`ManagedCredentialCreation` now actually includes legacy Writing initial states
with absent/partial/complete material; new commands still use the existing Begin.
The single lease-expiry action also covers recovery owners. Three semantic
mutants check physical cleanup, legacy epoch reuse and legacy over-denial; three
legacy witnesses require claim, repeated claim and abort to be reachable; a
fourth witness preserves fresh-command publication.
FairSpec checks conditional abort progress over all initial partitions. This
removes the former comment-only legacy coverage; it is not Rust refinement.
Six adapter phase rechecks remain redundant after claim success plus complete
locked snapshot equality. Shared backend rules retain independent token, epoch,
deadline, payload and durable-phase drift checks.
These checks do not prove UUID generation, arbitrary-length strings, the
enclosing repository CAS, external material IO or the complete recovery protocol;
the existing takeover obligation retains its abstract-model grade.

`CredentialEffectBoundary` expands exactness into Workspace, Session, source,
revision, holder, trust domain, target, usage, envelope, payload, generation,
and claim epoch. Its full configuration checks all 4,096 initial binary
match/mismatch combinations; revision, generation and epoch may then advance
independently. Expiry, withdrawal, crash and restart may interleave between
preparation, materialization and receipt publication. Effect observations
retain their original authority so later revocation cannot falsify history.
The shared mutation runner removes each necessary guard and independently
witnesses useful success, successor-epoch success, expired/revoked rejection,
stale-material rejection and retained pre-revocation materialization. These
abstract equalities do not prove resolver/crypto/transport implementation or
instantaneous remote revocation. No new credential selector/store is modeled.

`McpCredentialDelivery` proves the local post-adoption boundary: once a
revocation transition removes the accepting attachment generation, no later MCP
call can use that revision, and a replacement generation must rebuild the ACP
process before it is accepted. Calls record their requested generation/source
revision and the process, accepting route, revocation set, and delivery selection
at effect entry. The safety oracle checks that concrete observation instead of
a constant safe flag; later revocation cannot retroactively invalidate a legal
call. The strict mutation runner checks wrong generation/source, crash bypass,
revocation/activation, and delivery-policy regressions, and requires successful
calls in both modeled modes, replacement, and retained pre-revocation history.
Attachment replacement retains the credential revision and Session lease;
credential adoption advances generation/revision while retaining the lease;
recovery advances the lease while retaining desired generation/revision. The
model retains earlier Stage receipts across all three transitions and records
both the received and requested tuple at activation. Three stale-activation
mutants and three independent-coordinate success witnesses prevent accidental
counter aliasing. These are bounded model witnesses, not evidence that a
deployment implements either delivery mode. The Runtime implementation
additionally closes
new local transport admission, cancels and waits for every busy generation call,
quiesces the active Session Run, and reaps the generation-owned process before
acknowledging `Removed`; executable concurrency tests cover that implementation
boundary. The model still does not prove prompt provider-side revocation,
network delivery, or reversal of a third-party side effect already accepted
before cancellation. Those remain external operational boundaries rather than
overclaimed formal theorems.

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
and proof toolchain contract. Release manifest schema 3 consumes that receipt
and rejects incomplete, dirty or mismatched runs. Authenticity relies on the
trusted CI runner and the existing signed release provenance; the receipt does
not replace proof reports or establish unbounded Rust refinement.
Its source-input digests reuse the runner's committed Git-byte authority after
clean-source validation, preventing platform newline conversion from changing
the claimed proof inputs. Archive digests remain hashes of actual bytes. Both
Management staging and final release attestation consume the exact requested
profile and completed receipt through the same validator; no sidecar can qualify
merely by being downloaded. The bounded admission model includes these release
prerequisites, and tests execute the actual local consumer shell blocks before
their extraction/publication boundaries. CI authenticity and immutable checkout
ownership remain assumptions; this is not runtime admission or image execution
evidence, and it does not close deployment check/use races.
The Management image now carries the original validated archive manifest as a
read-only file. New/reused image paths share one exact binary/manifest/OCI-label
check instead of independently deciding payload equivalence, and the reused
signature must name the source workflow commit. The signed image digest binds
the included file; no second proof manifest or qualification policy is created.
Both branches now converge after digest resolution: authenticate reused images,
pull the immutable subject, compare its payload, run E2E on that subject, then
sign/promote new images. A good local staging-tag check is never transferred to
the registry digest. The model's optional image-path projection distinguishes
wrong payload, a good check of another subject, and exact-subject evidence before
TCB admission. Actual shell tests cover wrong/missing payload, stale copy output,
inspection cleanup, changed push payload, malformed digest, pull/signature/E2E
failure and check-before-sign ordering. These are bounded model/adapter checks, not
signature verification proofs or a live runtime Verified identity.

The generated release manifest extends the existing signed release provenance;
it is not a second obligation or assumption ledger. A standard artifact records
all remaining direct-proof gaps. A Verified artifact can be generated only
after the P0 production-proof gate is empty, and deployment must additionally
present one fresh, exact fingerprint for every TCB contract. Artifact, ledger,
profile, proof-input, or TCB mismatch removes the Verified identity rather than
weakening the safety claim.

`verification_manifest.py` is the one release/TCB admission validator. Its
`--runtime-fingerprint` input is a deployment snapshot, not a parallel TCB
ledger: exact dependency IDs resolve back to the generated manifest contracts,
and observations must match the independently supplied target versions and
configuration digests. Both lists must be complete and unique; time is a
nonnegative integer, observations cannot be future-dated, and maximum age is
inclusive. `VerifiedProfileAdmission` abstracts this frozen decision with two
dependencies and explicit finite time/match bounds. Its model mutants remove
version, configuration and expiry guards; production tests own the corresponding
cause/effect rules. This does not prove Python refinement or authenticate either
JSON input. The deployment owner must authenticate and freeze the actual target,
prevent check/use races, invalidate expired evidence, and bind Session execution
to that identity before a runtime may advertise Verified.

### Durable inbox/outbox and event-sourced reducers

`ToolResultProtocol` also checks that a retained Event-batch retry identity
cannot change between accepted and finally closed, including delayed sends.
Its finite model does not contain physical Session deletion or Classroom quota
settlement. Current Session tombstones discard Event-batch provenance; a 404
after deletion therefore remains unknown, not a final non-admission proof.
File-backed SQLite reopen and HTTP isolation tests cover the retained-root
implementation, not deletion recovery or cross-service convergence liveness.

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

## Run ownership and delegation evidence

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

A failed nonterminal Session realization closes its Host lease generation and
expires that assignment in the same durable failure CAS. The existing assignment
Kani harness proves an expired assignment must advance its epoch, including the
same owner/incarnation (or fail closed at u64 exhaustion). SQLite application
rows cover initial retry, terminal initial failure, Idle hot-replacement failure,
and stale replay; HTTP MCP hot-swap drives the retained Active server after both
failed replacement and retry. Together these connect the root write to the
already-modeled closed-generation fence. The scalar proof does not prove SQL
atomicity or an arbitrary transport outage's eventual recovery.

Local coordination admission and child execution reuse one `AbortOnDropTask`
owner. The local Session application call installs a child projection before
execution begins, so it needs the same poll-stack boundary as direct and durable
child execution. Default-stack Coordinator HTTP and scenario-factory tests cover
that complete chain; runtime-host tests require actual application/model-future
Drop receipts when their owning future is dropped. These native checks validate
stack and cancellation behavior. The TLA+ relation does not model Rust frame
sizes or prove Tokio's scheduler/abort implementation, and logical cancellation
alone remains insufficient evidence of physical model termination.

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
