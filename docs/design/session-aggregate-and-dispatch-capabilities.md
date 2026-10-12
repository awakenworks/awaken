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
| Reservation writer | `SessionRunReservations` |
| Reservation fence holder | `SessionReservationAuthority` |
| Local scheduling service | Execution-owned `ControlDispatch` bundle |
| Authenticated Worker HTTP server | `WorkerDispatchAuthority` |

The remaining leaf roles cover claims, execution, claim ownership, commit
authority, settlement, ordinary reads, maintenance, attempt control, control
claims and control reads. `InboxRead` is separate from Inbox writes. Role
bundles live with their consumers; the existing contract crate owns the leaves.

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

`DispatchWorker::new` and `new_bound` supply the combined local recovery source.
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

The four write-once lifecycle, Worker observation, dispatch observation and
preparation-compiler bindings remain a separate process-composition phase.
Their removal from the shared application mutation surface is still pending.

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
