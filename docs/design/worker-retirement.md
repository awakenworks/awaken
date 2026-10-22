# Worker Retirement With Retained Session Sources

This bounded qualification expands [ADR-0078](../adr/0078-terminal-recovery-retains-the-existing-session-authority.md).
It delegates the existing role catalog to the
[remote Worker component catalog](remote-worker-protocol.md#10-remote-worker-component-catalog);
it adds no service, queue, durable cleanup state or permission authority.

## Amendment 2026-10-22: observe the original source independently of execution configuration

The original published handle and Create fence identify the retained source.
The Coordinator's current Hand residency, installation namespace and volume
configuration describe a possible new execution, not that original source.
Comparing the current adoption-plan fingerprint before a retirement read can
therefore refuse a guarded original Pod/PVC which the canonical runtime observer
positively identifies as retained.

Reuse the ContainerProvider observation entry point and the runtime's immutable
incarnation/fingerprint/fence classifier. WorkerRetirement requires the original
effect fence, complete durable fingerprints and matching Session scope, then
passes those original coordinates to the same runtime observer without requiring
today's execution configuration to reproduce the original adoption fingerprint.
Existing legacy-handle normalization still validates its derivation. Recovery
and TerminalCleanup retain their existing adoption-plan validation; retirement
does not authorize either operation. Missing/foreign identities, failed retention
premises and backend errors keep their existing non-destructive outcomes.

The sequence remains complete Session-owner scan -> settled original Dispatch
scope -> exact retained-source observation -> existing Worker-domain decision ->
independently observed Actor exit. No Session or Run transition, source
preparation, cleanup receipt or execution capability is supplied by this read.

## Amendment 2026-10-22: install observation in the registered-Worker composition

The split Coordinator still owns no local claim pool or Hand. Its complete
Session-owner retirement read nevertheless needs the same physical source
observer the selected container tier supplies. Leaving the construction-time
uninstalled provider in that path makes `effective_spec` fail before any
Kubernetes observation, even when the original source and its guard are intact.
Tests that explicitly install an execution provider do not qualify this
process assembly.

Reuse the existing Deployment, container backend builder, effective-spec
projection, observation roles and immutable-incarnation classifier. The
Coordinator installs a read-only adapter into the existing realizer/adoption
role handles: metadata and retirement observation delegate to those same
owners, while creation, adoption, rebind and closed-evidence validation refuse
before backend access. Terminal, capacity and installation roles retain their
uninstalled behavior. This does not mark an execution provider installed.
There is no new provider protocol, observer service, state or receipt.

The canonical container factory distinguishes execution from observation
assembly. Observation preserves the same backend and layout configuration,
but never performs execution probes, runs package building, leases/prepares warm
capacity, constructs a Hand, or installs cache-volume mutation services.
Execution startup keeps all its existing evidence requirements unchanged.
Kubernetes construction retains the existing namespace NetworkPolicy read;
the composing infrastructure must grant that read without policy mutation.

The dynamic order is registered-Worker placement -> read-only tier assembly ->
existing Session projection -> effective spec -> exact provider observation ->
the existing retirement predicate. Errors remain errors; the read changes no
Session, Worker, Pod or PVC. Actual exit, original Create closure, same-root CAS
and cleanup still require their existing independent authorities.

Regression comments own the decision table. They must exercise the actual
Coordinator assembly from an uninstalled provider, rather than preinstalling a
test provider; prove read availability with no local execution capability;
refuse every mutation before backend I/O; and retain the existing exact-source,
foreign/missing participant and provider-unavailability cases. Source checks,
live observer qualification and original-mission acceptance remain separate.

## Amendment 2026-10-21: retain an active Resident Session across Actor replacement

Worker retirement observes Actor lifetime separately from Session and Run
completion. The existing exact Registry incarnation, closed claim admission,
zero physical/Work domains and complete Session-owner scan remain mandatory.
An active Idle, Running or Rescheduling Session may retain its same root when
its baseline is frozen, its Environment is Resident, its Event batches are
complete and the existing provider positively observes independent original
source retention. The read uses unprepared-source semantics; private Memory
buffers, unknown or missing original participants and unavailable providers do
not qualify. Preparing, Activating and nonresident roots keep their refusal.

Reuse the existing source-retention port, frozen projection, provider selection
and Worker-domain response. The terminal path keeps its own frozen action,
settled-source and preparation requirements. No new durable state, queue,
cleanup driver, scope owner or receipt issuer is introduced. Occupancy remains
true and the read leaves the Session, Run, activity epochs, lease, bindings and
all prior uncertain effects unchanged.

The existing release owner then stops the qualified Actor and independently
observes its exact exit and original Create closure. Only the existing root CAS
and lease-succession protocol may assign the same root to its successor.
Retirement supplies no Run result, process-domain closure or cleanup receipt.
The bounded retirement model includes a live-Session case in which source data
survives and the root transfers while every terminal preparation/disposal/ACK
transition remains disabled. Provider/API witnesses and deployed E2E remain
separate qualification requirements.

For Kubernetes, an unprepared `WorkerRetirement` observation accepts an exact
Running original Pod as well as a terminal original Pod when the original guard
is held, isolation and cleanup authorization are absent, and the exact live PVC
and Pod are independent of Worker garbage collection. The Running Pod remains
a retained data source; this observation grants neither execution nor disposal.
Pending, unknown or missing phases remain unqualified. Terminal cleanup keeps
its terminal-only guard predicate and its own authorization requirements.

The provider reports this fact as `SandboxObservation::RetainedOriginal`, never
by changing an observed Running phase to Terminal. The existing observation
entry point and immutable-incarnation classifier remain authoritative. Its
identity validation is shared with the ordinary physical-phase projection;
retention adds only the provider-owned guard, exact claim and GC-independence
premises. The result is transient, names the original physical incarnation and
grants no adoption, process closure, source preparation or disposal authority.
Ordinary adoption and terminal recovery reject it. An unprepared retirement
read requires this explicit retention fact; Ready or Terminal alone is
insufficient. An already-prepared root keeps its existing disposal replay.

Known retention-premise failures produce a non-destructive Incompatible
observation and therefore `retirement_permitted=false`. Provider I/O failures
remain errors. The Coordinator preserves the existing RunError kind/code at
its authenticated boundary with a bounded, secret-free stage/identity
diagnostic; an unavailable dependency must not become absence or permission.
`WorkerDomainReadFailure` is the transport owner's exported error DTO. Release
composition tests consume that pinned type; consumers may surface its safe
fields without inventing an error schema or interpreting operator messages.
No new durable state, recovery service, provider port or release bypass is added.

## Amendment 2026-10-16: separate source preparation, physical cleanup and Actor retirement

**Implementation status: Actor retirement remains a target.** Source partitions
are qualified separately in ADR-0078; generation-admission qualification below
does not prove retirement, deployment or original-mission completion.

The original local CRM and autonomous-development roots exposed the next two
provider partitions after logical scope freeze and exact predecessor-exit
reassignment succeeded. A retained V2 Pod/PVC handle can lack a projected-object
handoff. A failed original Pod can already have a deletion timestamp while its
exact continuation PVC remains live. Both stopped before source preparation
in the original failure. The replacement barrier retains the current Worker when
its Session realization domain is still occupied. Repeated expiry or a sampled
404 must not convert any of these facts into cleanup completion.

The objective remains one original Session root and one cleanup driver. Source
readiness, physical cleanup and the lifetime of its process-local Actor are
separate questions with separate evidence; none substitutes for the others.

| Scope | Authoritative existing owner and reuse | Required change |
| --- | --- | --- |
| Logical creation and execution | Runtime committed heads, sealed creation scope and `SessionSourceDispatchObservation`; retain their current meaning | Reuse unchanged; a settled source read is not a physical disposal receipt |
| Source and physical participant identity | Sandbox provider, frozen specification, retained handle, original Create ledger and K8s UID checks | Distinguish terminal source preparation from destructive cleanup; normalize a retained legacy handle only from sufficient provider-owned evidence |
| Actor authority and retained source obligation | Session root realization lease, terminal cleanup operation and Worker Registry incarnation | Fence new Actor grants on drain and distinguish an inactive Actor from a source obligation which remains retained |
| Replacement and publication | Existing Native Worker-domain observation and Cloud release/restore barrier | Consume an explicit Native retirement decision, observe real process exit, then permit exact successor admission; preserve the current source objects and root |

New code is limited to transient role-specific evidence where the existing
observation types cannot express the distinction, plus their regression/model
cases. It introduces no additional persisted cleanup flag, queue, scheduler,
provider journal, receipt issuer or replacement controller. Existing provider
builders, participant verifiers, lease succession, UID deletion and root CAS
remain authoritative.

A terminal source-preparation observation may admit an exact failed/deleting Pod
and its still-live exact PVC for bounded source access. It does not report that
the Pod is absent, release a claim, or certify participant disposal. Ordinary
execution and checkpoint admission retain their own rules. Destructive cleanup
still requires original participant identities, complete preparations and the
existing current-authority/finalizer/UID protocol.

A retained V2 handle supplies original Pod/PVC identity but does not itself
supply an original projected-object result set. A known-empty API projection
must be established from the exact retained handle and verified frozen provider
plan, not from missing RESULTS or an empty current LIST. A nonempty, partial,
foreign or otherwise unproved membership keeps its existing Unknown result.
Retained control/Memory/Secret evidence must be preserved rather than replaced
by the handle-free narrow joined-empty witness.

Ordinary drain must still let an already-owned cleanup reach its durable
boundary. A Pool-wide switch which stops all cold recovery is not an Actor
retirement protocol: Control must distinguish fresh takeover from continuation
of the original Actor. A retiring Actor cannot acquire another terminal root;
its exact already-owned root may continue across lease expiry.

Run grace and Session cleanup lifetime are distinct. Do not truncate every
cleanup renewal to a run-drain timer: a long preparation may still need its
original Actor. Continuing an already-owned root, including an expired exact-
Actor generation retry, retains the normal Registry-bounded lease protocol.
Safe forced stop is established by source durability, not by waiting for that
Actor to stop renewing. The actual process exit and original Create closure
fence its remaining authority before a successor acquires the same root.

The Native Worker-domain read may qualify a draining Actor separately
from its still-retained cleanup obligations. Reuse the existing complete Session
owner scan, frozen projection builder, source-dispatch observation, terminal
runtime role and selected Environment provider. The Session owner lowers its
canonical action to the existing prepared-source observation fact; the Runtime
role does not re-export or reinterpret that aggregate action. `Prepare` still requires positive
provider evidence that the original data survives this process's stop; `Dispose`
uses the already-durable preparations and the same original participant replay.
`Waiting`, incomplete scans, unsealed sources and unsupported
providers remain unqualified. The read installs no Runtime projection and issues
no cleanup or process-exit receipt. The existing release owner consumes this
Native decision, then scales the exact held Worker down; the existing independent
exit/Create closure and same-root CAS still precede replacement cleanup.

New code is limited to this read qualification, its typed observation purpose
and the optional positive Native domain projection. Reuse and factor the current
provider selection and Sandbox spec projection rather than construct a second
layout or run a second cleanup driver. Kubernetes source retention requires its
original guard and exact Pod/PVC identities outside Worker-owned garbage
collection; unavailable bytes or identities are not repaired by this read.

The existing Worker-domain transport module exports its query, response and
route path for consuming composition tests. A consumer must serialize these
exact pinned types into its existing release predicate; an invented fixture
field cannot establish producer support. Ordinary reads still omit the optional
qualification. These read-only types create no retirement or cleanup authority.

Actor retirement must not reuse `cleanup_completed` as its permission bit.
Retirement with retained sources requires exact draining Registry identity,
closed new business-work admission, no physical Run or Work claim, and a
positive provider observation that the
original source survives this Actor's stop. An unavailable retention observation
or an unqualified/unsealed Session blocks that path. The real exit and original
Create closure are still required before another Actor acquires the root;
retirement never acknowledges the old Source, Runtime slot, Session cleanup or
Flow inbox.

The dynamic order is: close admission -> fence new Actor grants -> establish
source retention -> stop the old Actor through its existing bounded shutdown -> independently
observe its exact process exit and close original Create issuance -> claim the
same Session through its existing CAS -> prepare the original source -> perform
only authorized UID-fenced disposal -> commit the original cleanup receipt ->
let Flow close its exact retained link and make its Work-owned recovery decision.

A forward code correction while a source is retained must use the existing
release owner and command-start Pod/template identities. It may not replace the
held Worker through compensation, reset rollback evidence, publish a different
business identity, or bypass an Unknown observation. Tests must cover the source
and Actor partitions together with the current physical, WorkQueue and Session
domain checks before this target can be called implemented.

## Source migration 2026-10-20: retain current component ownership

Sandbox observation purpose stays in `awaken-provisioning-contract`; the retired
Session and Container spellings are not restored. `RunAssembly` supplies the
required terminal read through its existing realization executor. Environment
holding owns provider/publication selection and the original physical read;
the existing pure Sandbox projection supplies the same complete frozen layout
used by installed adoption. Root CAS, Registry, preparation, UID disposal and
cleanup receipts retain their existing owners. No Host facade, duplicate
provider port, compatibility alias or additional cleanup driver is introduced.

## Amendment 2026-10-17: preserve owned cleanup across drain

The realization contract `SessionRealizationTarget` distinguishes continuation
of an already-owned terminal root from acquisition of a new obligation. Ready
may acquire; Draining continues only its exact owner/incarnation, including an
expired-generation retry. Full Run capacity and run-grace expiry do not revoke
that cleanup responsibility. At uncertain CAS readback, the existing conflict
record's predecessor establishes original ownership; the just-written claim
cannot manufacture affinity. Only an unowned/new-root claim is compensated
when drain wins that race, preserving concurrent successors.

This corrects the earlier over-restrictive live-generation-only proposal and
withdraws the unshipped universal drain-deadline cutoff. One canonical Registry
read, root CAS and compensation path remain. Retirement is independently
qualified from retained/prepared source data, then actual exit/Create closure
and existing root lease succession permit recovery. It never acknowledges the
old Session cleanup or Flow inbox.
