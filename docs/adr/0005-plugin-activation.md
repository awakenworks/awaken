# ADR-0005: Plugin Activation

- **Status**: Superseded by ADR-0009
- **Date**: 2026-03-21
- **Depends on**: ADR-0004

## Context

All installed plugins' hooks run for every `run_phase`. Need per-agent activation: "agent A uses [permission, reminder], agent B uses [permission, mcp]."

## Decision

**Live activation sources**: plugin activation is derived from the same live configuration system described in ADR-0004. `ConfigStore` carries the current activation baseline, while `AgentRegistry` provides named `AgentProfile`s. State may redirect activation by selecting a different active profile (for example, handoff).

**Per-agent hook filtering**: GATHER filters hooks by the effective `active_plugins` resolved at the current boundary. EXECUTE does not filter action handlers — they are global capabilities. State keys remain globally registered data, while activation controls which plugins contribute behavior at a given boundary. Semantic split: hooks = "what should this agent do now", handlers = "what can the system do", keys = "what data exists."

**Hook ordering not required**: In the gather-then-execute model (ADR-0001), hooks read the same snapshot and produce independent `StateCommand`s. Cross-plugin coordination flows through the action queue, not hook execution order. Topological sorting can be added later via `after`/`before` constraints on `register_phase_hook()` without changing the `Plugin` trait.

## Consequences

- Profile switching is immediate for the next execution boundary via `ConfigStore` mutation or deferred via `ActiveProfileOverride` state key (ADR-0004)
- No heavy run-wide resolve chain is needed — plugins stay installed, while activation is recomputed from live sources at each boundary
- Handoff-style agent switching is modeled as activation/config selection, not plugin reinstall or runtime rebuild
