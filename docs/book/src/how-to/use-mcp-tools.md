# Use MCP Tools

Use this when you want to expose MCP server tools as regular agent tools.

## Prerequisites

- `tirea-extension-mcp` dependency is available.
- `mcp = { package = "model-context-protocol", version = "0.2", default-features = false, features = ["client"] }` in your `Cargo.toml`.
- One or more reachable MCP servers.
- Runtime uses Tokio.

## Steps

1. Build MCP server configs.

```rust,ignore
use mcp::transport::McpServerConnectionConfig;

let cfg = McpServerConnectionConfig::stdio(
    "mcp_demo",
    "python3",
    vec!["-u".to_string(), "./mcp_server.py".to_string()],
);
```

2. Connect MCP registry manager and fetch tool snapshot.

```rust,ignore
use tirea::extensions::mcp::McpToolRegistryManager;

let manager = McpToolRegistryManager::connect([cfg]).await?;
let mcp_tools = manager.registry().snapshot();
```

3. Merge MCP tools into your tool map and build AgentOS.

```rust,ignore
use std::collections::HashMap;
use std::sync::Arc;
use tirea::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea::contracts::runtime::tool_call::Tool;

let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
// add your native tools first...

tools.extend(mcp_tools);

let os = AgentOsBuilder::new()
    .with_tools(tools)
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("deepseek-chat"),
    ))
    .build()?;
```

4. Keep `manager` alive for refresh lifecycle.

Optional refresh controls:

```rust,ignore
manager.refresh().await?;
manager.start_periodic_refresh(std::time::Duration::from_secs(30))?;
// shutdown path:
let _stopped = manager.stop_periodic_refresh().await;
```

## Verify

- `manager.registry().ids()` includes MCP tool ids.
- Tool execution result contains MCP metadata (`mcp.server`, `mcp.tool`).
- If MCP tool provides UI resource, result metadata includes `mcp.ui.resourceUri` and UI content fields.

## Common Errors

- Duplicate MCP server name in configs.
- Duplicate tool id conflict when merging with existing tool map.
- Periodic refresh started without Tokio runtime.

## Related Example

- `examples/ai-sdk-starter/README.md` can surface MCP tool cards when the starter backend is run with `MCP_SERVER_CMD`

## Key Files

- `crates/tirea-extension-mcp/src/lib.rs`
- `crates/tirea-extension-mcp/src/client_transport.rs`
- `examples/src/starter_backend/mod.rs`

## Related

- [Capability Matrix](../reference/capability-matrix.md)
- [Expose HTTP SSE](./expose-http-sse.md)
- `examples/src/starter_backend/mod.rs`
