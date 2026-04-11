# ADR-0014: Agent Spec Resolution Model

- **Status**: Accepted
- **Date**: 2026-03-27
- **Supersedes**: ADR-0004, ADR-0009
- **Depends on**: ADR-0001, ADR-0002, ADR-0006

## Context

ADR-0004 introduced `ConfigStore` with live configuration resolution at every execution boundary. ADR-0009 refined this with `AgentProfile` and per-phase boundary resolution. In practice, the implementation converged on a simpler model: a data-only `AgentSpec` resolved once at run start, with re-resolution only on handoff. A persisted `ConfigStore` may exist for configuration management APIs, but runtime execution does not resolve from live config sources at every phase boundary.

## Decision

### D1: AgentSpec is the sole agent configuration carrier

`AgentSpec` is a data-only, serializable struct that carries all agent configuration: model, system prompt, tools, plugin ids, active hook filter, and plugin-specific sections. It is not a live reference to a registry — it is a resolved snapshot.

### D2: Resolve once at run start

When a run begins, the runtime binds the current registry snapshot and resolves the initial `AgentSpec` from that snapshot. `PhaseContext` receives `Arc<AgentSpec>` for the duration of the run. Plugins read `ctx.agent_spec` for configuration values.

### D3: Re-resolve at step boundaries on handoff

Re-resolution occurs at step boundaries only when `ActiveAgentIdKey` indicates the active agent has changed (handoff). The loop runner checks the state key after each step completes. If the agent id differs, the new `AgentSpec` is resolved from the registry before the next step begins. No re-resolution occurs at `BeforeInference`, `BeforeToolExecute`, or other mid-step phases.

### D4: No per-phase live config resolution

A persisted `ConfigStore` may exist to hold serializable `AgentSpec` / `ModelBindingSpec` / `ProviderSpec` / `McpServerSpec` documents, but runtime execution does not query it directly at every boundary. Config changes are compiled into versioned registry snapshots; new runs see the latest published snapshot, while active runs and in-flight steps keep their pinned snapshot. Dynamic config changes still require either a handoff (agent switch) or a new run to observe different agent specs.

## Consequences

- Plugins access configuration via `ctx.agent_spec` — a single, consistent snapshot for the entire step.
- Config management APIs can persist new specs and publish a new registry snapshot without changing the per-step execution model.
- Dynamic configuration changes mid-step are not possible; this simplifies reasoning about what config is active.
- Handoff remains the mechanism for switching agent configuration without resetting runtime state.
- The `AgentRegistry` stores named `AgentSpec` definitions for lookup during resolution.
