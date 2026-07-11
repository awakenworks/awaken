# ADR-0052: The Management Assistant is an Ordinary Agent in a Reserved Scope — Privilege via a Scope-Keyed Tool-Catalog Projection, Not a Special Agent Type

- Status: Proposed
- Date: 2026-07-11
- Builds on:
  [ADR-0051](0051-tenancy-edge-aspect-one-opaque-scope-id.md)
  (tenancy is an edge aspect; one **opaque** `ScopeId` the core never interprets —
  this ADR reuses that opacity so a *reserved* scope and a future *org* scope are the
  same mechanism, and leans on its **"model catalog is org/deployment-shared"**
  resolution for D5),
  [ADR-0047](0047-compaction-as-agent-run-and-the-context-plane-boundary.md)
  (a built-in agent — the compactor — is a **normal agent run at the same execution
  altitude**, not a privileged type; the management assistant follows the same rule),
  [ADR-0034](0034-runtime-axis-model-and-orthogonality.md)
  (protocol is a projection over the neutral core; here **tool-catalog visibility** is
  likewise a projection over the request's scope),
  [ADR-0031/0032](0032-runnable-config.md)
  (`AgentConfig --compile--> RunnableConfig`; the runtime consumes the compiled
  snapshot, not the source config)
- Relates to:
  [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  (server-owned management tools; authority lives behind the `ToolExecutor` gate, not
  on the descriptor),
  [ADR-0048](0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md)
  D3 (management addressing via `/v1/workspaces/{ws}/…`; org resolved from the
  principal), D4 (org is cloud-only; self-hosted seeds one scope)

## Context

We want the platform equivalent of the "FAB" management assistant shipped elsewhere
in the product line (the `__admin_assistant` in `awaken-server` on `origin/main` and
in the `goal` reimplementation — a right-hand-corner console helper that reads
platform capabilities, drafts and validates `AgentConfig`s, but never publishes and
never touches secrets). That crate does **not** exist on this `1.0.0-dev` branch, so
this is a **port onto this branch's architecture**, not a code move.

How the reference implementation does it, and why we should *not* copy it verbatim:

- It **hardcodes a parallel path**: `admin_assistant_agent()` synthesises an
  `AgentSpec` in Rust, binds four `admin_*` tools into a bespoke ephemeral
  `MapToolRegistry`, and streams a one-shot runtime that never enters the config
  store. The "specialness" is a second, private code path.
- This branch already carries a *different* bypass of its own: the built-in agents
  (`assistant`, `compactor`, `judge`, memory extractor/selector, native delegate) are
  each hardcoded as `RunnableConfig::builder(...)` in host/ext crates
  (`awaken-runtime-host/src/config.rs:182`, `awaken-ext-compact/src/agent.rs:23`, …),
  never expressed as an `AgentConfig`, never compiled or content-addressed. The
  config store serves only user configs.

The stakeholder requirement cuts against both bypasses: the management assistant must
be **authored and edited exactly like an ordinary platform agent**, must **carry
tools**, and must **auto-bind a model**. The organizing insight is therefore the
mirror of ADR-0051's: *privilege is a cross-cutting edge aspect, not a property of the
agent definition.* The assistant is an ordinary `AgentConfig`; what differs is the
**scope it lives in** and the **tools visible in that scope** — both resolved at the
edge, both leaving the neutral core and its value objects untouched.

Two facts about the current code make the mechanism concrete:

- The tool catalog is **global**: `advertised_tools()`
  (`awaken-runtime-host/src/config.rs:137`) takes no scope; `ConfigService.tools` is a
  single `Vec<ToolDescriptor>` and `installed` is keyed by `agent_id` only
  (`config_plane.rs:31-35`). `compile` lets any config name any tool in that global
  slice (`awaken-config-store/src/compile.rs:51-78`), and errors `UnknownTool` on a
  name that is absent (`compile.rs:56`, fail-closed). There is **no per-scope
  visibility fence today**.
- The model catalog is **org/deployment-shared and readable from any scope**
  (ADR-0051's settled decision; `awaken-server-local/src/resource_owner.rs:14-15`).
  `compile` requires `model_binding` to be already filled (`compile.rs:82`, a plain
  clone with no default). There is **no auto-selection of a model today**.

## Decision

### D1: The management assistant is an ordinary `AgentConfig`, not a special agent type

It is authored, validated, compiled, content-addressed, published, and edited through
the **same** path as any platform agent (`ConfigService::validate/publish` →
`compile` → `RunnableConfig` → `AgentCatalog`). It is **not** an ephemeral synthesised
spec (reference impl) and **not** a `RunnableConfig::builder` hardcode (this branch's
built-ins). `AgentConfig` (`awaken-config-store/src/config.rs:13-48`) and
`ToolDescriptor` (`awaken-runtime-contract/src/resolved.rs:183-192`) gain **no new
field** — no `origin`, no `kind`, no `audience`, no `locked`. Adding such a marker
would smear authorization onto model-facing value objects that ADR-0043 keeps
authorization-free ("authority lives behind the gate"). Privilege is expressed by
**where the config lives** and **what its scope can see** (D2, D3), not by a flag on
the definition.

Corollary: this agent's hardcoded bypass is retired in favour of the ordinary path.
Migrating the *other* built-ins (compactor, judge, memory) onto `AgentConfig` is
**orthogonal and out of scope** — pursued separately if at all.

### D2: Its home is a reserved `ScopeId`, addressable as a reserved workspace

Because ADR-0051's `ScopeId` is **opaque** and the core never interprets whether it
denotes a workspace or an org, "a special workspace for the assistant" and "an
org-tier resource" are the **same mechanism** at the persistence/fence layer: one
reserved `ScopeId` stamped by `ScopedConfig`. We therefore give the assistant a
**reserved scope** and address it through the **existing** management rewrite —
`/v1/workspaces/{RESERVED}/config/agents/{id}` collapses to the flat config route via
`workspace_path.rs:36` and stamps the scope. This **reuses** all authoring,
addressing, and per-scope isolation machinery and **avoids inventing an Org-tier
addressing surface** (which does not exist today — `request_scope` always anchors at
`ScopeRef::Workspace`, `authz-enforce/lib.rs:165-169`).

Precedent: `DEFAULT_SCOPE = "default"` (`awaken-config-store/src/store.rs:96`) already
establishes a reserved-scope constant. Multi-org deployments derive one reserved scope
per org; self-hosted single-org collapses to one (ADR-0048 D4). No new tier, no new
column.

### D3: Tool visibility is a scope-keyed catalog **projection** — a `ToolCatalogSource` resolver

The management tools must be nameable **only** in the reserved scope. We express this
as a projection of the tool catalog over the request's scope, not as a field on the
tool and not as duplicated `ConfigService` instances:

```rust
// The consumer is ConfigService's compile-feed; named for that role.
trait ToolCatalogSource: Send + Sync {
    fn catalog_for(&self, scope: &ScopeId) -> Vec<ToolDescriptor>;
}

struct ScopedToolCatalog {
    global: Vec<ToolDescriptor>,   // advertised_tools() output — every scope sees this
    reserved_scope: ScopeId,
    admin: Vec<ToolDescriptor>,    // the four management descriptors (D4)
}
// catalog_for(s) = if s == reserved_scope { [global, admin].concat() } else { global }
```

- `ConfigService.tools: Vec<ToolDescriptor>` becomes `Arc<dyn ToolCatalogSource>`;
  `validate` (`config_plane.rs:72`) and `publish` (`config_plane.rs:88`) take the
  request `scope` (already stamped at the edge, `workspace_path.rs:64`) and call
  `compile(cfg, &catalog.catalog_for(scope))`.
- **The fence bites at compile time**, reusing the existing behavior: a config in a
  non-reserved scope that names a management tool hits `UnknownTool` (`compile.rs:56`)
  — fail-closed, and the tool's *existence* is not disclosed to tenants.
- `ToolDescriptor` stays pure: **visibility is catalog membership**, computed by the
  resolver per scope, not an attribute of the descriptor (consistent with D1). This is
  the same grain as ADR-0051's `ScopedConfig` decorator and ADR-0034's
  protocol-as-projection — "which tools exist for this scope" is now a function of the
  opaque scope.

**The runtime executor registry stays global.** compile already guarantees that only
the reserved scope's compiled snapshot can *name* the management tools (a tenant
config never compiles a snapshot that references them), so no run outside the reserved
scope can invoke them. The management `ToolExecutor`s additionally check authority
themselves (they read only the org-shared, redacted view) as defense-in-depth. We do
**not** scope the runtime tool registry — the compile-time projection is the fence.

### D4: No sandbox — `Backend::Native`; safety lives in the read-only tools behind the gate

The four management tools are **capability-access only**: read-only, redacted,
never-publish. There is no untrusted execution, no filesystem write, no egress — so
the sandbox axis (`awaken-sandbox-local`, bwrap, `NetworkPolicy`) is a no-op for this
agent. It runs `Backend::Native` in-process. Safety is a property of the
`ToolExecutor` implementations (behind the ADR-0043 gate), not of a cage:

| Tool id | Function | Constraint |
| --- | --- | --- |
| `admin_get_platform_capabilities` | Redacted, scope-aware snapshot of the org's agents / models / providers / plugins / tools / MCP / skills | Read-only; keys/credentials/headers redacted; reads the org-shared view (D5 / ADR-0051), never crosses scope to read tenant detail |
| `admin_create_agent_draft` | Derive an `AgentConfig` draft from operator intent | Never writes, never publishes; output always `published: false` |
| `admin_set_plugin_config` | Attach/replace a plugin config section on a draft and validate | Never publishes; size-bounded |
| `admin_validate_agent` | Validate a draft with the same server-side check as `/v1/config/agents/validate` | Read-only |

There is deliberately **no publish tool** — publication remains a console action, not
an LLM tool call. The agent's system instructions are seeded in its `AgentConfig` like
any agent's; we do **not** add a locked-system-prompt / `policy_prompt`-overlay /
bespoke `AdminAssistantConfig` mechanism (reference-impl artifacts whose only purpose
was to protect a prompt that, given read-only tools, needs no protection). The org
admin authors it as an ordinary editable config.

### D5: The model is auto-bound by a first-offering resolver over the shared catalog; resolved at publish, re-resolved on catalog change

The assistant must not require an operator to hand-pick a model, yet must stay
editable. The model catalog is org/deployment-shared and visible from the reserved
scope (ADR-0051; `resource_owner.rs:14-15`), so binding is possible; only the
auto-selection is missing.

- **Sentinel binding.** A `model_binding` left unset (a sentinel) means "auto"; an
  operator-set binding is a **pin** and is never overwritten. The two states give
  "auto by default, editable to override" in one field — no parallel config.
- **Resolver.** For a sentinel, a `ModelResolver` selects the **first provider-backed
  (non-scripted) offering** from the `ProviderCatalog`, reusing config-resolver's
  existing "first offering" semantics (`awaken-config-resolver/src/lib.rs:96,120`). The
  remaining provider-backed offerings fill `model_candidates`
  (`config.rs:46`), giving the engine's existing pool-failover
  (`awaken-runtime/src/engine/mod.rs:567,625`) something to fall back to at run time.
  If no provider-backed model exists, resolution fails **loud at publish** with a 409
  ("configure and publish a model first").
- **Placement: publish-time, with re-resolution on catalog change.** Resolution runs
  in `ConfigService::publish` *before* `compile` (which requires a filled binding,
  `compile.rs:82`), so the stored `RunnableConfig` is self-contained,
  content-addressed, reproducible, and fails at configuration time rather than at first
  use. To recover the freshness that a purely run-time resolution would give, a change
  to the model catalog **re-resolves and re-publishes** the assistant. This keeps the
  resolver entirely in the config plane and off the run/activation path.

  Rejected alternative — run-time resolution (resolve the sentinel at each
  activation): it is "live/self-healing" but pushes resolution into the run path
  (which today assumes an already-concrete binding), fails at run time instead of
  publish, and yields non-reproducible artifacts. Only warranted under strict
  real-time model churn, which is not our posture.

### D6: Access control by `Authority::covers`; no reserved-id guard; audit every call

Tenants are kept out of the reserved scope by the **existing** ingress reconciliation:
`resolve_scope` + `Authority::covers` (`awaken-tenancy/src/lib.rs:178-202`) — a narrow
tenant token's authority does not cover the reserved scope, so it cannot select it via
path or domain (a narrow token cannot widen via path/domain, tested at
`tenancy/lib.rs:253-281`). No bespoke reserved-id blocklist is needed. Every
management tool call emits a structured audit record (target `awaken::admin_audit`),
including the read-only draft/validate tools.

## Consequences

Positive:

- One authoring model. The management assistant is validated/compiled/edited by the
  same `ConfigService` path as any agent; the reference impl's ephemeral parallel path
  and this branch's `RunnableConfig::builder` bypass are both avoided for it.
- Neutral core and its value objects are untouched — no `origin`/`audience`/`locked`
  field; privilege is an edge projection (scope + tool catalog), consistent with
  ADR-0051 (tenancy) and ADR-0034 (protocol).
- No new tenancy tier and no new addressing surface: the reserved scope reuses
  ADR-0051's opaque `ScopeId` and ADR-0048's workspace rewrite. "Reserved workspace"
  and "org-tier resource" are the same mechanism.
- No sandbox surface to build or reason about; safety is localized in four read-only
  tool executors behind the existing gate.
- Model binding is zero-touch for the operator yet fully editable, reproducible, and
  fails loud at configuration time.

Costs (accepted):

- `ConfigService` gains a scope parameter on `validate`/`publish` and swaps its fixed
  `tools` vec for a `ToolCatalogSource`. This is the price of making tool visibility a
  function of scope; it is small and localized.
- A model-catalog change must trigger re-resolution/re-publish of the assistant to
  avoid a stale binding — a bounded staleness window between the two events (accepted
  per D5; the run-time alternative was rejected).
- One more reserved-scope constant to seed and document.

## Alternatives considered

- **Copy the reference impl (hardcoded `AgentSpec` + ephemeral private registry).**
  Rejected: a second code path, not editable as an ordinary agent — contradicts the
  stakeholder requirement (D1).
- **Add `AgentConfig.origin` + `ToolDescriptor.audience` marker fields.** Rejected:
  smears authorization onto authz-free, model-facing value objects; `origin`
  (provenance) would be overloaded to drive editability, tool admission, and run authz
  at once (D1). Catalog membership (D3) expresses the same fence without a field.
- **Two `ConfigService` instances (management vs tenant), each with its own catalog.**
  Rejected: structural duplication (`installed` maps, routing-by-instance) where a
  scope-keyed resolver is one port and one place to reason about visibility (D3).
- **Scope the runtime tool-executor registry too.** Rejected as redundant: the
  compile-time projection already prevents any non-reserved snapshot from naming the
  tools; executor-level authority is kept only as defense-in-depth (D3).
- **Invent an Org-tier addressing/authz surface for the assistant.** Rejected:
  unnecessary given opaque `ScopeId` — a reserved scope addressed via the workspace
  rewrite is mechanically identical and far cheaper (D2).
- **Run-time model resolution.** Rejected for our posture (D5) — non-reproducible,
  fails late, touches the run path.
- **Give the agent a sandbox / dedicated OS workspace.** Rejected: wrong axis for a
  trusted, read-only, in-process agent (D4).

## Migration (slices, each independently green, no stubs)

- **S1** — New crate `crates/agents/awaken-admin-assistant`: the four `ToolExecutor`s
  (read-only / redacted / `published: false`) and their four `ToolDescriptor`s, plus
  the seeded instruction text. Add the crate to `check_crate_boundaries.py`
  `ALLOWED_DEPS`.
- **S2** — `ToolCatalogSource` trait + `ScopedToolCatalog`; `ConfigService.tools` →
  `Arc<dyn ToolCatalogSource>`; thread `scope` into `validate`/`publish`
  (`config_plane.rs:72,88`); inject `ScopedToolCatalog { advertised_tools(...),
  reserved_scope, admin_descriptors() }` at assembly (`server-local/src/lib.rs:1321`).
  Test: reserved scope compiles a config naming the admin tools; a non-reserved scope
  with the same config gets `UnknownTool`.
- **S3** — Seed the assistant as an ordinary `AgentConfig` (locked-free, instructions
  + `tool_ids` = the four tools + sentinel `model_binding`) in the reserved scope;
  publish it through the ordinary path so it lands in `AgentCatalog` as a compiled
  `RunnableConfig`; `Backend::Native`, no sandbox. Retire this agent's builder bypass.
- **S4** — `ModelResolver` (first-offering + sentinel + `model_candidates` fill) run in
  `publish` before `compile`; a model-catalog-change hook re-resolves and re-publishes.
  409 when no provider-backed model exists.
- **S5** — Reserved-scope access via `Authority::covers` (no bespoke guard);
  `awaken::admin_audit` on every tool call; management `ToolExecutor` authority check
  as defense-in-depth.
