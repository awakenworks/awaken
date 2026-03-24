# ADR-0014: Agent Spec Resolution Model

- **Status**: Accepted
- **Date**: 2026-03-27
- **Supersedes**: ADR-0004, ADR-0009
- **Depends on**: ADR-0001, ADR-0002, ADR-0006

## Context

ADR-0004 introduced `ConfigStore` with live configuration resolution at every execution boundary. ADR-0009 refined this with `AgentProfile` and per-phase boundary resolution. In practice, the implementation converged on a simpler model: a data-only `AgentSpec` resolved once at run start, with re-resolution only on handoff. There is no `ConfigStore`, no `AgentProfile`, and no per-phase live resolution.

## Decision

### D1: AgentSpec is the sole agent configuration carrier

`AgentSpec` is a data-only, serializable struct that carries all agent configuration: model, system prompt, tools, plugin ids, active hook filter, and plugin-specific sections. It is not a live reference to a registry — it is a resolved snapshot.

### D2: Resolve once at run start

`AgentSpec` is resolved once when a run begins. `PhaseContext` receives `Arc<AgentSpec>` for the duration of the run. Plugins read `ctx.agent_spec` for configuration values.

### D3: Re-resolve at step boundaries on handoff

Re-resolution occurs at step boundaries only when `ActiveAgentIdKey` indicates the active agent has changed (handoff). The loop runner checks the state key after each step completes. If the agent id differs, the new `AgentSpec` is resolved from the registry before the next step begins. No re-resolution occurs at `BeforeInference`, `BeforeToolExecute`, or other mid-step phases.

### D4: No ConfigStore or live config resolution

There is no `ConfigStore`, no `ConfigSlot`, no `ConfigMap`. Configuration is not resolved from multiple live sources at every boundary. Dynamic config changes require either a handoff (agent switch) or a new run.

## Consequences

- Plugins access configuration via `ctx.agent_spec` — a single, consistent snapshot for the entire step.
- Dynamic configuration changes mid-step are not possible; this simplifies reasoning about what config is active.
- Handoff remains the mechanism for switching agent configuration without resetting runtime state.
- The `AgentRegistry` stores named `AgentSpec` definitions for lookup during resolution.
