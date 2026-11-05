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
