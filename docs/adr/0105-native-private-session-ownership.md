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
