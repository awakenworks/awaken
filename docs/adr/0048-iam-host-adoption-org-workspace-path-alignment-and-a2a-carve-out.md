# ADR-0048: IAM-Host Adoption, Org/Workspace Path Alignment, and the A2A Federation Carve-Out — follow-up to ADR-0042

- Status: Accepted
- Date: 2026-07-09
- Follow-up to / partially supersedes: [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md)
  (key-based tenancy, front-door envelope, IAM alignment — this ADR fixes the
  self-hosted assembly and paths its Non-Goals left deferred, and reconciles the
  Managed-Agents org/workspace alignment plan against it)
- Relates to: [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  (management plane owns the model; runtime consumes a snapshot),
  [ADR-0030](0030-permission-policy-axis.md) (tool permission is a separate axis),
  [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (protocol is a
  projection over the neutral core), G16 (only front-door adapters name product
  vocabulary)

## Context

ADR-0042 fixed the *envelope* (key-based tenancy, one host, three data-plane
front doors over the neutral core) and explicitly deferred the *code*: the
awaken-iam wiring, the adapter implementations, and the routing gateway were
listed as Non-Goals. Since then two facts changed the ground:

1. `awaken-iam-host` now packages the whole assembly — three modes
   (`Open`/`Local`/`Remote`), one `IamGate` (authentication + authorization in
   one handle), and an `auth_layer(gate, RouteActions)` axum middleware. The
   in-repo management plane instead hand-rolls two parallel engines
   (`ManagementAuthz` in `awaken-server-local/src/authz.rs`, `EnforceEngine` in
   `awaken-authz-enforce`), each with its own token directory, route→action
   table, and mint path, gated opt-in behind `AWAKEN_MGMT_IAM=embedded` (unset =
   fully open, no guard).
2. Aligning with Claude Managed Agents' (CMA) org/workspace model surfaced a
   plan whose parts brushed against ADR-0042 in four places: dropping Project,
   seeding an Org, per-workspace subdomains, and an A2A front door. Each must be
   reconciled rather than silently diverge.

This ADR ratifies the assembly, the management-plane addressing ADR-0042 left
open, and the four reconciliations. It does **not** re-decide D2 (key-based
tenancy) or D3 (one envelope) — those stand.

## Decision

### D1: One IAM assembly — adopt `awaken-iam-host`; one PDP, many PEPs

awaken embeds IAM through `awaken-iam-host` (`embed_local` / `connect_remote`)
and enforces with `auth_layer(gate, RouteActions)`. The two hand-rolled engines
(`ManagementAuthz`, `EnforceEngine`) are **superseded**: there is one
authorization decision point (`IamGate`) and one or more enforcement points
(axum middleware), each supplying a per-surface `RouteActions` (the route→action
table) — not a second engine. This deepens ADR-0042 D4 ("full awaken-iam reuse")
from "reuse the crates" to "reuse the assembly." Remote mode is the seam by
which awaken and awaken-flow share one identity substrate (both point at one
daemon; a person is one `AccountId` across products).

### D2: Authorization is intrinsic and fail-closed — single-tenant is *seeded*, not *absent*

The `AWAKEN_MGMT_IAM` opt-in and its "unset = open" bypass are removed.
Authorization is always on. The self-hosted single-machine convenience becomes a
*seeded* deployment, not a *missing guard*: `HostConfig::local_in_memory()`
boots with zero configuration (no dir, no seal key, no env), fail-closed, and
mints an ephemeral bootstrap `admin` token returned in-process — that token is
the login credential. `HostConfig::local(dir)` is the persistent variant (writes
`iam-admin-token`, 0600). This makes fail-closed the single code path; "open"
stops being a runtime mode.

### D3: Management-plane addressing — org implicit, workspace in the path

The management/admin plane addresses resources by their tenancy anchor; the
credential supplies the default, the path specifies when the credential spans
wider:

- **Org is never a path variable.** It is resolved from the authenticated
  principal. The literal `/organizations/` segment is dropped (id-less, it
  neither carries a discriminator nor resolves a collision — `{ws}` already
  distinguishes org-tier from workspace-tier collections). This is consistent
  with ADR-0042 D6 ("self-hosted treats org as an implicit singleton / omits org
  endpoints"). A CMA-verbatim `/v1/organizations/…` admin surface, if ever
  wanted, is an **edge ACL rewrite**, not the internal shape.
- **Workspace is a path parameter** for workspace-scoped admin operations
  (`/v1/workspaces/{ws}/agents|vaults|credentials|members|api-keys`), because the
  admin principal spans the org and the workspace cannot be inferred from the
  credential. This replaces the current "workspace_id in the request body"
  pattern (`awaken-admin-config-api`). The PEP maps the path `{ws}` →
  `ScopeRef::Workspace` uniformly, so the per-handler re-authorization
  (`token_router` target-workspace re-check) is removed.

This governs the **management** plane only. The data plane is unchanged: flat
`/v1/sessions`, workspace-from-key (ADR-0042 D2).

### D4: Organization stays cloud-only — reaffirm ADR-0042 D6

The self-hosted default roots the scope chain at **Workspace**; it seeds **no**
Organization node (`embed_local`'s bootstrap admin bound at a Workspace is the
correct self-hosted shape). `ScopeRef::Org` is emitted only where a real,
cloud-inserted Org exists; org→workspace role cascade is automatic via the scope
graph's `covers`, not copied. An interim plan proposal to seed an Org node in
the single-machine deployment is **withdrawn** — it would reintroduce the
degenerate Org level ADR-0042 D6 deliberately excludes.

### D5: Project stays dormant and out of the tenancy model — Anthropic parity, no deletion required

CMA has no Project tier (their "project" is realised as a Workspace). Accordingly
Project is **confirmed out of the Org/Workspace tenancy model**: it is not a tier,
not advertised, and carries no `ScopeRef::Project` authority — exactly the dormant,
selection-only state ADR-0042 D4a and its 2026-07-05 Amendment already fix
(`/projects/{id}` changes *selection*, never *authority*; bare paths are
byte-identical to before projects existed). This ADR does **not** delete the
Project code: alignment is already achieved by dormancy, so removal is optional
future cleanup tracked separately, not a prerequisite. The interim plan's
"delete Project" slice is therefore **not** ratified as a supersession-by-deletion
of D6; the reconciled position is "frozen and unadvertised," which conflicts with
neither D6 nor the Amendment.

### D6: A session persists its resolved owner

The workspace resolved at ingress (from the key) is recorded as the session's
**owner** on the durable record (`PersistedSession` gains `workspace_id`, plus
`org_id` where a cloud Org exists), and the `Session` aggregate carries it as
identity rather than as ambient request state. Webhooks, usage attribution, and
audit project from the stored owner. This closes the gap where the durable
session carried no tenancy and is consistent with D2 (tenancy is *resolved* from
the key, then *recorded*).

### D7: Per-workspace domain is a deferred optional Host-mapping — default stays key + path

ADR-0042 D2 stands: default addressing is one host + key + globally-unique id in
path; no DNS required. A per-workspace **domain** (`{ws}.host`) is a **deferred,
optional** Host→workspace mapping over the same resolver — extending the
Amendment's already-deferred per-project domain to workspace. Its motivating case
is a per-workspace browser surface (AG-UI) needing real origin isolation
(cookies/CORS/CSP); workspace slugs are already DNS-safe. It is never the default
and never a tenancy *parameter* — it is an edge vehicle that resolves to the same
`ScopeRef::Workspace`.

### D8: A2A is cross-tenant federation, carved out of the D3 envelope

Agent-to-Agent (A2A) is **not** a data-plane vocabulary adapter in the sense of
ADR-0042 D3. D3's rule — "no front door invents its own tenancy, auth, or
routing" — holds for Managed/AI-SDK/AG-UI because they share the key→workspace
envelope. **Inbound** A2A breaks that assumption: the caller is a *foreign*
principal (another tenant/provider) that does not hold one of our
workspace-scoped keys, and it must address the **callee's** workspace + agent in
the URL (the agent-card address). This is genuinely its own tenancy, auth, and
routing, so A2A is an **explicit carve-out** requiring its own ADR; it is not
folded into the D3 envelope. Internal multi-agent orchestration is unaffected: a
coordinator's sub-sessions are **same-tenant**, linked by parent session id as
threads, and stay entirely within the D3 envelope (no A2A, no URL tenancy).

## Consequences

- One authorization engine, not two: `ManagementAuthz` and `EnforceEngine`
  collapse into `IamGate` + `auth_layer`; the route→action tables become data,
  not code.
- Fail-closed is the only mode; the single-machine story is "zero-config with a
  seeded ephemeral admin token," which is *stronger* security than today's
  opt-in-off default while being *less* configuration.
- Management URLs read `/v1/workspaces/{ws}/…`; org and the `/organizations/`
  segment disappear from the internal shape; workspace leaves request bodies.
- The org/workspace alignment plan is now ADR-consistent: D4 (org cloud-only) and
  D5 (project frozen, not deleted) remove the two supersession risks; D8 keeps
  A2A from silently violating D3.
- Sessions become attributable (owner persisted), unblocking webhooks and
  per-workspace/org usage and audit.

## Non-Goals / Deferred

- The A2A federation ADR (its foreign-principal auth, agent-card addressing, and
  cross-tenant routing) — D8 only carves out the boundary.
- The per-workspace domain implementation (D7 fixes it as deferred-optional).
- Project code removal (D5 keeps it frozen; removal, if ever done, is its own
  cleanup).
- The migration slices themselves (host adoption, session owner persistence,
  path re-routing) land as code under their own changes; this ADR fixes the
  decisions, not the diff.
- awaken-iam pin reconciliation (awaken and awaken-flow currently pin different
  revs) and promoting the scope tree into `awaken-foundation` — shared-substrate
  mechanics tracked with the cross-repo work.

## Amendment (2026-07-10): Authorization is a cross-cutting aspect — Project fully removed, core is tenancy-agnostic

Two decisions here are **superseded** after the principle was made explicit:
*authorization/tenancy is a cross-cutting aspect at the edge, and the internal
core processing logic must be tenancy-agnostic.* A change to the permission model
must touch only the aspect (ingress + guard/PEP + PDP), never core processing.

- **D5 superseded — Project is fully removed, not frozen.** Tenancy is strictly
  **Org → Workspace**. Removed across the tree: `awaken-scope`'s `Tier::Project`
  (+ `project.rs`); `config-resolver`'s `Project`/`ProjectId`/`ProjectAgentConfig`/
  `ProjectStore`; `admin-config-api`'s `/v1/config/projects` routes + the project
  schema migrations; `protocol-managed`'s `ProjectScope` + `SessionInit.project_id`;
  `authz-enforce`'s `request_scope` now always returns `ScopeRef::Workspace` (never
  `ScopeRef::Project` — the external variant stays, we just never construct it);
  and the `/projects/{id}` ingress in `server-local` + `standalone`. This
  supersedes ADR-0042 D6's "workspace + project" skeleton and its Project-addressing
  Amendment: the skeleton is now **workspace only** (org cloud-only per D4).
  A session is reachable only at the flat `/v1/sessions`; the workspace is
  resolved from the key by the edge guard.
- **D6 superseded — the core session record is tenancy-agnostic.** The S3
  `workspace_id`/`org_id` columns on `PersistedSession` (and the runtime-host
  session-store migration v2) are reverted. The core stores no tenancy. The
  owning workspace still reaches the webhook lifecycle sink, but as an **edge
  value** passed to `create_session` (`WorkspaceScope`, resolved by the guard),
  never read back from the core aggregate. A durable session→owner map, if
  needed, belongs to the aspect layer, not the core.
- **Decoupled core selection.** `runtime-host`'s per-project MCP selection
  (`with_mcp(projects)` + `init.project_id → get_project_agent`) is removed; MCP
  is resolved by agent id (workspace-default), tenancy-agnostic.

Net: the permission-model change (add Org / remove Project) is confined to the
aspect + edge; the runtime engine, projection, session processing, and MCP
resolution are untouched by tenancy.

## Amendment (2026-07-11): webhook subscriptions are a config-plane resource

The S10 webhook plane originally kept subscriptions in its own store
(`awaken-webhook`'s `SqliteWebhookRepository`, env-gated by `AWAKEN_WEBHOOK_DIR`)
with the `whsec_` signing secret stored **in plaintext** on the row. That row is
config-shaped (id-addressed, workspace-scoped, CRUD'd through the management API),
so an operator manages it exactly like an MCP-server def or inference profile —
yet it sat outside the config plane and violated its **secret-free invariant**
(ADR-0043: config rows carry secret *references*, the vault holds material).

**Decision.** A webhook subscription is an id-addressed config resource
(`WebhookEndpointDef`), stored beside MCP defs / inference profiles in the admin
store (`awaken-admin-config-api`, one more secret-free table under the `admin`
bundle), reached through the sync `WebhookStore` read-port in
`awaken-config-resolver`. Its signing key is **sealed in the vault**
(`SecretStore`, the same seam MCP/model credentials use); the row carries only a
`secret_ref`, resolved to a `RedactedString` at dispatch. The `awaken-webhook`
crate is now storage-neutral — signing, the event shape, and delivery only —
driving a `SubscriptionSource` port whose config-plane adapter lives in
`awaken-webhook-managed`.

**Consequences.**
- The CRUD surface moves to `/v1/config/webhook-subscriptions/{id}` (PUT/GET/
  LIST/DELETE), joining the id-addressed resources under the `resource_owner`
  tenant fence. PUT mints + seals the secret and returns it once; GET/LIST are
  secret-free. Handlers **self-fence** on the row's `workspace_id` (the row
  carries it, unlike MCP/profile, because dispatch enumerates by workspace), so
  tenant isolation holds even without the management-plane ownership middleware.
- The webhook plane moves from the plain `mount()` to the management path
  (`management_router_over`), where the admin store + vault exist. A deployment
  without the config plane has no durable webhooks.
- **Standalone** (open, no `admin-config-api`) wires the plane over open
  in-memory stores (`InMemoryWebhookStore` + `InMemorySecretStore`): webhooks
  work but are not durable there — consistent with standalone having no durable
  config-authoring plane. `AWAKEN_WEBHOOK_DIR` is retired.
- Vocabulary boundary held: the config crate stays webhook-agnostic (it stores a
  generic secret-free row); minting + sealing live in the webhook front door,
  mirroring how the vault front door — not `admin-config-api` — seals MCP/model
  secrets.

## References

- [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md) — the
  envelope this ADR implements and reconciles (D2 key-based tenancy, D3 one
  envelope, D4 full IAM reuse, D6 org cloud-only, the Project-addressing
  Amendment — the latter now superseded by this ADR's 2026-07-10 amendment).
- [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  — management plane owns the model; unaffected by the addressing change.
- `awaken-iam-host` (`HostMode`, `IamGate`, `embed_local`, `auth_layer`) — the
  adopted assembly (D1/D2).
