# ADR-0052: The Management Assistant is an Ordinary Agent in a Reserved Scope — Privilege via a Scope-Keyed Tool-Catalog Projection, Not a Special Agent Type

- Status: Proposed
- Date: 2026-07-11
- Builds on:
  [ADR-0051](0051-tenancy-edge-aspect-one-opaque-scope-id.md)
  (tenancy is an edge aspect; one **opaque** `ScopeId` the core never interprets —
  this ADR reuses that type for a reserved **configuration namespace**, while keeping
  it distinct from the real execution Workspace, and leans on its **"model catalog is
  org/deployment-shared"** resolution for D5),
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
  `AgentSpec` in Rust, binds management tools into a bespoke ephemeral
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
**configuration namespace it lives in** and the **tools visible in that namespace** —
both resolved at the edge, both leaving the neutral core and its value objects
untouched. Its execution still belongs to a real Workspace.

Two facts about the current code make the mechanism concrete:

- The tool catalog is **global**: `advertised_tools()`
  (`awaken-runtime-host/src/config.rs:137`) takes no scope; `ConfigService.tools` is a
  single `Vec<ToolDescriptor>` and `installed` is keyed by `agent_id` only
  (`config_plane.rs:31-35`). `compile` lets any config name any tool in that global
  slice (`awaken-config-store/src/compile.rs:51-78`), and errors `UnknownTool` on a
  name that is absent (`compile.rs:56`, fail-closed). There is **no per-scope
  visibility fence today**.
- The model catalog is **org/deployment-shared and readable from any scope**
  (ADR-0051's settled decision; `awaken-server/src/resource_scope_fence.rs`).
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

### D2: Its configuration home is a reserved `ScopeId`; execution uses a real Workspace

Because ADR-0051's `ScopeId` is opaque, the configuration repository can use one
reserved `ScopeId` as a platform-owned **authoring namespace** without introducing a
new Agent kind. We give the assistant this reserved configuration scope and address
its draft through the existing management rewrite —
`/v1/workspaces/{RESERVED}/config/agents/{id}` collapses to the flat config route via
`workspace_path.rs:36` and stamps the scope. This reuses the authoring, addressing,
and per-scope isolation machinery without inventing an Org-tier addressing surface.

The reserved value is **not** a resource, credential, or execution Workspace. At
publication the edge supplies two independent coordinates:

```text
configuration_scope = __admin       # draft/publication and tool visibility
execution_workspace = selected_ws   # installed lookup, resources and credentials
```

`ConfigPlane::publish_for_execution_workspace` reads and persists through the
scope-bound configuration repository, resolves the reserved tool-catalog projection,
and installs the immutable snapshot under `(execution_workspace, agent_id)`. Model
credential access is pinned for that same execution Workspace. Ordinary Agents use
the simpler path where both coordinates are equal. The installed catalog must never
be keyed by Agent id alone.

This is bounded-context separation, not an authorization shortcut. The ingress PEP
still decides whether the principal may manage the reserved configuration namespace;
the configuration service receives only trusted coordinates. Resource stores and
credential adapters receive only the real execution Workspace and never receive
`__admin`, a principal, token, role, policy, or PDP decision. Self-hosted mode uses
its hidden default execution Workspace (ADR-0048 D4).

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
    admin: Vec<ToolDescriptor>,    // the six management descriptors (D4)
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

**The runtime executor registry stays global.** Compile guarantees that only a
snapshot authored through the reserved configuration namespace can *name* the
management tools (an ordinary Workspace config never compiles such a snapshot). That
snapshot is installed in the explicit real execution Workspace; the reserved
namespace is not propagated into Runtime. The management `ToolExecutor`s additionally
check authority
themselves (they read only the org-shared, redacted view) as defense-in-depth. We do
**not** scope the runtime tool registry — the compile-time projection is the fence.

### D4: No sandbox — `Backend::Native`; safety lives in bounded tools behind the gate

The six management tools are bounded platform operations and never publish an Agent.
Capability/help/validation are read-only and redacted; draft mutations use the
ordinary config/environment application ports plus durable audit and idempotency. No
tool accepts arbitrary code, filesystem paths, or network destinations, so
the sandbox axis (`awaken-sandbox-local`, bwrap, `NetworkPolicy`) is a no-op for this
agent. It runs `Backend::Native` in-process. Safety is a property of the
`ToolExecutor` implementations (behind the ADR-0043 gate), not of a cage:

| Tool id | Function | Constraint |
| --- | --- | --- |
| `admin_get_platform_capabilities` | Redacted, scope-aware snapshot of the org's agents / models / providers / plugins / tools / MCP / skills | Read-only; keys/credentials/headers redacted; reads the org-shared view (D5 / ADR-0051), never crosses scope to read tenant detail |
| `admin_draft_agent` | Derive and persist an `AgentConfig` draft from operator intent | Never publishes; audited/idempotent write; output remains `published: false` |
| `admin_patch_agent` | Replace selected draft fields and validate | Never publishes; audited/idempotent write; size-bounded |
| `admin_validate_agent` | Validate a draft with the same server-side check as `/v1/config/agents/validate` | Read-only |
| `admin_draft_environment` | Author an execution-environment draft through the shared registry | Never activates a run; audited/idempotent application write |
| `admin_explain_console` | Return bounded in-console help | Read-only; no arbitrary document or network access |

There is deliberately **no publish tool** — publication remains a console action, not
an LLM tool call. The agent's system instructions are seeded in its `AgentConfig` like
any agent's; we do **not** add a locked-system-prompt / `policy_prompt`-overlay /
bespoke `AdminAssistantConfig` mechanism (reference-impl artifacts whose only purpose
was to protect a prompt that, given read-only tools, needs no protection). The org
admin authors it as an ordinary editable config.

### D5: The model is auto-bound by a first-offering resolver over the shared catalog; resolved at publish, re-resolved on catalog change

The assistant must not require an operator to hand-pick a model, yet must stay
editable. The model catalog is org/deployment-shared and visible from the reserved
scope (ADR-0051; `resource_scope_fence.rs`), so binding is possible; only the
auto-selection is missing.

- **Explicit binding mode, not a sentinel.** `model_binding` is not "empty means
  magic"; it is a two-variant value object that *names* the two intents:

  ```rust
  enum ModelBinding {
      Auto,               // resolve to a first-offering at publish (default)
      Pinned(ModelRef),   // operator's explicit choice — never overwritten
  }
  ```

  `Auto` is the default; an operator edit that picks a model produces `Pinned`. The
  variants give "auto by default, editable to override" in one field — no parallel
  config — while revealing the intent at the type level (no reader has to know that an
  absent value carries behavior). `compile` still requires a *concrete* model (it reads
  the resolved `RunnableConfig`, not this source field), so `Auto` must be resolved to a
  `Pinned`-equivalent before compile — which is exactly what the resolver does below.
- **Resolver.** For `ModelBinding::Auto`, a `ModelResolver` selects the **first
  provider-backed (non-scripted) offering** from the `ProviderCatalog`, reusing
  config-resolver's existing "first offering" semantics
  (`awaken-config-resolver/src/lib.rs:96,120`). The remaining provider-backed offerings
  fill `model_candidates` (`config.rs:46`), giving the engine's existing pool-failover
  (`awaken-runtime/src/engine/mod.rs:567,625`) something to fall back to at run time.
  `Pinned(m)` resolves to `m` untouched. If no provider-backed model exists, resolution
  fails **loud at publish** with a 409 ("configure and publish a model first").
- **Placement: publish-time.** Resolution runs in `ConfigService::publish` *before*
  `compile` (which requires a concrete binding, `compile.rs:82`), so the stored
  `RunnableConfig` is self-contained, content-addressed, reproducible, and fails at
  configuration time rather than at first use. This keeps the resolver entirely in the
  config plane and off the run/activation path.
- **Freshness: one explicit reconciler port, not a scattered hook.** The one place this
  design bends simple design is that an `Auto` binding resolved at publish can go stale
  when the model catalog changes. We converge that concern into a single named seam
  rather than an ad-hoc callback:

  ```rust
  // Consumer: the model-catalog write path, which calls this after a catalog mutation.
  // It re-resolves every Auto-bound assistant and re-publishes through the ordinary
  // ConfigService path; Pinned bindings are skipped (an operator pin is authoritative).
  trait AssistantBindingReconciler: Send + Sync {
      async fn reconcile(&self, scope: &ScopeId) -> Result<Reconciled, ReconcileError>;
  }
  ```

  This gives the staleness recovery **one testable home** with an explicit contract for
  *who triggers it* (the catalog write path), *what it touches* (only `Auto` configs in
  the scope), and *how failure surfaces* (a `ReconcileError` the caller logs/retries —
  a failed reconcile leaves the last good published binding in place, never a broken
  one). It is idempotent (re-resolving an already-current `Auto` is a no-op by content
  address) so a retry loop is safe. The bounded staleness window is the interval between
  the catalog write and a successful `reconcile`.

  Rejected alternative — run-time resolution (resolve `Auto` at each activation): it is
  "live/self-healing" but pushes resolution into the run path (which today assumes an
  already-concrete binding), fails at run time instead of publish, and yields
  non-reproducible artifacts. Only warranted under strict real-time model churn, which
  is not our posture.

### D6: Access control by `Authority::covers`; no reserved-id guard; audit every change

Tenants are kept out of the reserved scope by the **existing** ingress reconciliation:
`resolve_scope` + `Authority::covers` (`awaken-tenancy/src/lib.rs:178-202`) — a narrow
tenant token's authority does not cover the reserved scope, so it cannot select it via
path or domain (a narrow token cannot widen via path/domain, tested at
`tenancy/lib.rs:253-281`). No bespoke reserved-id blocklist is needed. Every management
tool invocation is already durable Runtime history (`ToolCall`/`ToolResult`). Mutating
tools additionally emit a secret-free change record (target `awaken::admin_audit`) and
atomically pair the business write with a Runtime-owned `run + step + call` operation
identity. Read-only capability/help/validate calls do not enter the config-store
idempotency path.

## Consequences

Positive:

- One authoring model. The management assistant is validated/compiled/edited by the
  same `ConfigService` path as any agent; the reference impl's ephemeral parallel path
  and this branch's `RunnableConfig::builder` bypass are both avoided for it.
- Neutral core and its value objects are untouched — no `origin`/`audience`/`locked`
  field; privilege is an edge projection (scope + tool catalog), consistent with
  ADR-0051 (tenancy) and ADR-0034 (protocol).
- No new tenancy tier and no new resource: the reserved value is a configuration
  namespace only. Runtime resources, credential access, and installed lookup use the
  explicit real Workspace selected at publication.
- No sandbox surface to build or reason about; safety is localized in six bounded
  tool executors behind the existing gate and audited application ports.
- Model binding is zero-touch for the operator yet fully editable, reproducible, and
  fails loud at configuration time. Its two intents are named at the type level
  (`ModelBinding::{Auto, Pinned}`), not encoded as a magic absent value.
- The one place the design bends (freshness of an `Auto` binding after a catalog
  change) is isolated behind a single named port (`AssistantBindingReconciler`) with an
  explicit trigger/scope/failure contract, rather than a diffuse hook — so the bend has
  one testable, reason-about-able home.

Costs (accepted):

- `ConfigService` gains a scope parameter on `validate`/`publish` and swaps its fixed
  `tools` vec for a `ToolCatalogSource`. This is the price of making tool visibility a
  function of scope; it is small and localized.
- The model-catalog write path must call `AssistantBindingReconciler::reconcile` to
  avoid a stale `Auto` binding — a bounded staleness window between the catalog write
  and a successful reconcile (accepted per D5; idempotent + `Pinned`-skipping, so a
  retry is safe; the run-time alternative was rejected).
- One reserved configuration-scope constant to seed and document, plus an explicit
  execution-Workspace argument at the exceptional Admin publication/reconcile seam.

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

- **S1** — New crate `crates/control/awaken-admin-assistant`: the six `ToolExecutor`s
  (bounded / redacted / never-publish) and their six `ToolDescriptor`s, plus
  the seeded instruction text. Add the crate to `check_crate_boundaries.py`
  `ALLOWED_DEPS`.
- **S2** — `ToolCatalogSource` trait + `ScopedToolCatalog`; `ConfigService.tools` →
  `Arc<dyn ToolCatalogSource>`; thread `scope` into `validate`/`publish`
  (`config_plane.rs:72,88`); inject `ScopedToolCatalog { advertised_tools(...),
  reserved_scope, admin_descriptors() }` at assembly (`server-local/src/lib.rs:1321`).
  Test: reserved scope compiles a config naming the admin tools; a non-reserved scope
  with the same config gets `UnknownTool`.
- **S3** — Introduce `ModelBinding::{Auto, Pinned}`; seed the assistant as an ordinary
  `AgentConfig` (locked-free, instructions + `tool_ids` = the six tools +
  `ModelBinding::Auto`) in the reserved scope; publish it through the ordinary path so it
  lands in `AgentCatalog` as a compiled `RunnableConfig`; `Backend::Native`, no sandbox.
  Retire this agent's builder bypass. Test: `Pinned` survives publish untouched.
- **S4** — `ModelResolver` (first-offering for `Auto`, pass-through for `Pinned`,
  `model_candidates` fill) run in `publish` before `compile`; 409 when no provider-backed
  model exists. Then `AssistantBindingReconciler` called from the model-catalog write
  path: re-resolves `Auto` assistants, skips `Pinned`, idempotent by content address.
  Test: a catalog change re-publishes an `Auto` assistant to the new first-offering and
  leaves a `Pinned` one unchanged.
- **S5** — Reserved-scope access via `Authority::covers` (no bespoke guard);
  Runtime `ToolCall`/`ToolResult` history for every invocation,
  `awaken::admin_audit` plus atomic config-store identity for mutations; management
  `ToolExecutor` authority check as defense-in-depth.
