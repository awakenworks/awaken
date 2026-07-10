# ADR-0051: Tenancy is an Edge Aspect — Core Persistence Carries One Opaque `scope_id`, Resolved at Ingress, Enforced by a Scoped Repository

- Status: Proposed
- Date: 2026-07-10
- Refines / partially supersedes:
  [ADR-0048](0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md)'s
  **2026-07-10 amendment** (D6 "the core session record is tenancy-agnostic"). This
  ADR keeps that amendment's *principle* — the core **processing** logic is
  tenancy-agnostic — but supplies the isolation **mechanism** the amendment left
  unspecified, and corrects its literal reading. "The core stores no tenancy"
  becomes: **the core stores exactly one opaque `scope_id` the engine never reads**,
  not zero columns. The reverted `workspace_id`/`org_id` columns were wrong because
  they were *tier-named* (coupling the schema to the tenancy model), not because a
  column is wrong.
- Builds on:
  [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  (the runtime-unaware **secret** seam — this ADR is its twin for **tenancy**:
  resolved at the edge, absent from core processing),
  [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md)
  (key-based tenancy; workspace-from-key),
  [ADR-0034](0034-runtime-axis-model-and-orthogonality.md)
  (protocol is a projection over the neutral core; tenancy is likewise projected in)
- Relates to:
  [ADR-0048](0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md)
  D1 (`awaken-iam-host` is the single PDP; this ADR's ingress reconciliation and ACL
  translation ride it), D3 (workspace in the management path), D7 (per-workspace
  domain is an edge vehicle resolving to the same scope),
  [ADR-0050](0050-telemetry-content-capture-consent-and-gdpr-erasure.md)
  (`DataSubjectId` is a privacy axis orthogonal to `scope_id` — see D7)

## Context

A review of the tenancy integration found it half-wired and internally doubled:

- `awaken-scope` (`crates/foundation/awaken-scope/`) models `Org ⊃ Workspace` as
  first-class entities with a `ScopeRepo` port and CRUD, but has **zero consumers**
  (only the root `Cargo.toml:27` path declaration; no source imports `awaken_scope::`).
  Its promised seams — "SQL backends in the adapter layer" and "translated to
  `ScopeRef` at the authz ACL boundary" — were never built. The authorization
  coordinate it names lives in `awaken-iam` (external), so the tenancy tree is
  modelled **twice**.
- Only `credential-vault` partitions its rows by tenant (`workspace_id NOT NULL`,
  `WHERE workspace_id=?`). The config store (`awaken-config-store/src/schema.rs:17`
  — `id TEXT PRIMARY KEY`, no scope column), model catalog, MCP defs, inference
  profiles, and the `/v1/agents` registry are **global**; the management guard is
  the only fence, and the data underneath is shared.
- ADR-0048's 2026-07-10 amendment made the core **tenancy-agnostic** and reverted
  the `workspace_id`/`org_id` columns on `PersistedSession`. This is the right
  principle for *processing* but left the isolation *mechanism* unstated — as
  written, "no tenancy column" reads as "no data isolation," which cannot be the
  intent. The always-`None` `org_id` fed to `SessionLifecycleSink::emit`
  (`state.rs:911`) is the visible residue of that gap.

The organizing insight (ADR-0043's twin): **tenancy is a cross-cutting edge aspect,
exactly like secrets.** The edge resolves it; the core processes without it. What
0048's amendment missed is that *persisted data still needs to be attributable to a
tenant* — and the simplest expression of that is **one opaque column the engine
never reads**, not partitioning, not a side index, and not tier-named columns.

## Decision

### D1: Distinguish tenancy-agnostic **processing** from tenant-attributable **persistence**

The core **code** (the `RunExecutor`, the session state machine, the projection,
MCP resolution) never reads a scope, never branches on one, and takes no scope
parameter. The core **rows** carry one opaque `scope_id` used only by the
persistence layer for isolation and lifecycle. These are not in tension: a column
the engine never consults does not make processing tenant-aware.

### D2: Core persistence carries exactly one **opaque** `scope_id` column

Not the two tier-named columns 0048 reverted (`workspace_id` + `org_id` couple the
schema to the tenancy model — add a tier, add a column). Not namespace/partitioning
(an *implicit* foreign key encoded in a table name — worse than a field: fragmented
schema, per-namespace admin walks, harder migrations/joins). Not a separate
ownership index (a `scope_id` column subsumes it: `owner_of(id)` is
`SELECT scope_id WHERE id=?`; `ids_of(scope)` is `SELECT WHERE scope_id=?`).

**One opaque `scope_id TEXT` column**, stored beside the serialized aggregate:

```
session:  | session_id TEXT PK | scope_id TEXT | data BLOB(= serialized PersistedSession) |
```

Because it is **opaque**, a change to the tenancy model (add Org, re-tier) does not
touch this column — it holds whatever the edge resolved. Because the engine
**never reads** it, processing stays tenancy-agnostic (D1). The domain aggregate
(`PersistedSession`) gains **no** field — `scope_id` is a store-row concern, not an
attribute of the aggregate.

### D3: Enforced by a scope-bound repository decorator (`ScopedRepo`)

Isolation lives in one thin wrapper, injected at the edge, so no call site can
forget to stamp or filter. Two trait layers:

```rust
// Inner (infrastructure): scope-aware; only concrete stores implement it.
trait ScopedSessionStore: Send + Sync {
    async fn save_scoped(&self, scope: &ScopeId, s: PersistedSession);            // INSERT (…, scope_id, …)
    async fn get_scoped(&self, scope: &ScopeId, id: &str) -> Option<PersistedSession>; // WHERE scope_id=? AND id=?
}

// Decorator: implements the core-facing, scope-free port by binding a scope.
struct ScopedRepo<S> { inner: S, scope: ScopeId }

impl<S: ScopedSessionStore> ManagedSessionRepository for ScopedRepo<S> {   // the existing port, unchanged
    async fn save(&self, s: PersistedSession)   { self.inner.save_scoped(&self.scope, s).await; }
    async fn get(&self, id: &str) -> Option<..> { self.inner.get_scoped(&self.scope, id).await }
}
```

- The core depends on the existing scope-free port (`ManagedSessionRepository`,
  `session_repo.rs:44`) — its signatures gain no scope, so the engine cannot pass
  or read one.
- The edge constructs `ScopedRepo { inner, scope }` from the resolved `ScopeId`
  (D4) and injects it via `with_session_repo(...)`. The runtime is unchanged: it
  already holds `Arc<dyn ManagedSessionRepository>`.
- Every write auto-stamps the bound scope; every read auto-filters by it. Stamping
  is **impossible to forget** — the core method has no scope argument to omit.
- The same pattern wraps every core store the engine writes mid-run (thread
  append, `StreamCheckpoint`, resource stores): one `scope_id` column each, one
  scope-bound handle each. `credential-vault` already realises this shape inline;
  `ScopedRepo` lifts it into a reusable decorator so the other core stores need no
  bespoke scoping.

### D4: The `ScopeId` is resolved at ingress from token / path / domain, then injected

The edge obtains one authorized `ScopeId` before building the `ScopedRepo`. Multiple
ingress vehicles collapse to one id. **The token is authority; URL and domain are
selection** — an unauthenticated vehicle never *grants* scope, it only *selects*
among the principal's authorized scopes.

```rust
enum ScopeClaim { FromToken(ScopeId), FromPath(String), FromDomain(String) }
trait ScopeSource: Send + Sync { fn claim(&self, req: &RequestParts) -> Option<ScopeClaim>; }
```

Pipeline in the PEP:

1. Authenticate → principal + its authority (a narrow token *is* one scope; a broad
   admin/org principal reaches a set via the scope graph's `covers`).
2. Collect claims from the ordered sources (token, path, domain).
3. **Reconcile and authorize** to a single target scope:
   - **Narrow token:** a path/domain claim must **equal** the token's scope, else
     403 (this generalizes today's `workspace_id` query/body fence,
     `authz.rs:636-660`).
   - **Broad principal:** the path/domain **selects** a workspace; the PDP
     (ADR-0048 D1) authorizes it by ancestry — the per-handler re-check
     (`token_router`) is removed (ADR-0048 D3).
4. Stamp the resolved `ScopeId` on the request (generalize `stamp_workspace_scope`,
   `webhook-managed/src/lib.rs:33`, to `stamp_scope`) and build the `ScopedRepo`.

Per-surface configuration:

| Ingress | Sources | Target scope |
| --- | --- | --- |
| Data plane `/v1/sessions` + workspace key | token only | token (ADR-0042 D2) |
| Management `/v1/workspaces/{ws}/…` | token (authority) + path (selection) | path `{ws}`, PDP-authorized (ADR-0048 D3) |
| Per-workspace domain `{ws}.host` (AG-UI, deferred/optional) | token + Host header | subdomain slug → `ScopeId`, same authorization (ADR-0048 D7) |

When no source yields a scope (bare/self-hosted), the **seeded** default applies
(ADR-0048 D2 "seeded, not absent") — exactly one scope always resolves; there is
never "no tenant."

### D5: Core contracts gain nothing — scope rides the handle, not the contract

`RunActivation` (`awaken-runtime-contract/src/activation.rs`), `RuntimeRunContext`,
and `ExecutableAgentSnapshot` acquire **no** scope field. Tenancy reaches
persistence solely through the scope-bound `ScopedRepo` handle the edge injects, so
the neutral core contracts stay tenancy-free and the snapshot the runtime consumes
is a pre-authorized, tenancy-free artifact (the ADR-0043 seam, extended).

### D6: Retire the orphaned `awaken-scope` tree

The `Org ⊃ Workspace` tree — membership, slug uniqueness, archive, ancestry — is an
Access-context concern whose SSOT is `awaken-iam` (ADR-0048 D1). The in-repo
`awaken-scope` domain (`Entity`, `Tier`, `ScopeRepo`, `org.rs`, `workspace.rs`,
CRUD) is removed; its only durable concept is reduced to the opaque `ScopeId` value
object (a minimal serde-only foundation type). ACL translation `ScopeId →
awaken_iam_contract::ScopeRef` happens only in the PDP adapter — the seam
`awaken-scope` named but never built. Whether a thin management CRUD for
org/workspace survives (or is owned by `awaken-iam`, per ADR-0048's "promote the
scope tree to shared foundation" plan) is a management-plane decision **orthogonal
to this ADR**; the invariant is that **no core crate depends on the tree**.

### D7: Reconciliation with ADR-0050

Org-as-**controller** (GDPR) is an Access-context fact, not an in-repo entity;
0050's citation of `awaken-scope/src/org.rs` is repointed to the Access context.
`DataSubjectId` is a **privacy attribution axis orthogonal to `scope_id`** — the
subject a datum is *about*, not the tenant that *owns* it — and is unaffected. Both
may be persisted independently.

### D8: Strong physical isolation is an optional, orthogonal deployment knob

Per-tenant schema / database / encryption (for compliance postures that require
isolation at rest) is an optional physical strategy layered **over** the same
logical `scope_id` model — the edge may additionally route a tenant to its own
store namespace without changing D1–D5. It is never the default; the single opaque
column is the logical model in every deployment.

## Consequences

Positive:

- One SSOT for the tenancy tree (Access context); the dual model is gone.
- Core processing is tenancy-agnostic **and** core data is fully isolable,
  attributable, and tenant-deletable (`WHERE scope_id=?`) — the gap 0048's
  amendment left is closed.
- Tenancy-model changes touch only the edge and one opaque column, never core
  processing, contracts, or (child-row) schema.
- Stamping is unforgettable (the core port has no scope argument); the closest
  precedent (`credential-vault`) is generalized rather than re-implemented per store.
- Deletes an orphan crate with unbuilt seams.

Costs (accepted):

- Every core store that needs isolation gains a `scope_id` column and a
  `ScopedRepo` wrapper at its edge construction site.
- Background/cron access to a scoped aggregate must construct a repo bound to the
  correct scope (or an explicit admin/unscoped variant); a repo bound to tenant A
  cannot read tenant B's rows — which is the isolation working.

## Alternatives considered

- **Namespace/partition per tenant as the logical model.** Rejected: it is an
  implicit foreign key in a table name — more complex than a column (fragmented
  schema, per-namespace admin walks) for no logical gain. Demoted to D8 (optional
  physical isolation).
- **A separate aspect-layer `session → owner` index.** Rejected: subsumed by the
  `scope_id` column (D2).
- **Zero columns (0048's literal amendment).** Rejected: leaves persisted data
  unattributable — no isolation, lifecycle, or metering by tenant.
- **Tier-named `workspace_id` + `org_id` columns.** Rejected: couples the schema to
  the tenancy model; a single opaque `scope_id` is tier-agnostic.
- **The core holds a `ScopeId` in its contracts/aggregate.** Rejected: leaks
  tenancy into processing; scope belongs on the repository handle, not the engine.

## Migration (slices, each independently green, no stubs)

- **S1** — `awaken-tenancy` (`ScopeId` value object); PEP resolves it via the
  `ScopeSource` chain (generalize `RequestTenancy`); `ScopeId → ScopeRef` ACL in
  the PDP adapter.
- **S2** — `ScopedRepo` decorator + `scope_id` column on the core stores (session,
  thread, checkpoint, resource); the two-trait layering; edge injection. Self-hosted
  resolves the seeded scope, so behavior is unchanged.
- **S3** — resolve owner for webhooks/usage/audit from `scope_id`; delete the
  always-`None` `org_id` path (`state.rs:911`).
- **S4** — aspect resource stores (config/catalog/mcp/registry) gain `scope_id`
  and `ScopedRepo`, mirroring `credential-vault`; the management guard is no longer
  the only fence.
- **S5** — management addressing to `/v1/workspaces/{ws}/…` with PDP authorization
  (ADR-0048 D3); remove the body-`workspace_id` + per-handler re-check; retire
  `awaken-scope` (D6).

## Resolved: the model catalog is org/deployment-shared, not workspace-scoped

**Decision (settled):** the model catalog (providers / endpoints / offerings) is
**org/deployment-level shared configuration**, NOT a per-workspace resource. Every
workspace in a deployment reads one catalog; the per-resource ownership guards
(agents registry, MCP defs, inference profiles) that fence by the edge scope
deliberately do **not** cover the catalog routes. Isolation across orgs is by the
**deployment boundary** — org is cloud-only (ADR-0048 D4), so a self-hosted
deployment serves one org and its catalog is correctly shared within it; a distinct
org is a distinct deployment with its own store. Implementing in-process org
partitioning of the catalog would be implementing the deferred Org tier and is out
of scope here. A `tenancy_isolation_matrix` test asserts the shared behavior (a
provider authored via one workspace's path is readable via another's).

## Future option: org-level shared reads within a shared tier

If a single deployment ever serves multiple orgs (the deferred cloud Org tier),
org-shared reads authorize against the request's `readable` set (scope + ancestors,
resolved once at ingress in D4) instead of exact equality — still one opaque column,
filtered by `scope_id IN (readable)`. Until the Org tier lands, exact equality (with
the catalog explicitly shared per the decision above) suffices.
