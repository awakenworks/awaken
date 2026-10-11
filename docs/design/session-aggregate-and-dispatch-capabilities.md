# Session operations and Dispatch roles

`PersistedSession` retains its existing serialized DTO shape. Application code
loads it into `SessionAggregate::try_from`, reads through `snapshot()` or its
immutable `Deref`, and changes it through the activity, lifecycle, cleanup,
environment, resources, MCP, configuration, assignment, budget and events
operations. There is no `DerefMut`, mutable DTO getter or arbitrary mutation
callback. Commands validate the whole candidate before replacing working state.
`compile_mutation` feeds the existing repository CAS and replay identity; the
aggregate adds no persistence store, revision source or transaction authority.

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
