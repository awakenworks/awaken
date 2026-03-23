# ADR-0013: Extension Organization and Plugin Tool Registration

- **Status**: Accepted
- **Date**: 2026-03-27
- **Depends on**: ADR-0010

## Context

Extensions (`awaken-ext-*` crates and runtime built-in extensions) had inconsistent organization:
- Some used flat layout (`plugin.rs`), others nested (`plugin/plugin.rs`), others non-standard (`a2ui/plugin.rs`)
- Tools were registered outside the Plugin system via builder `with_tool()`, requiring callers to wire both plugins and tools separately
- No standard for where hooks, state keys, and tools should live within an extension crate

## Decisions

### D1: Plugin-scoped tool registration

`PluginRegistrar` supports `register_tool(id, tool)`. Tools registered this way are **per-agent-spec scoped** — only available to agents whose `plugin_ids` include the registering plugin.

Three tool sources exist, merged during resolve with conflict detection:
1. **Global tools** — registered via builder `with_tool()`, available to all agents
2. **Plugin tools** — registered via `Plugin::register()` → `register_tool()`, scoped to plugin activation
3. **Dynamic tools** — injected during resolve (e.g., A2A delegate tools from `spec.delegates`)

Tool ID conflicts across any source produce `ResolveError::ToolIdConflict`. After merge, `allowed_tools`/`excluded_tools` filtering applies uniformly to all sources.

### D2: Recommended extension crate layout

```
awaken-ext-xxx/
  src/
    lib.rs              # Public API exports
    plugin/
      mod.rs            # Plugin struct + Plugin trait impl
      hooks.rs          # PhaseHook implementations (if any)
      tests.rs          # Plugin-level tests (#[cfg(test)])
    tools/
      mod.rs            # Tool implementations (or tools.rs if single tool)
    state.rs            # State key definitions (if any)
    types.rs            # Domain types
    error.rs            # Error types (if any)
```

When an extension has only one tool, `tools.rs` (single file) is preferred over a `tools/` directory.

### D3: Self-contained plugins

Plugins should be self-contained: all tools, hooks, state keys, and permission checkers are registered in `Plugin::register()`. Callers only need `builder.with_plugin(id, plugin)` — no separate tool wiring.

Exceptions:
- **Dynamic tools** (e.g., A2A delegates) that depend on resolve-time information remain injected during resolve
- **Global tools** that are independent of any plugin continue to use `builder.with_tool()`

## Consequences

- Extension authors register tools in `Plugin::register()` rather than exposing them for external wiring
- `SkillDiscoveryPlugin` registers all 3 skill tools; `A2uiPlugin` registers its render tool
- `SkillSubsystem.tools()` and `extend_tools()` removed (tools are plugin-internal)
- `allowed_tools`/`excluded_tools` filtering covers both global and plugin tools uniformly
