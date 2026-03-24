# ADR-0009: Configuration, State, and Profile Switch

> **Status: Superseded** — see ADR-0014, ADR-0015

- **Status**: Superseded by ADR-0014, ADR-0015
- **Date**: 2026-03-22
- **Supersedes**: ADR-0004, ADR-0005
- **Depends on**: ADR-0001, ADR-0002, ADR-0008

## Context

The framework needs a mechanism that supports handoff (agent config switch while preserving runtime state), skill temporary permission elevation, permission mode switching, user model overrides, and per-inference thinking adjustments — without modules knowing about each other.

Key principle: configuration (how the agent should behave) is separate from runtime state (what has happened). Handoff switches configuration; it does not reset runtime state.

## Decisions

### D1: Three input sources for plugin hooks

Every hook receives a `PhaseContext` with exactly three input sources:

| Source | Access | Mutability | Contains |
|--------|--------|-----------|----------|
| Agent Profile | `ctx.profile` | Immutable reference, switched by handoff | model, prompt, tools, permission rules, plugin sections |
| State Snapshot | `ctx.state::<S>()` | Mutable via StateCommand | permission grants, handoff state, run lifecycle, tool call states |
| Run Input | `ctx.run_input` | Immutable, set at run start | user overrides (model, thinking, permission mode), run identity |

Hooks are pure functions: `(profile, state, run_input) -> StateCommand`. Plugin `self` holds only immutable construction-time defaults (no registry, no mutex).

### D2: Profile is an immutable reference, not copied into state

`PhaseContext` holds `&AgentProfile` directly. Loop runner resolves the reference at each phase boundary by reading `ActiveAgent` (a `Option<String>` profile_id) from state and looking up the profile in the registry.

```rust
struct ActiveAgent;
impl StateKey for ActiveAgent {
    type Value = Option<String>;  // profile_id
    type Update = Option<String>;
    fn apply(v, u) { *v = u; }
}
```

Handoff = write a string. No profile data copied into state. No serialization. Plugins read `ctx.profile` to get the active agent's configuration. If the profile is hot-updated in the registry, the next boundary picks it up automatically.

### D3: Stateless plugins

Plugins hold only immutable construction-time parameters (default values, credentials, endpoints). No `Mutex`, no interior mutability. No registry references — only loop_runner holds the registry to resolve profile references.

### D4: Each module defines its own merge strategy

The framework does NOT prescribe scope priority or replacement-vs-additive. Each module decides how to combine its three inputs.

**Replacement** (model, prompt, max_rounds, thinking):
```
effective = run_input override ?? profile value ?? self.default
```

**Additive merge** (permission rules):
```
effective = merge(
    profile.rules (source:AgentProfile),
    state grants (source:Skill),
    state user rules (source:User),
)
→ evaluate: Deny > Allow > Ask
```

**Independent gate** (allowed_tools):
```
tool must pass ALL gates:
  1. profile.allowed_tools (from active agent)
  2. permission rules evaluation
```

### D5: Permission — single key, source-tagged rules

One `Permission` with source-tagged rules and selective cleanup:

```rust
struct Permission;
impl StateKey for Permission {
    type Value = PermissionState;
    type Update = PermissionUpdate;
}

struct PermissionState {
    mode: PermissionMode,
    rules: HashMap<String, PermissionRule>,
}

enum PermissionUpdate {
    SetMode(PermissionMode),
    SetRule { key: String, rule: PermissionRule },
    ClearBySource(RuleSource),
}
```

Different sources, different lifecycles, same key:
- `source: User` — persistent, user explicitly manages
- `source: AgentProfile` — handoff clears old + writes new agent's rules
- `source: Skill` — run ends, `ClearBySource(Skill)` removes all skill grants

Skill grant = `SetRule { key: "mcp_x", rule: { Allow, source: Skill } }`. Additive per-key, no read-modify-write, no restore needed.

### D6: Handoff = write profile_id, not config data

HandoffPlugin writes one string to state:

```rust
cmd.update::<ActiveAgent>(Some("reviewer".into()));
```

Loop runner detects the change at next phase boundary, resolves `&AgentProfile` from registry, puts it on `PhaseContext.profile`. All plugins see the new profile's configuration. Runtime state (permission grants, messages, step count) is untouched.

Each plugin reacts in its own hook by reading `ctx.profile`:
- PermissionPlugin: `ClearBySource(AgentProfile)` + write new agent's rules
- Other plugins: read their section from profile, update their own state if needed

HandoffPlugin does not know any other plugin's key types. It writes one string.

### D7: Per-inference overrides are actions, not state

Model override, thinking adjustment, temperature change for one inference — these flow through hook return values as effects, consumed by loop runner for one inference. Not persisted to state.

```rust
cmd.effect(RuntimeEffect::InferenceOverride {
    model: Some("gpt-4o".into()),
    thinking: Some(ThinkingConfig { enabled: false }),
})?;
```

### D8: Scope lifecycle handles cleanup

| Scope | Cleanup | Example |
|-------|---------|---------|
| Thread (persistent) | User explicit delete | User permission rules |
| Session (not persistent) | Process exit | Permission mode, model preference |
| Run | Run start | Skill grants, handoff config |
| ToolCall | Tool completion | Tool-specific state |
| Per-inference | Consumed once | InferenceOverride effect |

Session scope = Thread scope with `StateKeyOptions { persistent: false }`.

Source-based cleanup within a key: `ClearBySource(Skill)` at run start, `ClearBySource(AgentProfile)` on handoff.

## Scenarios

**Handoff**:
```
ActiveAgent: None → Some("reviewer")
ctx.profile: &default_agent → &reviewer_profile
Permission: ClearBySource(AgentProfile) + SetRule(reviewer's rules)
Runtime state (messages, grants, steps): unchanged
```

**Skill elevation**:
```
Permission += { "mcp_x": Allow(source:Skill) }  ← insert, not replace
Run ends → ClearBySource(Skill)
```

**User model override**:
```
RunInput { model_override: Some("opus") }
Hook reads → emits InferenceOverride { model: "opus" }
Per-run, not per-inference (run_input is fixed for the run)
```

**Thinking per-step**:
```
Hook returns RuntimeEffect::InferenceOverride { thinking: false }
Loop runner applies to this inference only
Next step: no override → profile default
```

## Consequences

### Superseded

- **ADR-0004**: all config resolution types removed
- **ADR-0005**: active_plugins read from profile, filtered in run_phase

### To remove from codebase

- `src/config/` module entirely
- `PhaseRuntime.os_config`, `active_config`, `configure()`, `resolve_config()`, `set_os_config()`
- `PhaseContext.config` field, `with_config()`, `config::<C>()`

### To implement

- `PhaseContext` with `profile: &AgentProfile`, `run_input: &RunInput`
- `ActiveAgent` (`Option<String>`)
- `AgentProfile` with typed sections
- `Permission` with source-tagged rules + `ClearBySource`
- `RuntimeEffect::InferenceOverride`
- `RunInput` struct (user overrides, run identity)
- Loop runner: resolve profile reference at each phase boundary

### Deferred

- `KeyScope` lifecycle automation (ADR-0008)
- Profile hot-reload
- Profile inheritance
