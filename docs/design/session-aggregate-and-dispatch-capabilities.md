# Session operations and Dispatch roles

`PersistedSession` retains its existing serialized DTO shape. Application code
loads it into `SessionAggregate::try_from`, reads through `snapshot()` or its
immutable `Deref`, and changes it through the activity, lifecycle, cleanup,
environment, resources, MCP, configuration, assignment, budget and events
operations. There is no `DerefMut`, mutable DTO getter or arbitrary mutation
callback. Commands validate the whole candidate before replacing working state.
`compile_mutation` feeds the existing repository CAS and replay identity; the
aggregate adds no persistence store, revision source or transaction authority.

## Role Catalog

Session Runtime composition uses `SessionExecutor::from(Arc<T>)`, where `T`
implements `CompleteSessionRuntime`. Its thirteen role handles come from that
one owner and are immutable after construction. `SessionRuntime` retains four
core operations; coordination reads, projection reads, realization, terminal
cleanup, environment, budget and the other calling roles have explicit ports.
Required capabilities cannot disappear behind optional getters or be replaced
with a different Runtime owner after construction.

Dispatch contracts expose calling roles instead of a queue that every adapter
must pretend to implement. Select the smallest role required by a consumer:

| Consumer | Required authority |
| --- | --- |
| Submitter | `DispatchEnqueue` |
| Operator | `DispatchAdministration` |
| Claimed executor | Worker `Dispatch` bundle |
| Local executor | `LocalWorkerDispatch`, including the two exact epoch fences |
| Terminal discovery | `TerminalRecoveryDispatch` |
| Attempt evidence reader | `PhysicalAttemptReads` |
| Original Run input reader | `SessionRunAdmissionReads` |
| Reservation writer | `SessionRunReservations` |
| Reservation fence holder | `SessionReservationAuthority` |
| Local scheduling service | Execution-owned `ControlDispatch` bundle |
| Authenticated Worker HTTP server | `WorkerDispatchAuthority` |

The remaining leaf roles cover claims, execution, claim ownership, commit
authority, settlement, ordinary reads, maintenance, attempt control, control
claims and control reads. `InboxRead` is separate from Inbox writes. Role
bundles live with their consumers; the existing contract crate owns the leaves.

`SessionRunAdmissionReads` extracts the existing original-request/completion
query from reservation commands. Memory, SQLite, PostgreSQL and the Coordinator
erasure delegate to the same original implementation. A reader receives no
reserve, activate, reject, enqueue or lease authority. Missing and legacy facts
remain unknown; the query cannot mint a replay or recovery admission. Existing
reservation writers inherit this read role and keep their command semantics.

Physical Install and Replace retain an `AttemptAdmitted` operation in the same
Dispatch transaction as their slot mutation. The existing operational feed
returns this original entry identity; exact retry and refusal add no entry.
This inventory is distinct from the existing ordinary-return and independent
closure meanings, and grants neither replay nor stop authority. Old streams
without retained entries remain incomplete evidence for whole-execution recovery.

Session execution observation asks the existing `SessionCoordinationReads` role
for recovery evidence. Its Execution implementation joins the operational feed
and original records from one canonical Dispatch instance. The existing committed
`SessionProjectionSource` supplies each Thread's history; no new history or cache
authority is introduced. `DispatchAttemptRecordReads` owns the original retained
query, reused by Control and this execution query. Public diagnostic pages keep
their existing projection. Admission, ordinary return and independent closure
remain distinct evidence classes; the Session contract contains only original
record coordinates and does not depend upward on Dispatch.

The execution-owned `NativeAttemptFootprint` is the single decoder of original
Native coverage, model operations and tool entry domains. Physical retirement
and Session recovery both use that decoder through `awaken-dispatch-execution`;
the existing `awaken-dispatch` exports remain compatible. Coverage must have
its own positive commit coordinate strictly before every entry. Missing,
unaligned or reordered history, and duplicate coverage, cannot acquire a
response-only footprint through a weaker second parser. The recovery query also
retains its independent refusal of any recorded tool batch or tool-use message.
This consolidation supplies neither historical containment nor a cleanup receipt.

An already-closing Dispatch entry does not imply that the original Worker exit
has been observed or retained. When complete-attempt evidence is missing,
unavailable or unknown, the same stop-admission owner can still use
`independently_close_worker` to observe that exact expired/closed incarnation,
commit its Registry exit and release its provider evidence. Live Workers remain
protected. Exact complete-attempt evidence keeps its existing retirement path
without requiring a whole Worker stop; missing complete evidence still retains
the original physical slot and every external obligation.

Closed-Worker maintenance first observes natural exit without permission to
stop an active process. Both occupied-attempt and idle maintenance persist and
read back that fact through one Ingress helper. A retained physical slot or a
failed slot query can still prevent evidence release, but cannot erase or hide
an independently observed exit. Only an empty exact slot scan and an expired
Registry lease permit idle maintenance to stop a still-active process. Physical
retirement, provider retention release and Session recovery keep their existing
separate admissions.

The existing closed-Worker command carries that exact incarnation's immutable
Create ledger anchor from Registry. Kubernetes evidence release seals this same
provider ledger and original request group before releasing the Worker Pod
retention fence. Closed-idle release uses this same seal/join implementation.
A missing
historical anchor stays absent; a missing, foreign or unavailable recorded
ledger refuses release and retries with the already retained process exit.
The seal excludes future Create admission, not existing participant execution or
cleanup. Its existing per-group join still owns completion of admitted requests.

Local composition constructs `AnyCoordinatorDispatch` from an owner implementing
`DurableDispatch`, then uses `DispatchBinding::local`. Its cached Worker view
retains the exact queue owner. Remote composition uses
`DispatchBinding::remote(owner, claimed_commit)`: the owner must implement
`RemoteDispatch`, including checkpoint and credential RPC. Claim-fenced commit
is mandatory. Bindings have private fields and expose no mutable capability
replacement or optional control/RPC probes.

`AnyDispatchStore` exposes only Worker roles. An HTTP Worker cannot acquire
Coordinator administration, maintenance, Inbox writes, global scans or local
epoch guards. Those calls fail to compile. Local controllers and services reject
a Worker from another bound queue before starting work. Remote scheduling omits
local maintenance and relays; the original 42 scheduler observations cover
the selection, including failures, timing and joined shutdown.

`DispatchWorker::new` and `new_bound` require the existing `OperationCoordinator`
with their combined local recovery source. Both erased commit roles retain the
same actual owner before a projection is installed. `GuardedRunCommit::new`
forwards versioned operations to that owner's atomic CAS and persisted receipt;
it never derives a receipt from an ordinary commit sequence. Exact replay returns
the original sequence and version without another append. `from_plain` retains
only the existing ordinary commit meaning and rejects a versioned operation
before taking a lock or calling the plain commit. `ClaimedRunCommit` requires an
explicit operation implementation; it has no ordinary-to-operation default.
Both guarded write paths
retain the same dispatch epoch through validation, commit and observed release.
`from_parts` supports a direct local committed reader. Adding a projection to
that split configuration requires a source and is rejected during configuration
if absent. Remote `from_binding` and `new_bound` always install one required
claim-scoped projection. Replacing the cache preserves its selected source or
RPC and changes all reader references together.

Validation combines compile-fail capability boundaries, exact owner and reader
identity tests, Session transition/replay/overflow matrices, the 54-row checkpoint
matrix, scheduler observations, the existing backend conformance suites and
real authenticated HTTP tests. The checkpoint matrix checks both outcomes and
call counts: an unavailable local guard never falls back to remote RPC, and a
remote operation never acquires a local guard. Genuine optional components,
such as an installed local stream checkpoint store, remain optional.

Worker observations retain one Registry authority and use explicit read roles.
`WorkerObservationSource` supplies only the roster. The new `WorkerCurrentSource`
and `WorkerIncarnationSource` supertraits require logical-current and exact-
incarnation queries respectively. `WorkerDirectory` inherits both and defines
only mutations and the incarnation guard. These are new query roles owned by the
existing Worker contract; they add no registry, storage, state or HTTP route.

| Read implementation | Required roles |
| --- | --- |
| Memory, SQLite and PostgreSQL Registry | Roster, current and exact incarnation |
| Coordinator HTTP adapter | Roster and exact incarnation |
| List-only consumers and fixtures | Roster |

Current-only fixtures retain the original exact-identity fallback through one
`read_current_incarnation` helper. It performs one current query and filters the
complete identity; it does not reconstruct history from a roster. The HTTP
adapter uses the original authenticated domain route and complete DTO. Only an
exact successful domain response yields the old RegisteredWorker; missing history
remains absent, query failure remains an error, and list data never supplies a
missing capability. Compile-fail boundaries reject point queries on roster-only
sources and logical-current queries on the HTTP adapter.

The query roles project facts only. They do not prove physical execution or
external requests closed, original-domain access, participant disposal or cleanup
completion. Those facts and the full P0 safe-cleanup workflow remain pending until
the existing owners supply exact evidence and the Session aggregate accepts it.

Pool composition validates the resolver's required immutable dispatch queue
before spawning any drain, renewal, depth or maintenance task. Empty lease owners
are rejected at this same boundary. Every claim cycle verifies the queue again
before claiming or entering the resolver, including its provider effects.
Both ordinary and retry-exhausted resolution return a Worker; the Pool checks
its exact queue identity before driving or terminalizing it. A resolver cannot
override retry terminalization and bypass this shared execution boundary.

The full durable claim, rather than the Worker's independent-claim default,
supplies the Pool attempt's owner and renewal authority. Existing claim fences
continue to protect commits and settlement. Resolver binding rejection is a
construction error; dynamic Worker mismatch retains and defers the original
request without committing or settling through the foreign queue. Process
composition propagates construction errors before starting downstream services.
Cause/effect matrices cover foreign resolver queues, empty owners, ordinary and
retry-exhausted claims, foreign dynamic Workers and accepted same-queue execution.

Session process configuration and optional application features are installed by
the consuming `SessionApplicationBuilder`. Production exposes no setters for
credentials, models, Resources, Environment source or projection refresh.
Invalid realization owners and zero retry budgets return a typed build error
before reading the clock or creating the runtime incarnation. Fixture-only
constructors and setters remain behind `test-support`.

`SessionCredentials::read_write` accepts one authority implementing both the
read source and material ingress, preserving the exact instance for both roles. `SessionResources::with_retention` requires the reference index, File
catalog and purge scheduler together; a catalog-only process can explicitly
leave retention disabled. CLI and scenario assembly use these capability
values. The constructor preserves the existing Store/Clock and binding-sink
identity. Tests cross invalid configuration, normalized owners, exact creation
replay, disabled retention, authority failure before root CAS and recovery.

`SessionApplicationBuilder::new` requires a complete `SessionExecutionAdmission`
value containing the dispatch feed, Worker roster and preparation compiler.
Production application construction installs these once before sharing the
application. Coordinator mounting verifies the exact selected instances before
binding Runtime wakeups or building transports; equal observations from another
instance are refused without querying any service. Individual installation
helpers are private to application unit tests. Scenario assembly can complete
its explicit fixture value, first rejecting every existing foreign binding.
The lifecycle notifier still has a separate process-composition phase; removing
that final shared setter is the next lifecycle assembly change.

The Store and Clock are selected together in `SessionPersistence::new` before
the application incarnation is created. Fixtures use the consuming builder too;
there is no second application constructor or later clock replacement path.

Session credential SPIs, process configuration and reconciliation DTOs have one
definition in `awaken-session-application-contract`. Environment projection
queries are owned by `awaken-executable-environment-contract`, while Session
Work commands and leases are owned by `awaken-worker-contract`. The application
combines these required roles at construction and keeps compatible root exports.
All Work methods require an implementation, including release operations.
Environment execution implements both roles and has no dependency on the Session
application. Its former cross-context application exception is removed. The
immutable configuration owns and validates the only logical realization-owner
string; topology decisions read the frozen Session aggregate directly.

Recorded permission attribution now uses `ScopedToolPermissionKey`: the SDK
Thread id, a distinct committed Message or pending Run source, and the call id.
The pure reducer preserves the first recorded decision for that exact source.
It rejects historical source-kind aliases before converting to the compatible
public ToolUse coordinates; it never selects a permission by audit order or
policy strength. Existing public ids remain unchanged for unambiguous history.
The protocol replaces its disposable permission index only after the whole
reduction succeeds, retaining the previous index when a refresh is corrupt.
The decision table crosses identical/different source bytes and forward/reverse
audits, alongside Thread/Run/step reuse, replay and exact-prefix constraints.

Session MCP and Resource configuration now live in the `mcp` and `resource`
modules of `awaken-session-configuration-contract`. Their former contract crates
are physically removed. The existing generation, quiescence, Resource transition
and realization-fence algorithms move with their tests; their durable encodings
and fingerprints retain the same definitions. Callers name SDK MCP targets,
Session ids and provisioning checkpoint values at their actual owners rather
than importing them through a second contract. Creation replay tests cross exact
replay, a Resource-only change, an MCP-only change and corrupt Resource identity.

Profiled release uses the existing archive CAS and terminal cleanup driver. An
unattempted Frozen preparation retains its existing admission. A Frozen WorkUnit
whose execution projection is still Running may instead request archive when its
current committed primary Run ended in error or cancellation, its original Run
activity receipt matches the current root epoch, and no other activity, delegate
or incomplete Event batch is retained. The application rereads these authorities
on each CAS attempt. Ordinary archive remains idle-only; managed and interactive
Sessions cannot borrow this admission. Missing or conflicting evidence refuses
archive. Archive fences future entry but grants neither effect closure nor
successor admission. The private release response still requires the exact
verified cleanup receipt; pending cleanup retains the original cleanup obligation
and returns no successful release. No route, persisted Session field, recovery
queue or product WorkUnit state is added.
