# ADR-0078: Terminal Recovery Retains the Existing Session Authority

- Status: Accepted
- Date: 2026-09-07
- Amends: [ADR-0075](0075-unified-managed-session-worker-execution.md)
- Implementation: complete Resource projection and terminal-only release implemented; bounded same-process Kubernetes handle-free convergence implemented

## Context

Review against upstream main `982cc1b34` found that terminal assignment still
selected only active Resources. A Worker can already have staged desired B
before its activation receipt reaches the root. Sending active A -> A then
fails the Host's correct non-regression check against A -> B. Retrying the
same malformed projection cannot repair it.

The current root already retains active, pending, and Repository retirement
inputs. The current Host already serializes complete projection installation,
and recovery already pages past invalid terminal candidates and bounds control
calls. Adding another cleanup manifest, queue, recovery owner, or generation
counter would duplicate these mechanisms.

Container terminal preparation also rejects an absent durable handle. This is
an availability gap, but the fail-closed response is necessary until a provider
can prove more than a point-in-time absence. A Create request can be admitted,
lose its response, and arrive at Kubernetes after an absence query. Coordinator
lease checks alone cannot fence an already-sent physical request.

## Decision

### 1. Reuse the root's complete transition

The sole Session terminal assignment adapter uses the existing frozen desired
projection, which includes the previous active manifest. Warm polling and cold
claims share that adapter. The Host's existing Terminal mode stages this
projection without ordinary activation or Resource effects. Its revision and
Workspace checks are not relaxed.

Pending intent is not discarded at termination. Only the existing verified
cleanup receipt permits `complete_terminal_release`. Existing Repository
retirement plans retain their separate canonical inputs; active and pending
bindings are not flattened into an invalid single Resource manifest.

This first slice changes no aggregate, schema, public wire type, or API. Its
owners are `awaken-session-application` and `awaken-session-executor`; the Resource
state and cleanup contracts retain their current ownership. G55 and the
production adapter/Host tests enforce it.

### 2. Require physical closure before claiming handle-free absence

The physical slice remains inside the existing
`ContainerEnvironmentProvider::prepare_terminal_environment_for_effect` and
Session cleanup protocol. No generic effect service or product recovery API is
introduced. The generic runtime default and every unsupported topology stay
fail closed.

The provider must distinguish:

| Evidence | Allowed result |
|---|---|
| Exact reconstructible realization, matching namespace, scope, frozen provider configuration, original effect, Pod UID and retained PVC UID | Reconstruct terminal participants, then use existing prepare/dispose commands |
| Exact original attempt closed, every possible participant absent, and no admitted delayed Create can materialize | Verified absence |
| Missing handle, a point-in-time 404, timeout, ambiguous transport, foreign identity, missing participant evidence, or unsupported backend | Retryable uncertainty; no completion receipt |

Stable names and labels discover candidates; they never authorize deletion.
Memory/Secret writeback and pending Repository effects must be accounted for,
not silently skipped. No cleanup may delete the remote GitHub repository or
application/database PVCs.

Implementation requires an exact attempt-closure witness at the physical effect
owner: either a completed join of every admitted attempt or a durable primitive
that rejects late creates at their own linearization point. Merely checking a
ConfigMap/lease before a separate Pod POST leaves the same race. UID-fenced
deletion solves replacement safety, not late-create absence. Kubernetes,
Docker, and Podman adapters must name and test the primitive independently.
Until then unsupported cases remain visibly pending, not falsely converged.

The 2026-09-11 bounded implementation supplies the first of those witnesses for
same-process Kubernetes creation only. Before its first backend effect, the
Kubernetes adapter records the exact `(scope, operation, owner, runtime
incarnation, epoch)` attempt and enters a shared read-side gate around both Pod
reservation and activation. Terminal preparation may close only an already
observed exact gate, waits for every reader to leave, and leaves a process-life
tombstone that rejects late re-entry. While holding the close side it admits
only an ephemeral plan with no package, bind, Memory, or continuation-PVC
participant, then linearly observes both canonical Pod and PVC absence. A new
Worker has no gate witness and therefore remains pending. This is a join plus
absence proof, not the rejected point-in-time 404 rule.

### 3. Join an already-terminal Session through its existing cleanup owner

Ordinary user archive retains IdleOnly admission. The internal
`SessionApplication::release_terminal_session` uses a distinct TerminalOnly
admission in the existing archive CAS kernel and requires the existing verified
cleanup receipt before returning success. It admits only durable
`ActivationFailed` or `Terminated` execution, preserves the failure state, and
rejects every live execution class, including Idle. It exposes no public route,
new durable operation, epoch, or Run-to-Session lifecycle authority.

This narrows and replaces the earlier draft's proposed exact-Run release for
this failure-recovery slice. A Session's terminal execution is absorbing under
the existing state machine; an old Run therefore cannot use this command to
terminate a newer active Session. A Run that ended while its Session remains
live is deliberately insufficient. A future command that terminalizes live
Sessions would need exact activity admission and is not part of this repair.
The existing public archive and Repository publication paths are unchanged.

Run outcome, Session cleanup, and downstream Issue acceptance remain distinct
facts. A read-only UI can show committed failure plus cleanup pending without
creating a second durable status. Moving business completion before cleanup is
a separate policy change and is not part of this repair.

## Verification and release sequence

1. Reproduce active-only assignment on the latest main before fixing it. Cover
   empty/nonempty active state crossed with absent/unattempted/attempted pending
   state after an actual SQLite read. Assert exact previous/desired payload and
   zero root mutation. Check warm/cold Host installation and rejection of the
   old projection, without live Resource materialization.
2. Extend `SessionResourceActivation.tla` to retain pending state until receipt
   and check complete terminal projection and conditional convergence. Restore
   the old adapter projection and premature receipt as negative controls; each
   must fail its named invariant, not just produce a tool error.
3. `SessionUnmaterializedCleanup.tla` records the physical proof boundary. Its
   current config proves that unsupported cleanup remains pending. Its Target
   config checks convergence only assuming exact physical join and fair
   external progress. Removing the join must expose the late-Create race. The
   Kubernetes adapter's executable gate table covers the implemented
   same-process correspondence; durable cross-process closure remains outside
   that correspondence and fails closed.
4. Before implementing physical convergence, exercise actual adapter faults at
   intent commit, pre-create, PVC-only, Pod-created, response-loss, preparation
   receipt, disposal, and final root CAS. Resume an old Worker at every gap;
   race a newer incarnation and a same-name foreign UID. Run the same command
   conformance locally/remotely and across supported root stores. Cancellation
   of an HTTP future is not an attempt-closure witness.
5. Cover all seven execution states at terminal-only admission, unchanged public
   IdleOnly rejection, preserved failure, verified cleanup, response-loss replay,
   accepted Event fences, and unavailable cleanup followed by retry. Reuse the
   existing `terminal_execution_never_reopens` Kani harness and Session Root
   proof; they prove terminal absorption, not adapter cleanup completion. Keep
   a poisoned head, a timeout, and more than one page
   of bad candidates in recovery tests; reuse existing pagination/deadline
   owners rather than adding another scheduler.
6. Publish reviewed upstream fixes, update the consuming exact Git revision,
   then validate the local deployment. A full upstream-main upgrade is separate
   from this schema-free projection patch: consumers must adapt the new
   optional Environment sandbox-policy input and obey G47's old-writer cutover
   rules for changed Session payloads. Do not point shared manifests at local
   paths or unpublished commits, or patch Cargo caches.
7. Product acceptance still requires the two independent UI-created projects
   to produce real Issue -> Coding -> GitHub PR/commit evidence, accepted root
   output, clean-checkout offline tests, and their browser oracle. Mock tests,
   a terminal Session, or a model-checking pass cannot replace those results.

## Rejected alternatives

- Treating no handle/404 as success loses delayed or partially created effects.
- Rolling the Host revision backward hides missing pending cleanup obligations.
- Duplicating a cleanup aggregate, queue, or Resource collection creates a
  competing authority alongside the existing Session protocol.
- Public force archive permits a stale Run to terminate a newer activity.
- A new Run/activity release protocol is unnecessary for an already-terminal
  Session; it expands authority and compatibility work beyond this repair.
- Fixed startup sleeps assume network timing; downstream workers should wait
  for actual control and advertised MCP transport availability, with shutdown
  cancellation and Worker-independent server liveness.

## Consequences

The first repair is small and independently testable. Same-process,
zero-participant Kubernetes attempts can now converge without a durable handle;
cross-process and rich-topology physical availability gaps remain explicit
rather than being hidden behind a successful receipt. Formal evidence states
the bounds and assumptions; production physical conformance and downstream
end-to-end acceptance are still required.

## Amendment 2026-09-25: profiled release joins terminal cleanup

The existing Workspace-scoped profiled Session release route is used by Flow to
close a failed WorkUnit before it may retry. Its no-publication branch previously
called the public idle-only archive, so an `ActivationFailed` or `Terminated`
Session with unverified cleanup retried forever with “only an idle Session may
be archived.” This was a transport-adapter mismatch, not permission to force
terminate a live Session.

That same profiled route now reads the durable Session root only to select the
existing application command. An already-terminal root joins
`release_terminal_session` and requires its verified cleanup receipt. An idle
root still passes idle-only archive admission and then joins the same terminal
cleanup; every other live state remains rejected. The application rechecks the
root under its canonical CAS, and the route retains its existing exact
Workspace ownership authorization. The ordinary public archive remains
idle-only. No new route, cleanup queue, status authority, credential, or Run
permission is introduced; this is the existing product release contract
reusing the terminal-only Session owner.
