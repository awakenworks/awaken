# Use MCP Tools

Use this when you want to connect to external Model Context Protocol (MCP) servers and expose their tools to awaken agents.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- Feature `mcp` enabled on the `awaken` crate
- An MCP server to connect to (stdio or HTTP transport)

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["mcp"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Configure MCP server connections.

```rust,ignore
use awaken::ext_mcp::McpServerConnectionConfig;

// Stdio transport: launch a child process
let stdio_config = McpServerConnectionConfig::stdio(
    "my-mcp-server",          // server name
    "node",                    // command
    vec!["server.js".into()],  // args
);

// HTTP/SSE transport: connect to a running server
let http_config = McpServerConnectionConfig::http(
    "remote-server",
    "http://localhost:8080/sse",
);
```

2. Create the registry manager and discover tools.

```rust,ignore
use awaken::ext_mcp::McpToolRegistryManager;

let manager = McpToolRegistryManager::connect(vec![stdio_config, http_config])
    .await
    .expect("failed to connect MCP servers");

// Tools are now available as awaken Tool instances.
// Each MCP tool is registered with the ID format: mcp__{server}__{tool}
let registry = manager.registry();
for id in registry.ids() {
    println!("discovered: {id}");
}
```

3. Register tools with the runtime.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_mcp::McpPlugin;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let mut agent_spec = AgentSpec::new("mcp-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Use MCP tools when they help answer the user.")
    .with_hook_filter("mcp");
agent_spec.plugin_ids.push("mcp".into());

let mcp_registry = manager.registry();
let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding(
        "gpt-4o-mini",
        ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        },
    )
    .with_agent_spec(agent_spec)
    .with_plugin("mcp", Arc::new(McpPlugin::new(mcp_registry)) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

The `mcp` plugin snapshots the current MCP registry during agent resolution and
registers the discovered tools as plugin tools. `plugin_ids` must contain
`"mcp"` for those tools to be loaded for the agent.

4. Enable periodic refresh (optional).

   MCP servers may add or remove tools at runtime. Enable periodic refresh to keep the tool registry in sync:

```rust,ignore
use std::time::Duration;

manager.start_periodic_refresh(Duration::from_secs(60));
```

Periodic refresh updates the manager registry. New runs resolve the agent again
and see the latest snapshot; in-flight runs keep the tool set they resolved with.

## Verify

1. Run the agent and ask it to use a tool provided by the MCP server.
2. Check the backend logs for MCP tool call events.
3. Confirm the tool result includes `mcp.server` and `mcp.tool` metadata in the response.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `McpError::TransportError` | MCP server not running or unreachable | Verify the server process is running and the path/URL is correct |
| No tools discovered | Server returned empty tool list | Check the MCP server implements `tools/list` |
| Tool call timeout | Server too slow to respond | Increase timeout in the transport configuration |
| Feature not found | Missing cargo feature | Enable `features = ["mcp"]` in `Cargo.toml` |
| `mcp__server__tool` not found | MCP plugin not loaded for the agent or no tools discovered | Add `"mcp"` to `plugin_ids`, register `McpPlugin::new(manager.registry())`, and verify discovery |

## Related Example

- `crates/awaken-ext-mcp/tests/`

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-mcp/src/lib.rs` | Module root and public re-exports |
| `crates/awaken-ext-mcp/src/manager.rs` | `McpToolRegistryManager` lifecycle and tool wrapping |
| `crates/awaken-ext-mcp/src/config.rs` | `McpServerConnectionConfig` transport types |
| `crates/awaken-ext-mcp/src/plugin.rs` | `McpPlugin` integration with awaken plugin system |
| `crates/awaken-ext-mcp/src/transport.rs` | `McpToolTransport` trait and transport helpers |
| `crates/awaken-ext-mcp/tests/mcp_tests.rs` | Integration tests |

## Related

- [Add a Tool](./add-a-tool.md)
- [Add a Plugin](./add-a-plugin.md)
- [Use Skills Subsystem](./use-skills-subsystem.md)
