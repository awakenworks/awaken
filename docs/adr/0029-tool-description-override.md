# ADR-0029: Tool Description Override

- **Status**: Accepted
- **Date**: 2026-05-05
- **Depends on**: ADR-0010, ADR-0014, ADR-0023, ADR-0024, ADR-0026

## Context

Operators can already override built-in agent specs through the
`PATCH /v1/config/agents/:id/overrides` surface that landed alongside the
admin console refresh. The same mental model — a `ConfigRecord<T>` envelope
with a `Builtin` source and optional `user_overrides` JSON merged at
read-time — is the right shape for tuning the descriptions that registered
tools advertise to the model.

Tool descriptions are a first-class prompt-engineering surface for tool-using
LLMs:

- They disambiguate adjacent tools (`read_file` vs `read_database`).
- They encode non-obvious constraints ("paths are project-relative",
  "max 1000 results") that the schema cannot express.
- They allow domain/locale alignment so descriptions speak the agent's
  vocabulary instead of generic English.
- They give operators a no-deploy lever to correct misleading wording when
  an agent misroutes calls in production.

Today every tool's description is a hard-coded string returned by
`Tool::descriptor()` and there is no path to change it without rebuilding
the binary.

## Non-Goals

- **Per-agent overrides.** Per-agent variation belongs in the agent's
  `system_prompt`, not in description fan-out. Layering per-agent overrides
  on top of this design is a strict additive future step, not in scope here.
- **Schema or name overrides.** `parameters_schema` and `id`/`name` are part
  of the calling contract. Allowing UI edits would silently break tool
  invocation. Out of scope.
- **Authoring new tools from the UI.** Tools are still registered in code
  via the `Tool` trait. The UI tunes metadata of code-registered tools only.
- **Replacing system prompts.** Long, rule-laden tool descriptions dilute
  attention. The UI surfaces a soft length warning that pushes operators to
  put complex behaviour rules in the agent's system prompt rather than the
  description.

## Decisions

### D1: `Tool` becomes a first-class `ConfigRecord` kind

A new `ToolSpec` is added to `awaken-contract`, stored in `ConfigStore`
under namespace `tools` with `source: Builtin { binary_version }`. The
record envelope, audit hooks, restore semantics (ADR-0028) and listing API
(ADR-0023) all apply uniformly.

```rust
// crates/awaken-contract/src/tool_spec.rs
pub struct ToolSpec {
    pub id: ToolId,                 // canonical key, sourced from ToolDescriptor.id
    pub name: String,                // read-only display label
    pub description: String,         // the tunable field
    pub category: Option<String>,
    pub parameters_schema: Value,    // read-only snapshot, for UI + audit context
}
```

User-created tools are not allowed; `RecordSource::User` is rejected by the
service layer with 422 for namespace `tools`.

### D2: Patch surface — `description` only

```rust
// crates/awaken-contract/src/tool_spec_patch.rs
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpecPatch {
    pub description: Option<String>,
}
impl Patchable<ToolSpec> for ToolSpecPatch { /* merge_tool_spec */ }
```

`merge_tool_spec(spec, patch)` follows the same `Option`-as-inheritance
convention as `merge_agent_spec`: `None` keeps the built-in value, `Some(v)`
replaces it. `deny_unknown_fields` keeps the surface forward-compatible —
adding a new patchable field later is purely additive and rejects stale
clients early.

Validation rules applied in `ConfigService::patch_tool_overrides`:

- `description` must be non-empty after trim.
- Length hard-cap: 4096 bytes (defensive; UI surfaces a soft warning at
  ≥ 400 chars).
- No control characters except `\n`.

### D3: Seed protocol

At startup `SpecRegistry` enumerates registered tools and writes a
`ConfigRecord<ToolSpec>` for each, taking the live `ToolDescriptor` as the
built-in payload. The existing idempotent seed protocol handles version
bumps automatically: when `binary_version` advances, the built-in payload is
refreshed but `user_overrides` is preserved.

### D4: HTTP API

| Method   | Path                                              | Purpose                                  |
|----------|---------------------------------------------------|------------------------------------------|
| `GET`    | `/v1/config/tools`                                | List with effective specs, paginated     |
| `GET`    | `/v1/config/tools/:id`                            | Effective (post-merge) spec              |
| `GET`    | `/v1/config/tools/:id/meta`                       | `RecordMeta` including raw overrides     |
| `PATCH`  | `/v1/config/tools/:id/overrides`                  | Shallow-merge new patch into overrides   |
| `DELETE` | `/v1/config/tools/:id/overrides`                  | Clear all overrides (revert to built-in) |
| `DELETE` | `/v1/config/tools/:id/overrides/:field`           | Clear a single override field            |

There is no `POST` or `PUT` for `tools` — built-in is the only allowed
source. The single-field `DELETE` is provided for symmetry with agents and
forward-compatibility with future patchable fields; today only `description`
is accepted.

All routes are behind `ensure_admin_auth` (ADR-0023).

### D5: Runtime application point

The override is applied at registry compile time, in the same call chain
that already merges agent overrides:

```
PATCH /v1/config/tools/:id/overrides
  └─> ConfigService::patch_tool_overrides
       └─> apply_locked()
            ├─> load_managed_config()
            │    └─> deserialize_namespace()
            │         └─> apply_overrides() merges ToolSpecPatch
            ├─> compile_registry_set()
            │    └─> for each tool: take Tool::descriptor(), substitute
            │        description from the merged ToolSpec, store the
            │        patched ToolDescriptor in the snapshot
            └─> replace_registry_set()  // RwLock-backed atomic swap
```

The `Tool` trait is **not** modified. The override lives on the
`ToolDescriptor` stored in the registry snapshot, alongside the unchanged
`Arc<dyn Tool>` whose `invoke()` behaviour is untouched.

### D6: Hot-reload semantics

Mirrors the existing agent override behaviour exactly:

| Dimension                            | Agent override | Tool description override |
|--------------------------------------|----------------|----------------------------|
| Trigger                              | PATCH → `apply_locked` → snapshot replace | same |
| Merge point                          | `apply_overrides` in `deserialize_namespace` | same |
| Synchronisation                      | `Arc<RwLock<RegistrySnapshot>>` | same |
| New-run effect after PATCH           | immediate, no restart | immediate, no restart |
| In-flight run effect after PATCH     | not applied — `ResolvedAgent` is captured at run start and reused | identical |
| Blast radius of a single PATCH       | full registry set recompile + atomic swap | identical |

The "in-flight run is unaffected" property is a known and intentional
behaviour of the existing config pipeline (ADR-0014). It is documented here
explicitly so future readers do not treat it as a tool-specific quirk.

### D7: Audit

Reuses `AuditEvent` with `resource = "tools/:id"` and the standard
`before` / `after` payloads carrying the **effective** `ToolSpec`. The
restore endpoint from ADR-0028 works for `tools` resources without
modification because the patch flow goes through the same envelope path.

### D8: Admin console UI

A new top-level Tools section is added to the admin console, sibling to
Agents:

- **List page** `/tools` — every registered tool, columns: id, name,
  category, "overridden?" indicator, `updated_at`. Filter: show only
  overridden tools.
- **Editor page** `/tools/:id` — three-view layout reused from the agent
  editor:
  - **Built-in** (read-only): the binary-shipped description.
  - **User override** (editable): the patch payload.
  - **Effective** (read-only preview): post-merge result, identical to what
    the LLM will see.
  - "Revert to default" button calls `DELETE …/overrides`.
  - Live character count with a soft warning at ≥ 400 chars: "Long
    descriptions dilute model attention. Consider moving rules into the
    agent's system prompt."

Diff/save logic reuses the `diffPatchableFields()` pattern that lives in
`apps/admin-console/src/pages/agent-editor-page.tsx`; the API client
(`apps/admin-console/src/lib/config-api.ts`) gains
`patchToolOverrides(id, patch)` mirroring `patchAgentOverrides`.

### D9: Orphan handling on tool removal

When a tool is removed from the binary (no longer registered) the seed
protocol, applied universally to every namespace, takes one of two paths:

- If the existing `ConfigRecord<ToolSpec>` has a non-empty
  `meta.user_overrides`, the seed marks `meta.hidden = true` instead of
  deleting it. The runtime's `deserialize_namespace` skips hidden
  records, so the tool is invisible to resolution; the override is
  preserved for forensic and recovery purposes.
- Otherwise the record is hard-deleted.

Re-introducing the spec in a later binary version automatically clears
`hidden = false`, restoring the tool to the runtime with the user
override still applied. This soft-delete-with-revival cycle is the
mechanism that makes rolling deploys safe even when a tool's id changes
or temporarily disappears from a partial rollout.

`hidden` is reserved for orphan-preservation today; user-toggleable hide
would need a separate signal. See `crates/awaken-server/src/services/builtin_seed.rs`.

### D10: Validation surface

`ConfigService::patch_tool_overrides` rejects with 422 when:

- The target tool id is not currently registered (no orphan patching).
- The description fails the validation rules from D2.
- The body contains fields other than `description` (deny_unknown_fields).
- The caller attempts a `RecordSource::User` payload via PATCH.

A patch that yields an empty `user_overrides` after merge is normalised to
`None` so the meta surface stays clean.

### D11: Distributed deployment

The override mechanism inherits the existing config pipeline's distributed
semantics. The same caveats and operational guidance apply uniformly to
every namespace (agents, providers, models, mcp-servers, tools).

**Cross-instance propagation.** Each replica maintains its own in-memory
`Arc<RwLock<RegistrySnapshot>>`. A `PATCH` updates only the receiving
replica's snapshot; followers pick up the change via
`start_periodic_refresh` (default cadence is operator-supplied; the
starter backend uses 5 s). Stores that implement
`ConfigChangeNotifier` (e.g. Postgres LISTEN/NOTIFY, Redis pub/sub) push
events through `ConfigRuntimeManager::start_change_listener`, reducing
the cross-replica latency to single-digit milliseconds; periodic refresh
remains the safety net.

**Concurrent admin writes.** `lock_apply()` is an in-process mutex. Two
replicas receiving simultaneous `PATCH`es to the same record can race a
read-modify-write in the shared `ConfigStore`; the second write wins. For
multi-replica admin traffic this ADR recommends sticky-session routing
(L7 cookie or consistent hash on `Authorization`) until a future
revision adds optimistic concurrency (revision number on `RecordMeta`
plus a CAS `put_if_revision` method on `ConfigStore`).

**Boot-time seed contention.** The seed protocol's documented
precondition is single-writer. Multi-replica deploys must run
`apply_seed` on exactly one replica per cluster. The starter backend
exposes this contract through the `--leader-seed` /
`AWAKEN_LEADER_SEED` flag (default `true` for back-compat); operators
running multiple replicas set it to `false` on followers, which then
rely on `start_periodic_refresh` (or the change notifier) to absorb the
leader's writes. Combined with the soft-delete-on-orphan behaviour
described in D9, this keeps user overrides safe across rolling
upgrades that change the registered tool/agent set.

**In-flight runs.** A run captures its `ResolvedAgent` at start; mid-run
override changes do not affect it. Any replica's in-flight runs see the
description present at run start. Restarting affected runs is required
for immediate-effect propagation. This is the established behaviour
documented in ADR-0014; no tool-specific change.

**Apply rollback.** When `apply_locked` fails after a store write, the
local replica rolls back the envelope. Other replicas that have already
absorbed the in-flight write through `periodic_refresh` only see the
rollback on their next refresh. The window is bounded by the refresh
interval. Audit emission for apply-failure is a future improvement
(operator visibility, not correctness).

## Alternatives Considered

**Per-agent description overrides as the primary mechanism.** Rejected as
the default surface: it conflates "the tool's own metadata" with "how a
specific agent talks about that tool", which is what `system_prompt` already
expresses. Layering it on top later is purely additive — adding
`tool_description_overrides: HashMap<ToolId, String>` to `AgentSpecPatch`
would not require any change to this design.

**Storing overrides outside `ConfigStore`.** Rejected because it would
fragment the audit, restore, version-switching and seeding mechanisms that
already cover all spec kinds uniformly.

**Allowing `name` and `category` to be patchable.** Out of scope for this
ADR. `name` may be the tool-calling protocol identifier in some integrations
and changing it could break invocation; `category` is a UI grouping concern
that we do not yet have a story for. Both can be added later without
breaking changes thanks to `deny_unknown_fields` on the patch type.

## Open Questions

- Should the soft length threshold be operator-configurable per
  installation? Defaulting to 400 chars based on rule-of-thumb for current
  tool-using models; revisit if telemetry shows operators routinely
  approving longer descriptions.
- Localisation: a single `description` string assumes one
  audience-language-at-a-time. Multi-language tooling would extend the
  patch shape later (e.g. `description: Map<Locale, String>`).
