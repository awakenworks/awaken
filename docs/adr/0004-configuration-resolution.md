# ADR-0004: Configuration Resolution

- **Status**: Not Implemented
- **Date**: 2026-03-21
- **Depends on**: ADR-0001, ADR-0002

## Context

Plugins need typed configuration (permission rules, MCP server URLs, model name) set from outside the runtime. Configuration differs from state: whole-replacement, no revision tracking, sourced externally. However, some config changes (handoff, agent/profile switching, dynamic model overrides) originate from hooks that can only write state. The system also needs to support live reads of the latest profile/config sources without forcing the entire run to re-resolve a long-lived runtime object.

## Decision

**`ConfigSlot` trait**: Parallel to `StateSlot` but without `Update`/`apply`. Whole-replacement values in a `ConfigMap` (separate `TypedMap` with distinct marker). Registered via `PluginRegistrar::register_config::<C>()`.

**Live configuration sources**:

- `AgentRegistry` stores named `AgentProfile`s and allows dynamic lookups by id
- `ConfigStore` stores OS defaults, current selection state, and runtime/request overrides
- state and memory can influence configuration indirectly by selecting a profile or producing plugin-specific overlays

These sources are mutable at runtime and may be queried at any time. They are the source of truth; the runtime must not depend on a long-lived `ResolvedRun` object.

**Multi-source resolution** happens at execution boundaries, in precedence order:

1. `RunOverrides` — per-call, not persisted
2. State-driven selection and overlays — e.g. `ActiveProfileOverride`, handoff state, plugin-produced overlays
3. Active selection in `ConfigStore` — runtime baseline, including active profile id and explicit config overrides
4. Active profile config — looked up dynamically from `AgentRegistry`
5. `OsConfig.defaults` — global defaults
6. `C::Value::default()` — type default

Hooks can trigger profile switches by writing `ActiveProfileOverride` (a built-in `StateSlot`); the next boundary resolves it.

**Resolve at boundary**: the runtime resolves a short-lived execution view at the start of each execution boundary rather than once for the whole run. The minimum boundaries are:

- `run_phase`
- `before_inference`
- `before_tool_execute`
- action/effect dispatch

Each boundary gets an immutable execution-local view (`ExecutionConfig` / `ConfigSnapshot`) so all code within that boundary observes a consistent configuration version, while later boundaries can see newer live config.

**No long-lived `ResolvedRun`**: the system does not keep a single fully resolved runtime object for the whole run. Profile switching, handoff, and request-level model overrides should take effect at the next boundary without rebuilding the whole runtime.

**`ConfigView` deferred**: Cross-source merge projections (config + state + memory → effective value) are plugin-defined functions for now. A framework-level trait will be introduced when repeated patterns emerge.

## Consequences

- `PhaseContext` and later inference/tool contexts gain execution-local config accessors backed by a boundary-local resolved view
- Runtime needs `AgentRegistry` + `ConfigStore` as live config sources
- `PhaseHookRegistration` gains `plugin_id: String` for activation filtering
- Handoff and similar mechanisms become ordinary state-driven config selectors instead of special runtime rebuild paths
