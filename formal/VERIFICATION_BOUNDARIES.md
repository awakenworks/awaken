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

- Session ownership now observes the sole `SessionRootKernel`; the independent
  `SaveOwned` model that could overwrite an owner is removed. The wrapper adds
  only a read snapshot and observation pulse, not another write protocol.
  `SessionOwnershipProof` reuses the root theorem and proves exact read capture,
  coherent visible owner/revision pairs and immutable owner binding over
  unbounded traces for arbitrary permitted kernel parameters. Historical reads
  may remain after deletion. TLC checks concrete lifecycle/read witnesses and
  semantic mutations. One selected identity abstracts unrelated operations as
  stutters; SQL transaction execution, multi-identity concurrency and universal
  implementation refinement remain outside this abstract theorem.

- Session root time remains attached to the existing application CAS. Its
  production observation kernel is directly checked over every `Option<u64>`
  previous observation and `u64` clock, including zero, equal/backwards samples
  and `u64::MAX`: the result is exact, monotonic and repeat-idempotent without
  arithmetic overflow. The actual `SessionMutationResult` interpretation kernel
  accepts the candidate only for Applied and returns that exact revision;
  Replayed requires the existing durable-root read, while Conflict/Mismatch
  reject it. These proofs do not execute SQL transactions, asynchronous reload,
  create-time stamping or an unbounded crash schedule. The whole root-time
  obligation therefore retains `abstract_model_checked`; SessionRoot TLC,
  SQLite crash/race tests and a shared SQLite/Postgres lost-ack/conflict table
  provide complementary evidence, not universal refinement.
  Fresh low-level Replace is also checked against the current durable root in
  one shared store admission, after replay and the owner/revision fence. The
  contract guard directly proves creation-time immutability (including legacy
  unknown) and forbids erasing/regressing a known update over all `Option<u64>`
  inputs. It no longer relies on callers preserving those fields. Both SQL
  adapters exercise the same 17-row rejection/publication/replay table; malformed,
  foreign-identity and revision-inconsistent current rows fail closed at the
  shared decoder gate. Historical unknown update can remain unknown in low-level
  replacements; only the application samples the clock. The SQL/async/refinement
  limits above remain unchanged.
- Terminal reconciliation keeps one production page iterator shared by Host and
  Worker. Kani calls that exact iterator over 0..3 arbitrary byte identities,
  arbitrary usize limits and initial cursor, and every early-drop position
  (unwind 5, default checks). It verifies ordered bounded consumption, advance
  before observation, retention on cancellation, and short/empty-page wrap.
  Native decision-table cases extend lengths to 300 and cover 256/257/MAX limits;
  actual RunId async tests cover failed observers, cancellation, SQL corruption
  and custom adapters returning oversized pages. The iterator enforces the visit
  budget even if such an adapter violates its read limit; adapter allocation is
  still outside that bound. The existing TLC model separately owns conditional
  progress under finite stable rows, fair calls, returning reads and bounded
  restarts. Neither evidence proves Tokio locking, RunId allocation, SQL keyset
  refinement or universal liveness; the whole scan obligation retains its
  abstract-model grade.

- Maintenance keeps its existing Service and Pool/Reconciler entrypoints.
  The shared observation gate now checks 42 executed rules, including 30
  in-flight GC/relay/scan cancellation cuts with successful and failed returns.
  Raw port events and polling the actual shutdown Future establish the observed
  begin/return order and Pending-before-release-before-join boundary. The same
  DispatchMaintenanceScheduling model owns that relation; eight real-trace
  counterfactuals, three schema/coverage negatives and an extra-hint positive
  control qualify it. Three production shutdown mutants (Service, Pool and
  Reconciler returning before their in-flight task joins) are also rejected by
  the native Pending-before-release assertions. This is finite trace
  correspondence, not exhaustive Tokio
  interleaving or universal Rust refinement. Returning I/O, runtime scheduling
  and clock correctness remain explicit premises, and the whole obligation
  keeps its abstract-model grade. Direct Kani qualification remains open:
  with Kani 0.67.0 and Tokio 1.53.1, a harness calling
  `tokio::time::Instant::now()` reaches unsupported C `clock_gettime`
  (1 failed check, 55 undetermined); polling the actual cancelled
  `publish_dispatch_hint` Future did not finish within 240 seconds. Neither
  result is a business counterexample or a proof. Stubbing that clock and
  sequentializing Tokio atomics would require an explicit runtime refinement
  boundary before any wider direct-proof claim.
- ACP prompt and wire claims are maintained in [their bounded topic](verification-boundaries/acp-handshake-and-wire.md).
- Attempt return, participant closure and cancellation are maintained in
  [their bounded topic](verification-boundaries/attempt-return-and-cancellation.md).
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
  begin uses the same unique-key arbitration contract as PostgreSQL. The common
  durable-begin result owns the business-entry and completion-attempt predicates
  consumed by Admin tools and protected HTTP. Kani exhausts presence/equality,
  every signed state, every begin result and both business outcomes, directly
  proving local absent/replay gates. AuditCommit owns multi-scope, identity,
  atomicity and crash schedules; backend tests cover tuple isolation, replay,
  migration/cross-connection behavior. SQL/heap equality, durability and async effects remain outside the kernel; identity, atomicity, HTTP completion
  and crash preservation retain model grades.
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
- BackgroundTask's single process-local Active/Waiting/Completed slot retains
  owner/epoch under one mutex; only a higher committed epoch replaces it. The
  supervisor's sole completion-fold plan returns an exact candidate without
  consuming it, retires only a strictly older epoch, and preserves absent,
  same-epoch-different-owner or newer evidence. Kani exhausts attempt presence,
  owner equality and both u64 epochs. Native tables require an identical staged
  command after commit loss; five Loom checks cover selected two-thread races.
  This closes only the process-local candidate-to-fold kernel: caller freshness,
  same-epoch callbacks, ThreadCommit/database durability, Tokio/async scheduling,
  external effects and whole-driver refinement remain unproved or in the TCB.
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
  Both durable claim adapters also consume the same refresh-age cutoff in
  `schema.rs`. Its full-width Kani proof uses an independent signed i128 oracle:
  absent age or age greater than now selects no cutoff; otherwise the exact
  difference is clamped to the signed database ceiling. Equality retains
  `Some(0)`, so a refresh at zero remains eligible. A 72-row native boundary
  table and the same four-rule SQLite/PostgreSQL adapter table cross-check
  absent/too-old age, the exact zero cutoff and a later refreshed lease.
  The adjacent deadline proof calls the existing database conversion, requested
  TTL normalization and lease-expiry functions over their full u64 domains.
  Independent u128 arithmetic establishes the default 60 seconds, the minimum
  requested second and the exact millisecond deadline capped to i64::MAX;
  512 native rows cover unit-conversion and signed/unsigned overflow boundaries.
  At the signed ceiling, a positive TTL need not be strictly in the future.
  This proves the numeric clock rules, not SQL predicate evaluation, the
  separate age-reclaim/claim transaction sequence, clock behavior, or complete
  WorkQueue refinement. The exact-boundary obligation keeps its abstract grade.
  The test-support in-memory reference queue additionally owns Work rows,
  token digests and its existing LeaseBook under one aggregate mutex. Four
  actual-port Loom scenarios cover release/wake/reacquire, token authentication
  against reclaim, owner teardown against reclaim, and renewal against reclaim;
  each uses two competing threads with at most two preemptions. The original
  release and token-authentication implementations both fail these scenarios.
  These bounded reference-adapter checks do not prove SQLite/PostgreSQL isolation,
  arbitrary schedules or token entropy, and neither atomicity/release ledger row
  is promoted beyond its existing abstract theorem grade.
- Config ordinary and audited CAS consume one production row-admission owner
  before their shared SQLite/PostgreSQL statement seam. It distinguishes absence
  from present zero, admits absence only for expected zero, admits a present row
  only for its positive exact expectation, and derives the checked successor.
  Kani exhausts `Option<u64> x u64` without assumptions. SQLite holds an
  `IMMEDIATE` transaction and PostgreSQL locks present rows; the final SQL keeps
  the same predicate for absent-row races and defense in depth. AggregateCAS,
  adapter tables, concurrency, rollback and reopen tests cross-check this result.
  SQL interpretation, `RETURNING`, locking, isolation and atomicity remain TCB;
  separate unconditional-upsert arithmetic keeps global monotonicity abstract.
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
  `AcpObservationRefresh.tla` separately checks the ACP adapter projection:
  one backend, two references and two serialized refreshes, with mutable host
  state and reply-backend identity between steps. Seven mutants reject
  per-Workspace duplicate probes, credential-derived protocol cardinality,
  cross-reference login borrowing, cross-refresh cached evidence and foreign
  backend evidence in credentials, cached capabilities and fallback probes.
  Seven witnesses retain useful projections and reach each foreign-reply denial.
  The shared discovery consumer scopes replies before caching or projection;
  the production process adapter already emits the requested catalog identity.
  Rust rows cross two CLIs, all three-binding status combinations, negotiation
  failure and foreign-backend loss/recovery across 72 actual refreshes, including
  zero bindings. Six isolated production mutants independently qualify identity
  checks, the three discovery consumers and both post-probe equality fences.
  This does not prove external revocation, per-use revalidation, concurrent
  custom consumers or Rust refinement.
- ACP WorkerLocal binding extension validates the complete input batch and
  publishes it under the same resolver mutex. Rejection cannot leak a prefix
  insertion or status update. `AcpLocalBindingBatch.tla` covers two writers,
  three keys, two revisions, independent Active/Disabled updates and one reader.
  Per-key writes occur inside the retained lock; each successful commit is
  complete, rejection stutters and observations match a committed serial prefix.
  An accepted later status update may replace an earlier accepted value. Six
  semantic mutants check these boundaries; six witnesses require both writer
  orders, legal status replacement and an observation between commits.
  Native threads and Loom execute the same production constructor/add_bindings
  methods with 24 meaningful writer/status/initial-state combinations plus four
  malformed-input races. The complete map, both results, observer and consecutive
  replays must agree with one serial order. Only the mutex is instrumented;
  no preemption, permutation or duration truncation is allowed. This is bounded
  Rust schedule evidence, not Kani or universal Rust/Mutex refinement. Arbitrary
  input sizes, external projection replacement, delivery, revision rotation,
  panic/poison recovery and asynchronous login freshness remain outside it.
  No second credential store or transition implementation is added.
  The Worker-owned implementation now resides in
  `awaken-run-executor-acp::host`; Coordinator registration translates its
  Control rows into the same exact binding DTO.
- `AcpProjectedBindingUse.tla` checks two consumers under one async gate with
  at most two external file replacements (active, rotated revision, different
  CLI, disabled, absent, invalid), with matching or foreign probe replies.
  Five mutants check post-probe comparison, exact CLI binding, serialization,
  cancellation release and probe-backend identity; four witnesses require
  successful use, revoked denial, a peer succeeding after cancel and a foreign
  probe denied despite an unchanged binding. Executable tables exercise
  Codex/Claude, all login states, actual bounded file reads/replacements and a
  paused login probe, both heartbeat and exact use ports, and standalone Worker
  configuration/assembly. The existing await-barrier table now crosses all seven
  file changes with both ports and matching, foreign-known and unknown reply
  backends (42 rows); no second selector or projection store is added. This proves neither
  automatic authenticated projection delivery nor rollback/ABA detection,
  OS IO correctness, the final check-to-launch interval, arbitrary live cloud
  failures or Rust refinement. AllInOne source lifecycle refresh remains a
  separate integration obligation; these bounds must not be reported as full
  cloud/local or Windows/macOS production qualification.
  The shared resource E2E additionally selects a shipped standalone Worker
  process, signed HTTP-over-TLS transport, exact registry observations,
  projected disable/delete/restore, and Worker-only graceful/abrupt replacement.
  Its unchanged File/Memory/Repository oracle checks bytes, physical roots,
  conflict retention and terminal tombstones. This is executable integration
  evidence, not an extension of the model's state space or proof grade. Local
  probe intervals and Coordinator heartbeat publication have separate clocks.
  The Worker now rejects a negotiated budget unless evidence TTL strictly exceeds
  probe interval + renewal interval + request timeout, at initial admission and
  each subsequent lease receipt. `observation_publication_budget_is_strict_and_overflow_safe`
  verifies the actual pure Rust kernel over all Duration seconds/nanoseconds,
  using an independent u128 sum oracle. This is scalar arithmetic proof, not an
  availability theorem for delayed/failed probes, network delivery or scheduling.
  Lifecycle integration verifies initial withdrawal and later absorbing drain.
  The production kernel module can be verified directly with standalone Kani;
  cargo-kani 0.67 currently fails artifact discovery with a separate global Cargo
  build-dir. No build-directory override is needed for the standalone proof.
- Ordinary Provider and Managed Resource publication share Vault's exact material
  handoff. Native/Claude decision tables cover recipient-envelope success and
  issuer failure, substituted fingerprint/recipient, expiry and empty identity;
  inactive or missing sources cannot reach issuance. This is executable
  integration coverage, not a new cryptographic proof or proof of automatic
  self-hosted Control-to-Worker delivery. Existing envelope contracts retain
  authority over authenticated binding and recipient admission.
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
  contain the provider host path. Kani exhausts the production path-fidelity
  requirement/advertisement gate. The contract's native decision table checks
  the exact root, child, prefix-lookalike, traversal, provider-host and
  runtime-owned path classes. The real Linux Namespace/Hand conformance test
  exercises successful and failing filesystem
  tools, Bash and Git against `/workspace`, including Debug and serialized wire
  outcomes; `HandToolContext` diagnostics expose only shape, never provider
  roots, environment values or launcher data. Provider bind construction, mount
  namespace, kernel, Git, filesystem, arbitrary process output, serialization
  correctness and unobserved operator diagnostics remain executable or
  environmental boundaries rather than consequences of the decision kernel.
- The Session application has one internal acceptance point that returns only
  after the complete root and idempotency receipt are durable; the existing
  lifecycle supervisor then resumes realization from that root without a Job
  aggregate. `SessionStartProtocol.tla` proves that internal acceptance cannot
  precede the complete root. The standard Managed `/v1/sessions` adapter waits
  for creation and returns the official synchronous 200 shape; no public async
  status projection is claimed at this checkpoint. Database durability,
  scheduling fairness, and HTTP delivery remain outside the theorem.
- Session activity receipts retain the unbounded abstract theorem in
  `SessionActivityProof.tla`. The four existing `PersistedSession` activity and
  execution methods now delegate to one reducer borrowing the actual scalar,
  `BTreeSet` and execution fields. There is no second receipt map or state owner.
  A 7,840-row native table checks complete aggregate frames across admission,
  exact settlement, legacy settlement, terminal clearing and terminal replay or
  rejection. Eight production mutations qualify the table, including borrowing
  the wrong persisted scalar. Two Kani base cases and four Next/Exact two-call
  partitions call the real reducer from an empty set and Idle execution over
  every `u64` historical/requested value. They check exact nonwrapping result,
  scalar, membership and execution frame after each call, including exhaustion
  and actual insert/search/Drop. A separate proof starts Running, uses two real
  Next admissions from any historical scalar to produce 0..2 members, and checks
  both terminal targets, complete clearing, the scalar frame, terminal replay
  and rejection of Running. No uniqueness or tree-height conclusion is assumed.
  All seven harnesses keep global unwind four and every default safety/unwinding
  assertion. `scripts/ci/formal_kani_options.py` is the single runtime/checker
  owner of the pinned std height-loop profiles: eight bound-one loops for
  admission and seven for terminal clearing. Other loops retain the global
  bound; any reachable extra height iteration is a proof failure, not an
  assumption. Unqualified raw overrides, wrong package/harness bindings, duplicate
  selections and appended check-disabling options are rejected.
  Two actual production mutations fail the named Kani assertions: omitting
  terminal clear fails T2; skipping Exact insertion only when the set is
  nonempty fails M1 while preserving the empty-set base behavior. Matching
  green baselines, source/argv binding and retained checks qualify both failures.
  Real removal followed by two completions remains unproved: combined and
  input-partitioned probes exceeded the 12 GiB solver memory guard; checked
  per-loop path exploration reached its 600-second deadline. These failures
  are not registered as CI proofs. Arbitrary preexisting sets, unbounded traces
  and the full constructor's MCP/serde/hash path remain outside this boundary.
  Empty Running legacy settlement can return `Some(true)` repeatedly before
  the application commits Idle. SQL receipts, durable CAS, interval closure and
  application exactly-once settlement remain outside this executable aggregate
  evidence; the obligation retains its abstract-theorem grade.
- The committed-event Runtime projection has one closed pending-tool reducer.
  `SessionRuntimeProjection.tla` checks replacement, accepted-but-unprocessed
  reply classification, per-id processing, terminal clearing, and error evidence.
  Active aggregate authority and accepted-but-unprocessed replies block the client
  send control; server admission may retain behind the sole lifecycle selector. Two additional
  mutants cover hidden errors and presentation accidentally releasing admission. Within the configured finite prefix domain (`0..3`), it also checks
  that a successful refresh advances the paired result and source checkpoint to
  the observed committed prefix, while a failed refresh stutters. Browser tests cover
  TypeScript projection; list status mapping rejects unknown states.
  `ManagedProjectionPublish.tla` checks two reverse-completing
  refresh writers over independent Session, root/child Thread, lifecycle, and
  budget source coordinates, including a source advance between the first and
  second read. It checks mixed-read rejection, result/checkpoint pairing, local
  cache CAS, source non-invention, visible non-regression, failure/rejection
  stuttering, and that equal source coordinates plus equal rendered content
  change neither cache nor broadcast. Equal legacy coordinates with changed
  rendered content still publish rather than hiding an unversioned source.
  The original three numeric clocks/version-two/cache-four configuration is
  retained. Additional configurations cover two writers with an optional child,
  and the complete one-step decision input boundary with two numeric coordinates
  and two independently absent/zero/one child keys. Missing a zero-version child
  or renaming that child is a regression; adding a zero-version child is a change.
  The input configuration covers stable lagging candidates even when an ordinary
  monotonic read schedule cannot reach the regression branch. Nine semantic
  mutants and six witnesses qualify the decision, actual cache increment, paired
  result/checkpoint and broadcast. Binding the receipt to the visible cache closes
  a former gap where Safety accepted a ghost increment without an actual increment.
  The existing Rust decision test now checks 419,904 real HashMap/String calls
  across all four scalar clocks, two child identities, unsigned boundaries,
  cache CAS and content changes. The actual publisher test also requires one
  visible cache increment. These are native cross-checks alongside the bounded
  models, not an unbounded HashMap/serde/mutex or Rust-to-model refinement.
  Reading/built failure preserves the current cache, including a competing
  publisher's success. Three failure mutants/three witnesses qualify this in
  a two-writer/one-clock/version-one/cache-two slice. Native full-record frames
  check read cuts, private-reduction errors, cache exhaustion and budget expiry; coverage
  records exact cases and actual mutants. The complete async chain remains unrefined.
  Together these models check bounded pending-reply/publication safety, not SQL
  snapshot isolation, pre-anchor cold history, browser delivery or supervisor fairness.
  Direct Kani evidence additionally calls the production reply-anchor selector
  over 0..3 lifecycle facts, all seven event kinds, exact/mismatching Thread and
  Run identity classes, and every u64 commit, lifecycle cursor and snapshot
  fence. It returns precisely the greatest nonzero same-owner Awaiting commit
  within the fence, or the exact retryable Unavailable error. Native cause tables
  cover competing order, duplicate commits, missing owners, and both numeric
  boundaries. This selector proof does not prove arbitrary identity codecs,
  unbounded history, async prefix collection, SQL snapshots or durable reply admission.
  The production kernel maps Pending/unknown to rejection, Resolving to durable queuing
  without request-local drive, and Processed to ready; browser send blocks Pending/Resolving.
  `SessionRuntimeProjection.tla` also checks opening identity with the bounded
  `SessionRuntimeProjectionOpening.cfg`: one retained opening, up to three
  source commits and one later interval, arbitrary live reads, terminal
  observation and cold rebuild, including a terminal-only first interval.
  Both projections must retain the first admitted opening; choosing the latest
  Running after terminal or borrowing the next interval's Running for a
  terminal-only predecessor is rejected by the Cowork lifecycle mutants.
  A successful cold read must return the opening; an empty result is distinct
  from no read yet and is rejected by a separate omission mutant.
  The existing Rust recovery test exercises 24 source/terminal combinations,
  three observation cuts, warm/cold equality and bidirectional pagination.
  This is abstract model checking, not Rust refinement: Run/Thread selection,
  legacy admission anchors and actual process failure are executable-test scope.
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
  `ToolExecutionPolicy`; one resolver owns Managed normalization and MCP-policy
  completion. Kani exhausts both sources and all optional default axes, and proves
  one exact override replaces only its matching default. `ToolPermissionPolicy.tla`
  checks the same abstract rules; runtime tests cross-check shared Native/ACP
  authorization. Wire admission, arbitrary names/counts, assembly, external effects,
  and universal Rust-to-TLA refinement remain evidence boundaries.
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
- Session post-activation MCP publication has one application-owned collection
  plan. Activate consumes it; ACK and recovery reuse the same generation
  selectors and exact receipt bijection. A 144-row native decision table starts
  with the real initial/replacement constructors and then claims, activates,
  renews, acknowledges and removes generations. It checks unique identities,
  exact staged receipt ownership, publish/drain disjointness, no root mutation
  and rejection of missing, duplicate, future-expiry or wrong-identity live
  receipts. Five actual production mutants break generation allocation,
  retirement, publication selection, bijection or drain coverage and are rejected
  by those assertions. This is executable combination evidence, not a completed
  Kani proof of the owned Vec/BTreeSet/Drop paths or durable/HTTP refinement.
  The publication-excludes-retired obligation retains its abstract-model grade.
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

- Session network restriction retains one sorted, deduplicated Vec and uses
  stable filtering for meet; its empty result shares the canonical `None`
  constructor. TLAPS proves six sequence-selection properties over arbitrary
  indexed collections, alongside the existing meet algebra. Exact source
  positions and complete membership give exact intersection; increasing and
  injective positions preserve order and uniqueness. Three concrete witnesses
  establish the need for these premises. These mathematical selection premises
  are not a universal refinement of Rust library calls. TLC checks the existing
  three-host/blank model and an 84-case sequence oracle; five model mutations
  are rejected. Native normalization exhausts 16,515 policies with at most two
  one-byte ASCII hosts, while meet crosses 25,281 pairs over twelve representative
  bytes. Eight production mutations, Unicode property tests and SQLite root
  readback cross-check the implementation. Five direct Kani partitions jointly
  prove exact normalization for all 16,515 ASCII policies: None, Unrestricted,
  and allowlists of zero, one or two hosts. Real String/Vec sort, dedup, retain
  and Drop run with unwind 3 and default checks, without stubs or proof cuts.
  The input constructor is independently exhausted; 144 native size/order cases
  extend collection coverage to 257 hosts using the same bitmap oracle.
  Collecting before blank filtering preserves input length through sorting;
  blank-heavy inputs use more temporary slots than early filtering.
  Twenty-five disjoint Kani partitions also prove the actual intersection
  composition for all 272,745,225 ordered pairs in that same raw-policy domain.
  Both real normalizations, binary search, filtering and owned Drop remain
  enabled. The oracle checks exact bitmap content, strict ordering, uniqueness
  and enum representation; independent parameter audits check partition coverage.
  Actual no-fold, no-dedup and widening Kani mutants violate the intended
  semantic assertions. This qualifies the bounded production-kernel grade;
  empty-string host inputs, multi-byte strings, longer vectors, universal Rust
  refinement, persistence, DNS and provider enforcement remain outside it.

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

Source-only publication shares complete Source/WAL admission on
`CredentialMutationIntent` across three adapters. SQL, locks and affected-row
checks remain adapter-owned; distinct replay does not lock its predecessor.
Native qualification checks 3,392 snapshot cells, 65 Source/stage cells and
16 WAL/phase cells, including live PostgreSQL. Five policy mutants fail on all
adapters; three phase-write mutants break replay; one catches late phase checks.
pre-Cow complete-method Kani probe (fixed identities/material-free, full i64,
real heap/Drop/default checks) hit the 600-second guard; no PASS is registered.
Arbitrary heap/SQL/SecretStore refinement and application successor arithmetic
remain outside these witnesses; the whole row stays `abstract_model_checked`.

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

Rollout payload identity already has one production owner: the derived
`ManagedCredentialRollout` equality in the Credential contract. InMemory,
SQLite and PostgreSQL consume that equality under their existing lock or
transaction for insert replay and acknowledgement; there is no second
proof-only identity predicate. Its Kani harness exercises actual equality,
inequality, String allocation and Drop with five independent 0..2-byte ASCII
strings per event (including NUL), both full-width u64 revisions and all four
operations, at unwind 6. A raw-byte/length/tag oracle independently checks all
eight fields. Native tests exhaust the 256 differing-field subsets plus each
string's length, prefix, empty and unused-byte cases. The shared real-backend
suite checks exact commit replay, seven individual non-key payload collisions,
absent-key no-op, exact acknowledgement and repeated acknowledgement while
checking the committed pair and outbox after rejected acknowledgements. A
subsequent valid mutation reuses that pending event id with a new child
revision; all three adapters reject publication and retain the original pair,
outbox and unapplied WAL before exact abort cleanup. The same suite also runs
left-first, right-first and concurrent-join schedules for two valid transactions
with distinct source/child/vault identities and one event id. Both source and
child revisions change, making partial pair publication observable. It checks
the winning Reclaiming WAL, rejected Ready WAL and unrelated event/pair/WAL
frames. Once the winner is exactly acknowledged, the rejected transaction can
publish under the released id; a delayed old-payload ACK rejects and exact old
commit replay preserves the successor event. PostgreSQL uses distinct parent
rows, so the two contenders do not first serialize at the vault lock; this is
still an executable witness, not exhaustive SQL scheduling. Native production
guard mutations qualify collision rejection, delayed-ACK rejection and SQL
rollback against these new rules, independently of the earlier sequential cases.

A direct Kani probe of the actual InMemory `complete_managed_rollout` Future,
with its existing Mutex/HashMap/String/Drop and no stubs, fails even for an
absent key: Mutex unlock reaches unsupported libc `syscall` through
`futex_wake_all`. The rejected probe is not a qualified proof harness and adds
no implementation-composition coverage. A tool-supported treatment of the real
synchronization and storage paths is still required before that boundary closes.
This adds bounded production equality evidence; arbitrary strings, JSON
projection, insert-collision rollback and transaction composition remain outside
the kernel proof. The complete event replay/ack obligation therefore keeps its
abstract-model grade.

Single-event rollout delivery additionally has a controlled-port executable
oracle for the actual `reconcile_managed_credential_rollout` Future. Its 540 rows
cross Pending plus every closed target error, immediate completion or one wait,
cancellation during that wait, all four operations and revisions 0/1/u64::MAX.
IDs and payload-bearing errors use fixed one-byte strings. A stateless monitor
rejects every repository method synchronously when called, before a returned
Future could be polled; it contains no persistence algorithm. Thus this checks
that neither completion nor cancellation on these non-converged schedules can
call the acknowledgement port. The existing InMemory rollout test separately
checks durable outbox retention, error redaction and eventual exact ACK. Four
production mutations are rejected by the executable oracle; the existing
TLC all-target adoption invariant rejects first-target-only convergence.
The whole obligation retains its abstract-model grade. These executable rows
are not a completed Kani proof of the Future, Session busy selection, arbitrary
strings/subscribers, SQL/crash durability or general executor liveness.

The Source and Managed creation envelopes share `CredentialMaterialMutationFence`.
Its material-input projection is also shared by both fresh-command and durable
validation paths, replacing four copies of reference-set/attempt-suffix logic.
Material lifecycle sets use the byte-lexical order derived by `SecretRef`, so
cleanup's ordered union already supplies unique sorted deletion references;
the former HashSet-plus-Vec-sort path is removed.
Preparation, input admission and Ready recovery share the same borrowed
candidate-delta iterator. It extends its predecessor set directly, without the
FromIterator temporary Vec/sort, and preserves candidate order and repeated new
aliases. The independent 5,184-row executable oracle checks this exact sequence;
input admission consumes it once for both facts. Exact suffix checks borrow the
attempt and fixed separator,
without allocating a formatted suffix. The no-predecessor Kani harness executes
the actual BTreeSet/BTreeMap/String projection with primary and one auxiliary
candidate slot, six reference classes and optional attempt. The corresponding
heap proof with a predecessor remains outstanding. Collection loops use unwind 3
and builtin memcmp uses 16; every unwinding assertion remains enabled. The executable
oracle covers 5,184 input rows across both predecessor states, including slot migration, duplicates,
retained refs, mixed valid/foreign refs and near-matching suffixes. No set or
comparison stubs are used. A separate executable table checks first and later
references against the physical namespace grammar for empty, UUID-shaped,
separator-bearing and Unicode attempt ids; it does not widen the Kani bound.
The cleanup decision table also asserts the exact
unique lexical delete sequence, including aliases across slots. Arbitrary
strings/collection sizes, command-validation composition and external material
IO remain outside these bounded checks.
Source and Managed Ready recovery also share the existing candidate-read decision
in `cleanup`: retained refs are not reopened, missing candidate material selects
abort and unavailable reads retain the WAL for retry. Its cause/effect unit table
includes mixed missing/unavailable candidate reads in both encounter orders;
its existing recovery/backend tests are executable evidence, not an async
refinement theorem or proof of SecretStore observations.

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
The six adapters now use canonical claim success plus complete locked snapshot
equality without a second phase recheck. Shared backend rules retain independent token, epoch,
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
rewrite a Resident or Retiring fence. The Candidate receipt reducer additionally
distinguishes the prior source identity (`AdoptionFrom`) from the current
committed Adopt. Same-generation source conversion is allowed; after current
commit, a different receipt is rejected. Normal publication and post-dispatch
failure retention consume this same decision. Cold/warm owner and SQLite claim
replacement tests exercise the production mapping and retry sequence; the Kani
harness exhausts the closed origin, exactness and compatibility inputs. It does
not prove the Store/Provider's physical truth or arbitrary async interleavings.

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

`verification_manifest.py` is the one release/TCB admission validator; `--runtime-fingerprint`
supplies an independent snapshot, not a parallel ledger; IDs resolve to manifest
contracts and observations match targets. Both lists are complete and unique; time
is nonnegative, observations are not future-dated, and maximum age is inclusive.
Every success consumes `awaken-service-lifecycle::release_admission` through its
CLI. Kani exhausts release facts and exact/fresh TCB scalars over all `u64`
times; checked subtraction rejects future observations without underflow.
`VerifiedProfileAdmission` and its mutants remain independent bounded
cross-evidence, not Python/Rust refinement. Python parsing/fact projection,
JSON/String behavior, Cargo/rustc, subprocesses, SHA-256, and authentication
remain boundaries. The deployment owner must freeze the target, prevent
check/use races, invalidate expired evidence, and bind Session execution to
that identity before advertising Verified.

### Durable inbox/outbox and event-sourced reducers

`ToolResultProtocol` also checks that a retained Event-batch retry identity
cannot change between accepted and finally closed, including delayed sends.
The model excludes deletion and Classroom quota settlement. The delete CAS
retains content-free keyed receipts from its locked root. Scoped
tombstone reads distinguish legacy unknown from complete inventory: exact keys
replay Accepted/Closed, changed fingerprints conflict, absent keys return the
deletion revision; a bare 404 remains unknown. SQLite/PostgreSQL tests check
ordered multi-key replay, not universal SQL refinement or convergence liveness.

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

Local coordination admission and child execution reuse one `AbortOnDropTask` owner. The local
Session application call installs a child projection before execution and needs the same poll-stack
boundary as direct and durable child execution. Default-stack Coordinator HTTP and scenario-factory
tests cover that chain; runtime-host tests require actual application/model-future Drop receipts.
These native checks validate stack and cancellation behavior. The TLA+ relation does not model Rust
frame sizes or prove Tokio abort scheduling; logical cancellation alone does not prove termination.
Delegated child invocation consumes the sole durable `DelegationRegistry` immediately before
`executor.start`; Kani proves only an exact Open relationship admits this scalar gate. Native tests
cross the real aggregate and closed recovery; serialization, durability, scheduling and executor behavior remain executable evidence or TCB.

The Managed WorkQueue proof covers queue state, transactional single-active
claim, lease epoch/expiry, heartbeat compare-and-set, exact-boundary reclaim,
and terminal authority clearing. The official worker header is persisted as
the claim owner, including before the first heartbeat; only that worker may
advance the `first`/`matching last_heartbeat` condition. Stop consumes the exact
owner-and-epoch lease receipt, so a paused same-incarnation predecessor cannot
retire a higher-epoch successor. Parameterized TLAPS lemmas bind every existing
kernel Stop effect to its request, prove that owner/epoch mismatch permits only
unchanged authority, and frame every other Work row. The original finite TLC
configuration additionally checks this action property; independently deleting
the owner or epoch guard must violate that precise oracle, even when ordinary
queue Safety still holds. A reachable legal Stop qualifies the terminal checks.
The specification permits unlabeled Stopped stutter but contains no replay
request or boolean result; this cannot establish the stores' replay response.

The existing in-memory, SQLite and PostgreSQL conformance suite independently
alters each receipt's owner, epoch, Work id, Environment id and Session id. Every
rejection preserves the complete Work projection and current lease. Exact release
clears authority; Stopped/same-epoch replay with either the former or a different
owner returns true without changing state or recreating authority. This preserves
the existing response-loss replay contract: its success is not another release
effect authorized by that owner. These adapter cases cross-check the abstract
request boundary, without proving Rust/SQL refinement or arbitrary concurrent
histories; the release row retains its abstract theorem grade. Stop remains
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
