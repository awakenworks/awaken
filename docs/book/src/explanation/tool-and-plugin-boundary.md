# Tool and Plugin Boundary

Awaken separates user-facing capabilities (tools) from system-level lifecycle logic (plugins). This page explains the design boundary, when to use each, and how they interact.

## Tools

A tool is a capability exposed to the LLM. The LLM decides when to call it based on the tool's descriptor.

```rust,ignore
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    fn validate_args(&self, args: &Value) -> Result<(), ToolError> { Ok(()) }
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError>;
}
```

Key properties:

- **Registered by ID** -- each tool has a unique string identifier from `ToolDescriptor`.
- **LLM-visible** -- the descriptor (name, description, JSON Schema for parameters) is included in inference requests.
- **Executed in tool rounds** -- tools pass through `ToolGate`, `BeforeToolExecute`, execution, and `AfterToolExecute`.
- **Isolated** -- a tool receives only its arguments and a `ToolCallContext`. It cannot directly access other tools, the plugin system, or the phase runtime.
- **User-defined** -- application code creates tools for domain-specific actions (file operations, API calls, database queries).

## Plugins

A plugin is a system-level extension that hooks into the execution lifecycle. Plugins do not appear in the LLM's tool list.

```rust,ignore
pub trait Plugin: Send + Sync + 'static {
    fn descriptor(&self) -> PluginDescriptor;
    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError>;
    fn config_schemas(&self) -> Vec<ConfigSchema> { vec![] }
}
```

Key properties:

- **Registered by ID** -- each plugin has a unique identifier from `PluginDescriptor`.
- **LLM-invisible** -- the LLM does not know plugins exist.
- **Run in phases** -- plugin hooks fire at phase boundaries (RunStart, BeforeInference, AfterToolExecute, etc.).
- **System-wide access** -- hooks receive `PhaseContext` with access to the state snapshot, event sink, and run metadata.
- **Declarative registration** -- plugins declare their capabilities through `PluginRegistrar`.

## PluginRegistrar

When `Plugin::register` is called, the plugin uses `PluginRegistrar` to declare everything it needs:

| Registration Method | Purpose |
|---------------------|---------|
| `register_key::<K>()` | Declare a `StateKey` for the plugin's state |
| `register_tool_gate_hook()` | Add a pure tool-interception decision hook |
| `register_phase_hook()` | Add a hook that runs at a specific phase |
| `register_tool()` | Inject a tool into the runtime (plugin-provided tools) |
| `register_effect_handler()` | Handle named effects emitted by hooks |
| `register_scheduled_action()` | Handle named actions scheduled by hooks |
| `register_request_transform()` | Transform inference requests before they reach the LLM |

Plugins can register tools. This is how delegation tools (`AgentTool`) and MCP tools enter the runtime -- they are tools owned by plugins, not directly registered by user code. The LLM sees these tools identically to user-registered tools.

## When to Use a Tool

Use a tool when:

- The LLM should be able to invoke the capability by name.
- The capability performs a domain-specific action (search, compute, file I/O, API call).
- The capability takes structured input and returns a result the LLM can reason about.
- The logic does not need access to runtime internals (state, phases, other tools).

## When to Use a Plugin

Use a plugin when:

- The logic should run automatically at phase boundaries without LLM involvement.
- The logic needs to modify inference requests (system prompt injection, tool filtering).
- The logic needs to inspect or transform tool results before the LLM sees them.
- The logic manages cross-cutting concerns (permissions, observability, reminders, state synchronization).
- The logic needs to register state keys, effect handlers, or request transforms.

## Interaction Between Tools and Plugins

Plugins can influence tool execution without the tool knowing:

- **Permission plugin** -- decides at `ToolGate` whether a tool call is allowed, blocked, or suspended. The tool itself has no permission logic.
- **Observability plugin** -- wraps tool execution with OpenTelemetry spans. The tool does not emit traces.
- **Reminder plugin** -- injects context messages after specific tools execute. The tool does not know about reminders.
- **Interception pipeline** -- `ToolGate` hooks can replace tool results or skip execution entirely before any execution-time hooks run.

This separation means tools remain simple and focused on their domain logic. Cross-cutting concerns are handled uniformly by plugins, applied consistently across all tools without per-tool opt-in.

## Plugin-Provided Tools vs User Tools

| Aspect | User Tool | Plugin Tool |
|--------|-----------|-------------|
| Registration | `AgentRuntimeBuilder::with_tool()` | `PluginRegistrar::register_tool()` |
| Lifecycle | Exists for the runtime's lifetime | Exists while the plugin is active |
| Configuration | Direct construction | Derived from plugin config or agent spec |
| Examples | Custom business logic tools | `AgentTool` (delegation), MCP tools, skill tools |

Both appear identically to the LLM. The distinction is purely about ownership and lifecycle management.

## See Also

- [Tool Trait Reference](../reference/tool-trait.md)
- [Add a Tool](../how-to/add-a-tool.md)
- [Add a Plugin](../how-to/add-a-plugin.md)
- [Run Lifecycle and Phases](./run-lifecycle-and-phases.md)
