# ADR-0042: Public API — Key-Based Tenancy, Path Routing, IAM Alignment, and Front-Door Consistency

- Status: Accepted
- Date: 2026-07-03
- Relates to: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (managed
  protocol is a front-door axis over the neutral core),
  [ADR-0037](0037-managed-capability-advertisement-wire-alignment.md) (align the
  session agent to the official wire; never guess a shape),
  [ADR-0030](0030-permission-policy-axis.md) (permission policy as a runtime
  axis), G16 (only front-door adapters name product vocabulary)

## Context

As awaken evolves toward awaken-next it becomes multi-tenant and grows **three
public data-plane front doors** over the same neutral runtime: the Managed
Agents wire (ADR-0037), an AI-SDK / OpenAI-Anthropic-compatible surface, and
AG-UI (agent↔frontend event protocol). Two questions have been answered ad hoc
and need one coherent decision: **how a request is addressed, routed, tenanted,
and authorized**, and **how the three front doors avoid becoming three
different-feeling products**.

Claude Managed Agents (CMA) is the reference: control plane vs data plane, a
persistent `agent` id + an ephemeral `session` id, **key-based tenancy** (the
API key resolves the workspace; the URL carries no org/project), a single API
host with globally-unique ids in the path, and a per-tool `always_ask` gate.
`awaken-iam` is already built Anthropic-compatible (`anthropic_admin.rs`,
`awaken-iam-preset`), so it is not a peer to reconcile — it is the intended
implementation, and a superset, of CMA's permission model. This ADR fixes the
shared envelope so the three front doors differ only where they must.

awaken-iam is also shared across products, and the default (self-hosted /
embedded) deployment has a single organization — so the always-present tenancy
boundary is the **workspace**, not an org. The skeleton is therefore
**workspace + project**, with organization an optional cloud-only wrapper, not
CMA's always-present org (D6).

## Decision

### D1: Control plane REST + data plane events; two ids, not one

Config is a **versioned REST resource** (`agent_…`, create-once/reference-many);
a run is an **ephemeral stateful instance** (`session_…`) driven by an event
stream. The management identity and the data-plane routing handle are **issued
separately** so the endpoint can be revoked/rotated without touching the agent's
identity, RBAC, or history (matches CMA; ADR-0034/0037).

### D2: Key-based tenancy, single domain, path addressing — no org/project in the URL, no per-instance subdomain

Two orthogonal routing dimensions:

- **Tenant + authorization** ← the **API key** (workspace-scoped). The URL is
  tenant-agnostic.
- **Resource / instance** ← a **globally-unique id in the path**
  (`/v1/sessions/{id}`); the gateway routes to the instance by that id and does
  stateful pinning internally.

No `orgs/{org}/projects/{project}` hierarchy in the URL, and no
`{id}.agents.example.com` subdomain. This is the CMA / OpenAI / Stripe model.
Rationale: trivial AI-SDK ergonomics (one `baseURL` + one key), one certificate,
works on local/self-host **without DNS**, and sharing a URL never grants access
(the key gates it). The cost — one key binds one workspace, so human multi-org
switching lives in a console front-end (its own URL param → mints the right
token); the backend API stays key-based.

### D3: One envelope, three vocabularies — front-door consistency

Managed Agents, AI-SDK-compatible, and AG-UI are all **data-plane front-door
adapters over the same neutral core** (ADR-0034 axis; each an ACL under G16).
They **MUST share one envelope**:

- transport: HTTP + SSE;
- auth: bearer API key;
- tenancy: key → workspace (D2);
- addressing: single domain + path + globally-unique ids (D2);
- authorization + approval semantics (D5).

They **MAY differ only in event/message vocabulary** — managed-agents events vs
`chat.completions` chunks vs AG-UI events. **No front door invents its own
tenancy, auth, or routing.** A new surface is a vocabulary adapter, not a new
stack.

### D4: awaken-iam is the identity + permission backend; three layers, only one is IAM's

awaken reuses awaken-iam **in full** — both authentication (OAuth/OIDC, session,
`ApiToken` verification, WIF) and authorization — not authz alone. The three
layers below are about what IAM *authorizes*; login/identity is the same backend.

| CMA permission layer | What it is | Owner |
|---|---|---|
| **1. Platform RBAC** | org / workspace / member / API key / role | **awaken-iam** (entirely) |
| **2. Tool permission policy** | `always_allow` / `always_ask` | agent config, **driven by IAM decisions** (D5) |
| **3. Agent downstream credentials** | vaults (MCP OAuth, env vars) | **separate secrets plane — not IAM** |

Layer-1 mapping (awaken-iam is a superset):

| CMA | awaken-iam |
|---|---|
| Workspace (shared top) | built-in `Workspace` scope — the shared cross-product boundary (D4a); `Organization` above only in cloud (D6) |
| Member + workspace role | `Account` principal + `RoleBinding`; roles are `RoleDef` action-pattern bundles (preset-seeded) |
| Team (workspace-scoped group) | `Group` (dynamic membership) |
| Project & below | **per-product** — registered `Resource` under the shared Workspace (D4a), not a shared scope |
| API key (workspace-scoped) | `ApiToken` (workspace-scoped, role-bearing) |
| Service account / WIF | `Service` principal + Workload Identity Federation (RFC 8693) |
| OAuth scopes | grants (action-patterns), not token scopes |

#### D4a: Cross-product sharing stops at Workspace; Project and below are per-product

Products that share awaken-iam share **one membership spine — User (`Account`),
Team (`Group`), Workspace — plus the roles/grants/`ApiToken`s bound at those
scopes.** Everything from **Project downward is per-product**: each product
registers its own grouping via awaken-iam's open **ResourceModel**
(`Resource{type,id}` with a parent edge to the shared Workspace); awaken hangs
its agent/session resources this way. **Project is not a shared cross-product
entity, and there is no shared
`ScopeRef::Project` id space** — the built-in `Project` scope is not used to join
products (an earlier proposal to share it is withdrawn).

Rationale: cross-product membership is delivered **for free by Workspace-level
cascade** — a grant at the shared Workspace already covers both products'
resources under it. A shared sub-workspace Project scope would pay off **only** if
users needed one project's membership list to span both products' objects, which
is not a requirement. Avoid the middle state where both products use the built-in
`ScopeRef::Project` over independent id spaces — the same `Project{W, "P1"}` would
then denote two different things and a grant would false-apply. Revisit only if a
unified "one project holds both agents and issues, one membership list" experience
becomes a real product goal.

This also keeps **cloud-login / local-use** clean: the cloud is authoritative for
the shared spine (User/Team/Workspace + roles/grants) and syncs it as the
`PolicySnapshot`; each product's Project-and-below structure is **registered
locally**, evaluated fail-closed against the synced grants. The cloud never needs
to know a product's internal project topology.

### D5: Approval bridge — `always_ask` ≡ `RequireApproval`, single-sourced across all front doors

Tool calls query `authorize(principal_chain, action, resource)` **per call**.
`Allow` → run; `RequireApproval` → emit the front door's confirmation gate
(managed: `always_ask` + `user.tool_confirmation`; AG-UI: its approval event;
AI-SDK: the tool-call gate) and bind the approval obligation back; `Deny` →
reject. The gate is not stored per-tool on the agent — it is derived from one IAM
decision so gate and policy cannot drift (cf. ADR-0037 D2).

### D6: Default tenancy is Workspace + Project; Organization is a cloud-only outer scope

The always-present tenancy skeleton is **`Workspace → Project`** — the default
(self-hosted / embedded) shape, where Workspace is the tenant/data boundary and
there is no Organization. **Organization is an optional
outermost scope present only in the cloud-hosted multi-tenant deployment**,
wrapping workspaces for billing / ownership / cross-workspace admin. awaken-iam's
scope graph (`Global → Organization → Workspace → Project`) already makes
Organization an optional ancestor with cascade from any level, so this is **one
model with a deployment-conditional outer scope**, not two: self-hosted roots the
chain at Workspace; cloud inserts a real Org above it.

This is a **scope-model** decision only — it does not touch D2 addressing.
Org / Workspace / Project are authorization/ownership scopes, **not URL
segments**; the data plane stays key→workspace + globally-unique id in path
regardless of whether an Org exists. Consequently the CMA-compatible **admin**
surface (org + workspace) is a **cloud** property; the self-hosted default
exposes workspace + project and either omits org endpoints or treats org as an
implicit singleton — the data plane is identical either way.

The `Project` in this skeleton is awaken's **own** per-product scope under the
shared Workspace — a registered `Resource` (D4a), **not** a level shared with
other products; cross-product sharing stops at Workspace. Beyond the skeleton
awaken takes the awaken-iam superset: **`on_behalf_of` principal chains are on**
(the agent authorizes as its **own** narrower principal, conjunctively with the
human — CMA leaves this on the table), plus resource-level grants + scope-graph
cascade.

### Ratified decisions

1. **Organization is cloud-only**; the self-hosted default roots the scope chain
   at Workspace. (D6)
2. **Full awaken-iam reuse — authentication *and* authorization**, not authz
   alone. (D4)
3. **`on_behalf_of` is on** — the agent authorizes as its own narrower principal,
   conjunctively with the human. (D4/D6)
4. **Vaults / secrets stay out of IAM** (layer-3 is a separate secrets plane).
   (D4)

## Consequences

- The three front doors feel like one product: same key, same host, same auth,
  same authz/approval — only the event vocabulary changes (D3).
- AI-SDK integration is trivial (issued endpoint = `baseURL`; key = tenant); no
  DNS/wildcard-cert/subdomain machinery; local and self-host work unchanged (D2).
- Authorization is not a second system to reconcile — awaken-iam **is** CMA's
  permission model and a superset; the approval gate is single-sourced (D4/D5).
- The default deployment carries no degenerate Organization level; the tenancy
  skeleton (workspace + project) plus the shared awaken-iam backend keep IAM
  integration uniform across products (D6).
- Tradeoff accepted: key-based tenancy pushes human multi-org UX to a console
  front-end.

## Non-Goals / Deferred

- The AI-SDK-compatible and AG-UI adapter implementations, and the routing
  gateway itself.
- Multi-region key routing.
- Cross-repo realization: awaken-iam wiring and the awaken-next public surface
  land under their own ADRs; this ADR fixes the envelope, not the code.

## References

- [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) — front-door axis vs
  environment/provider axis.
- [ADR-0037](0037-managed-capability-advertisement-wire-alignment.md) — align to
  the official wire; gate/advertisement single-sourcing (D2).
- [ADR-0030](0030-permission-policy-axis.md) — permission policy as a runtime axis.
- [INVARIANTS.md](../INVARIANTS.md) — G16 (only front-door adapters name product
  vocabulary).
