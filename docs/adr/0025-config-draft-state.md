# ADR-0025: Config Draft State (Save vs Publish)

- **Status**: 📐 Proposed
- **Date**: 2026-05-02
- **Depends on**: ADR-0018, ADR-0023

## Context

Every `PUT /v1/config/:namespace/:id` request currently writes to `ConfigStore`
and then triggers `ConfigRuntimeManager::apply`, which rebuilds the live runtime
immediately. The apply path in
`crates/awaken-server/src/services/config_runtime.rs` holds no concept of a
pending or staged state: whatever is stored is also live.

This creates three practical problems:

1. **No review window.** A partial edit — changing a system prompt mid-sentence,
   or referencing a provider that does not yet exist — goes live the moment the
   PUT lands. There is no way to stage several related changes and commit them
   atomically.
2. **No rollback affordance for the UI.** The admin console's agent editor
   (`apps/admin-console/src/pages/agent-editor-page.tsx`) tracks an unsaved
   form state locally and guards navigation with `UnsavedChangesGuard`. Once
   the user saves, the runtime is already updated. Undoing requires the user to
   re-open the editor and type the old values back.
3. **Production risk.** Teams sharing a single server instance across
   engineering and production traffic have no safe way to prepare a config
   change without immediately exposing it.

## Options

### Option A — `draft` flag on every spec; explicit publish endpoint

Add a boolean `draft` field to every stored document. `PUT` writes the document
with `draft: true` by default unless `?publish=true` is passed. A new endpoint
`POST /v1/config/:namespace/:id/publish` flips the flag and triggers apply.

**Storage shape**: same `ConfigStore` namespace, single document per resource,
`draft` field participates in the stored JSON. `ConfigService::list` may filter
by `draft` status via a query parameter.

**API surface**: one new endpoint per namespace; existing `PUT` semantics change
(callers that relied on immediate apply must add `?publish=true`).

**Migration**: existing documents gain `draft: false` on first read via a
serde default. No data migration required.

**Observability**: `ConfigRuntimeManager::apply` can log and meter how many
resources are still in draft state.

**RBAC implication**: a future two-role model could restrict `publish` to
operators while allowing `draft` writes from editors.

### Option B — Separate `drafts/` namespace; promote with a copy

Draft documents live under a parallel namespace key: `drafts/agents/my-agent`
vs `agents/my-agent`. A `POST /v1/config/drafts/:namespace/:id/promote` copies
the draft document over the live namespace key and triggers apply.

**Storage shape**: doubles the key space. Both the live and draft copy exist
independently. Diffs are possible by comparing the two keys.

**API surface**: new namespace prefix and promote endpoint. Existing endpoints
are unchanged.

**Migration**: no migration; drafts namespace starts empty.

**Observability**: easy to count draft documents by listing the drafts prefix.

**RBAC implication**: same as Option A; promote action is the privilege
boundary.

### Option C — Per-resource `state` enum + atomic state machine

Each stored document carries a `state: "draft" | "published"` field managed
through explicit transitions. `PUT` creates or updates the document without
changing its state. A `PATCH /v1/config/:namespace/:id/state` with body
`{"state":"published"}` makes the transition atomic at the store level.

**Storage shape**: same as Option A with richer semantics. Future states
(`archived`, `deprecated`) fit naturally.

**API surface**: one new PATCH endpoint; apply is only triggered on transitions
into `published`.

**Migration**: existing documents default to `state: "published"` via serde.

**Observability**: state field is queryable; metrics can track documents per
state.

**RBAC implication**: state transitions are a natural permission boundary
independent of CRUD.

### Option D — Do nothing; rely on a staging environment

Keep instant-apply semantics. Teams that need a review window operate a second
server instance pointing at a separate config store, promote changes by
exporting and importing configs, and use infrastructure-level controls
(separate URLs, network policies) to separate staging from production.

**Storage shape**: unchanged.

**API surface**: unchanged.

**Migration**: none.

**Observability**: unchanged.

**RBAC implication**: none at the config level; isolation is at the
infrastructure level.

## Trade-offs

| | API compat | Storage delta | Atomic multi-resource | RBAC boundary | Complexity |
|---|---|---|---|---|---|
| A — draft flag | Breaking (PUT default changes) | Minimal | No | Publish endpoint | Low |
| B — drafts namespace | Non-breaking | 2× key space | No | Promote endpoint | Medium |
| C — state enum | Non-breaking | Minimal | Possible via batch | PATCH transition | Medium |
| D — do nothing | None | None | N/A | External | None |

## Recommendation

**Option C** (per-resource `state` enum) is recommended.

It is the only option that is non-breaking, keeps a single authoritative document
per resource, and provides a natural extension point for additional states
without a schema revision. The `PATCH /v1/config/:namespace/:id/state` verb
is unambiguous — no overloading of the `PUT` semantics — and aligns with how
`ConfigRuntimeManager::apply` works today: only the `published` state triggers
an apply, leaving the existing fast-path intact for embedders that never touch
the state field (they always see `state: "published"` via the serde default).

Option A is attractive for its simplicity but the change to `PUT` default
semantics would silently break any integration that calls `PUT` and expects the
runtime to update. Option D is acceptable for small single-tenant deployments
but leaves the problem unsolved for the growing multi-team use case.

## Consequences

**Implementers gain:**

- A review window between writing a config change and deploying it to the live
  runtime, controllable per-resource.
- A foundation for future RBAC: the state transition endpoint can be gated by
  a separate permission without touching the CRUD surface.
- An audit hook insertion point: every `published` transition is a discrete,
  loggable event (see ADR-0026).

**Implementers pay:**

- `ConfigService` must filter documents by state when calling
  `apply_locked`: only `state: "published"` documents participate in the
  runtime snapshot.
- `GET /v1/config/:namespace` must expose state in the list response and accept
  a `?state=` filter so the admin console can show drafts separately.
- The admin console agent editor must learn the distinction between a local
  unsaved form and a server-side draft, adding a second save action ("Save
  draft" vs "Publish").
