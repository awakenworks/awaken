# ADR-0105: Private Session Ownership Is an Immutable Native Root Fact

- Status: Accepted
- Date: 2026-11-05

## Context

Workspace authorization alone cannot isolate personal Sessions in a shared
application Workspace. Mutable metadata and a product-side index cannot stop
native history, Resource, file, stream or management access. An Org administrator
must not gain personal content by granting itself a generic Workspace role.

## Decision

The Coordinator Session root retains an optional immutable private binding:
Org, application, Account, Workspace and original application-binding revision.
Only a fresh complete root may acquire it. Ordinary root CAS cannot add, remove
or replace it, including through a decoded replacement. Missing bindings remain
historical unknown; metadata is never promoted to ownership.

The application ownership port distinguishes Workspace access from private
access. Workspace-only proofs refuse a private root. Private access requires an
exact binding supplied by a trusted ingress after current authentication,
membership, application binding and authorization checks. Neither an HTTP DTO
nor an Org/Workspace administrator grant supplies this proof. The Runtime core
does not interpret these product coordinates.

Creation identity includes the complete binding before the first insert. Every
external Session and linked-content surface must use the same native ownership
decision, including list filtering before pagination, replay, history, originals,
SSE, cancellation and tools. Native Work execution retains its separate exact
lease authority and must not become a general personal-content credential.

Private assistant readiness stays withheld until trusted ingress, creation,
all linked-content entrypoints, current revocation and composed two-Org tests
are qualified. Landing the root mechanism does not qualify that capability.

## Consequences

The existing Session root, idempotency receipts and SQL transaction remain the
only persistence authority; there is no personal-session database or per-person
Workspace. Generic management grants cannot rewrite private ownership. Legacy
Workspace Sessions keep their existing representation and behavior. Unknown
legacy roots in a private application cannot silently become personal Sessions.

Validation includes each ownership coordinate, malformed or unknown bindings,
metadata edits, decoded replacement attacks, same-key retries and actual SQL
reopen/CAS behavior. Linux and Windows evidence and complete product acceptance
remain distinct under the repository test-validation contract.

## Original creation receipts

Knowing a valid original creation key is not private-content authority. The
Workspace-only creation replay port must hide private roots just as ordinary
Session reads do. Private creation and recovery use the same native root and
receipt transaction with the exact immutable binding supplied by current trusted
ingress. The binding precedes the first insert and participates in request
identity even when a protocol supplies its own request fingerprint. Replays
retain the original root and binding; they never reassign an old Session to the
current application coordinates.

## Creation and recovery producer

The Session application exposes private creation and original-receipt recovery
through its existing creation role. A trusted binding enters the same complete
root compiler before the atomic insert; asynchronous acceptance and awaited
realization keep their existing lifecycle. The protocol-neutral request lowering
shares that driver rather than creating a second execution path. Unsupported
creation-role implementations return unavailable instead of falling back to
ordinary creation. A binding for another Workspace is rejected before insertion.

These producer methods do not authenticate a caller. Current trusted ingress is
still required, and no public request DTO gains an owner or private-authority
field. Private assistant readiness remains withheld until that ingress and the
linked-content and revocation checks above are qualified together.

## Workspace File ingress and deletion evidence

A Session-scoped File retains its existing immutable `scope_id`; the Files
adapter obtains the Session owner's Workspace-content verdict before metadata,
bytes or deletion, and filters that verdict before pagination. Workspace input
uploads keep their existing representation. Missing or unsupported Session
ownership cannot discharge a scoped File request. The adapter does not obtain
private authority from a File id, broad management grant or request metadata.

The existing Session deletion transaction retains a minimal `private_content`
bit on its own tombstone, derived from the committed root rather than from the
Delete command. This prevents deletion from making a private output public.
Ordinary retained outputs remain readable from the same owner partition after
their Session is deleted. One SELECT observes either the live root or its
tombstone; errors, mixed identities and corrupt roots fail closed. This adds no
File ownership database and copies no private content into deletion evidence.

The new tombstone field has no permissive default or inferred backfill. A
historical tombstone, or a still-running older writer that omits the new field,
retains unknown visibility and cannot expose scoped content. Only the current
native deletion transaction can positively record an ordinary retained output.

This boundary only serves Workspace-visible content. A private-content ingress
must still establish current private authority and use an explicitly admitted
path. Unscoped uploads in S and their attachment lifecycle are part of that
remaining qualification, so these checks do not make private assistant Ready.

## Execution reads of referenced Files

A frozen File id or a model-content reference identifies an input, not the
private owner of its source. The Resources content adapter must obtain the
Session owner's source-visibility verdict before opening a scoped File's blob.
Ordinary source visibility uses the same retained root/tombstone evidence.
Private sources require the exact immutable binding of the reader's Session.
Unknown sources and unavailable ownership remain refused.
Private readers also refuse unscoped Files and ordinary or unknown source
Sessions in S; a missing private owner must not become an implicit shared input.

The read purpose carries an optional reader Session coordinate, selected from
the Runtime's actual Session context. The Worker edge compares any supplied
coordinate with its held Run dispatch or exact materialization intent before
it can affect source visibility, then supplies that actual reader to the source
even when an older request omitted the coordinate. A wire coordinate alone never grants access.
Missing reader context can still consume ordinary Workspace-visible inputs,
but cannot consume private sources. The same Resources adapter and read-only
Session owner serve in-process and remote execution; neither gets another
File database, executor or policy interpreter. Deployment composition must
select that owner before private execution can become Ready.


## Consistent native root observation

Before minting Workspace or private Session access, the original durable owner,
requested Session id, returned root id and any immutable private Workspace must
agree. A well-formed but differently addressed root cannot substitute its owner.
Session queries and linked File observations share this live-root observation;
unknown or inconsistent observations grant no access. Current authentication,
membership and PDP admission remain separate ingress requirements.

## Request-bound Managed projection

The trusted product ingress may derive a private request view of the existing
Managed state after its current identity, membership, application-binding and
PDP checks. The view retains one exact immutable binding for that request; it
cannot mutate the authority of another request. It shares the original native
application, disposable projection cache, event channels and identity sequence.
It creates no Session store, Runtime or executor. A decoded request field never
selects this view, and constructing the view does not authenticate a caller.

The ordinary and private views select the corresponding native ownership proof,
creation driver and original receipt. Private create keys include the complete
binding so two people using the same external key in S cannot select one root.
Both durable and cached collections filter exact ownership before pagination.
Each cache candidate takes its private-binding projection from the same durable
root as its content, including cold recovery and committed-prefix rebuilds.
Ordinary Workspace views must hide a private root even after a private request
has warmed their shared cache.

This adapter seam does not supply current authentication, authorize a long-lived
stream, or qualify private Files, child delegation or product readiness. Those
remain the separate ingress and linked-content qualifications above.

## Shared private-content coordinates

The closed five-coordinate value belongs to the lower Resources contract so a
private input File can retain the same representation without depending on the
Session aggregate or introducing a second coordinate validator. The Session
binding remains a transparent wrapper: its root attachment, immutable CAS,
serialized bytes and original request identity retain their existing semantics.
Decoding either value validates shape and grants no authority.

The resource-plane fitness rule recognizes only this closed provenance value,
its shape-identical decoding fields and constructor in their one owning module.
An additional Org-bearing record, a moved copy, IAM types, credentials and policy
fields remain forbidden. These coordinates record immutable content provenance;
the Resources plane neither owns Org edges nor evaluates IAM policy.

This representation change does not enable private input uploads. Their original
File metadata, immutable writes, recovery and trusted ingress must still be
qualified; an unowned upload in S cannot be promoted into personal ownership.

## Original File input provenance

Personal input provenance is an optional closed binding on the original logical
File record, read in the same row observation as its content identity. FileStore
migration 8 adds a nullable column without a default or historical backfill. A
missing binding retains ordinary or legacy meaning; malformed or contradictory
stored provenance is an authority failure, never an ordinary File.

Repository creation validates the exact Workspace and excludes Session artifact
identity from personal input records. Original command recovery cannot change or
clear any coordinate, adopt a legacy File, or revive a deleted personal input.
Independent connections use the existing catalog transaction and command lock.
Ordinary application metadata, collections, bytes, deletion and retry paths hide
personal inputs before caller pagination or lifecycle effects. Repository reads
remain internal observations for reference accounting and recovery.

The Resources application supplies an admitted private File view over the same
catalog, byte store, lifecycle and clock. Trusted ingress must freshly check
identity, membership, application binding and native PDP authority before
selecting it. The factory authenticates nobody and is never selected by a wire
field. Creation retains the exact binding and namespaces the original external
command key by all five coordinates. The view cannot create Session artifacts,
adopt an ordinary upload, read another binding, or revive a deleted input.

Execution reads use the same original File row for provenance and immutable
content identity, then ask the selected native Session repository about the
actual admitted reader before touching bytes. Personal inputs require exact
five-coordinate equality with that reader. A claimed or frozen reference alone
does not supply ownership. Private Session outputs continue to derive their
owner from their original Session instead of duplicating it in File metadata.

Interrupted deletion resumes through the existing durable purge intent. The
internal preparation path verifies that complete original command against the
retained File before removing its logical reference; it does not grant a new
caller access to private metadata or bytes. No second receipt store is added.

These native seams do not enable a product upload endpoint. Current trusted
product ingress, S permission boundaries, stream/child authority, and compatible
node rollout remain separate qualifications before personal input writes become
available to product users.

## Ordinary hosted entrypoints require positive Workspace purpose

Hosted composition can install one request-time ordinary-Workspace observation
port in Control and Coordinator public routing. Ordinary Cloud management and
service protocol admission uses it after authenticating the credential and
resolving the trusted Workspace, before sending a PDP request. The AI SDK and
AG-UI application guards consult the same port at the already authenticated
application grant Workspace. No HTTP field supplies an observation. Private,
unknown, inactive and unsupported Workspaces cannot enter as ordinary; an
unavailable authority fails closed without reusing a prior observation.

The port reads the hosted Workspace-purpose owner. It does not authenticate the
principal, choose the Workspace, or replace the native IAM decision or Session
ownership check. Ordinary non-hosted compositions do not acquire this hosted
dependency. Public discovery and the distinct native Work capability retain
their own authority and cannot be reinterpreted as ordinary human admission.
Hosted private writes still require proven installation at every ordinary edge,
the separate current private ingress, and source and node adoption.

The native Management package provides the configured HTTP implementation of
this observation port. The deployment supplies the finite endpoint, its
audience and the existing workload credential projection. The implementation
reuses native internal HTTP transport; only the original hosted Scope owner
knows the purpose or can attest a positive result.

Startup installs the port on the target public router after path-addressed
Workspace selection. Path dispatch reconstructs request extensions, so putting
the port only on the outside router would silently leave qualified Workspace
paths unprotected. Flat and addressed requests enter the same target router
with the same configured purpose authority.

## Explicit current private Session reads

A hosted Coordinator can mount an addressed private Session read path over the
same Managed state and repository. The ingress accepts the original direct
Account bearer for the configured runtime audience. Native IAM UserInfo proves
that the Account and credential are current. The hosted purpose owner then
resolves the private binding for that Account at the addressed Workspace. The
Coordinator requires the original Hosted Runtime action decision before it
passes the five coordinates in process to the canonical Session read handler.
No bearer is passed to the handler, and responses cannot be shared by caches.

Flat requests, delegations, service credentials, generic management, and the
Worker listener cannot select this view. A disabled or changed application
binding cannot adopt old content. Session lists, history, threads, and Session
resources retain their native ownership checks, response conventions, and Org
admission. Writes, File bytes and long-lived streams require separate admission
and rollout proof before they can enter through this path.

The addressed private read surface also admits exact File metadata and byte
reads under the existing Workspace File action. The Resources application
selects personal uploads with the exact immutable File provenance. Session
outputs instead require the original Session's exact private ownership verdict.
An ordinary upload, foreign binding or unknown Session never substitutes for
either source. The private alias does not expose Workspace File listing,
mutation, or deletion. All responses use the original Files adapter and are
marked no-store by the private ingress.

Private Session and Thread event streams use the same addressed admission and
canonical projection. Each admitted stream has an absolute 30-second delivery
window, including replay; an idle stream also closes at its deadline. A
reconnect obtains fresh Account, application and PDP authority. Revocation
denies new admission, and does not recall frames already delivered within an
in-flight window. Ordinary streams retain their existing lifecycle. The alias
accepts no create, ordinary event submission, or File mutation.

The addressed `POST /v1/workspaces/{workspace}/private-assistant/sessions/{id}/stop`
operation requires the current direct Account, the current private binding,
the native `run.cancel` action, an empty body, and the original idempotency key.
The ingress creates exactly one native `user.interrupt` Event and sends it
through the original Managed Session handler and owner check. Request body and
caller-supplied data-subject attribution cannot select an Event or another
Thread. The original Session Event command owns the retained replay receipt;
the alias adds no second interrupt or execution owner.

Hosted Coordinator can select a private write profile supplied by the deployment
for the exact Org, application, private Workspace and binding revision. The
profile pins the Agent version and Environment used for a new Session. The
native private ingress accepts an original idempotency key and a closed create
body, restricted user events and personal File uploads. The current direct
Account, exact private binding and native action must still admit every request.
Session and File handlers use the admitted binding at the original command
owners, including on replay. A profile is enabled only after the deployment
qualifies the ordinary ingress and compatible runtime nodes.
