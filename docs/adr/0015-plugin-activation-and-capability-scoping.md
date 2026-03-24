# ADR-0015: Plugin Activation and Capability Scoping

- **Status**: Accepted
- **Date**: 2026-03-27
- **Supersedes**: ADR-0005
- **Depends on**: ADR-0001, ADR-0014

## Context

ADR-0005 described plugin activation derived from live configuration sources, recomputed at each execution boundary. The actual implementation uses a two-level mechanism: `plugin_ids` controls which plugins are loaded, and `active_hook_filter` controls which plugin capabilities are active at runtime. Filtering is applied at resolve time, not recomputed per-phase.

## Decision

### D1: plugin_ids controls plugin loading

`AgentSpec::plugin_ids` determines which plugins are loaded for a given agent. Only plugins in this list contribute hooks, tools, and request transforms to the execution environment.

### D2: active_hook_filter controls runtime capability scoping

`AgentSpec::active_hook_filter` controls which of the loaded plugins' capabilities are active. Despite the name, this filter applies to all plugin contributions — hooks, tools, and request transforms — not just hooks. An empty filter means all loaded plugins are active.

### D3: Filtering applies at resolve time

Filtering is applied once during `ExecutionEnv::from_plugins()` when the execution environment is constructed. This happens at run start and on handoff re-resolution (ADR-0014). There is no per-phase recomputation of active plugins.

### D4: Filtering is all-or-nothing per plugin

When a plugin is filtered out by `active_hook_filter`, none of its contributions are active. There is no partial plugin state where a plugin's hooks are inactive but its tools remain available. This prevents inconsistent states where a tool is callable but the plugin's hooks that govern its behavior are not running.

## Consequences

- Disabling a plugin via `active_hook_filter` consistently removes all its contributions (hooks, tools, transforms).
- Plugin tools cannot be called if the owning plugin is filtered out.
- Activation changes require handoff or a new run, consistent with ADR-0014's resolution model.
- No runtime mutex or interior mutability needed for activation state.
