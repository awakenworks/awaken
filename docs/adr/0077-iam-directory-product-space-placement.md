# ADR-0077: IAM Directory Owns Agents Product-Space Placement

- Status: Accepted
- Date: 2026-08-27
- Implements: awaken-iam ADR-0013
- Preserves: ADR-0048 IAM host adoption, ADR-0063 resource/authz isolation

## Context

Agents already persists one opaque Workspace owner beside product state and uses
IAM for identity and authorization. Adding an Agents-owned Org/Workspace tree,
directory repository, transport, or migration would duplicate IAM. Conversely,
using a movable directory node as the Workspace identity would couple runtime
and resource ownership to presentation hierarchy.

## Decision

The stable Agents Workspace id remains the product and persistence coordinate.
IAM stores its independent placement as
`ProductSpaceRef { product_id: "agents", space_id: "workspace/<opaque-id>" }`.
Local embedded boot reconciles the hidden Org and calls IAM's canonical
`EnsureProductSpacePlacement` through the existing `DirectoryApi` and shared
SQLite store. IAM alone derives the node id, canonical slug, timestamp, active
state, and audit actor. Organization bootstrap still uses IAM authorization
administration; Directory placement does not. Hosted provisioning performs the
same command through Cloud's remote or embedded IAM administration path.

Agents adds no directory model, repository, database tables, client, server, or
hierarchy-aware runtime API. Moving the IAM node therefore changes neither
`ScopeId`, `WorkspaceId`, authorization bindings, Session ownership, nor resource
rows.

## Static structure

`awaken-control` is the local composition adapter. It maps the existing stable
Workspace id to IAM contract values and invokes IAM `DirectoryApi`.
`awaken-tenancy`, runtime, config, resource, and store crates remain unchanged
and IAM-free except at their existing authorization edge.

## Dynamic behavior

On first local boot, the adapter migrates IAM, idempotently creates the hidden
Org, then ensures one Directory placement. On restart, IAM returns the existing
placement without advancing Directory revision or overwriting later user moves
and metadata. A placement found in another Org fails boot. Directory mutation
failures fail closed; no product-side placement write is attempted as a fallback.

## Consequences

- Local and hosted modes use the same IAM Directory contract while choosing
  embedded or remote composition.
- Agents product state remains stable across arbitrary directory moves.
- Awaken Objects is not introduced and no future product hierarchy is encoded
  into the Agents runtime.
