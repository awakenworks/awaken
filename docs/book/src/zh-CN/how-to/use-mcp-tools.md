# 使用 MCP Tools

当你想连接外部 Model Context Protocol（MCP）server，并把它们的工具暴露给 awaken agent 时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `mcp`
- 有一个可连接的 MCP server（stdio 或 HTTP/SSE）

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["mcp"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 配置 MCP server 连接：

```rust,ignore
use awaken::ext_mcp::McpServerConnectionConfig;

let stdio_config = McpServerConnectionConfig::stdio(
    "my-mcp-server",
    "node",
    vec!["server.js".into()],
);

let http_config = McpServerConnectionConfig::http(
    "remote-server",
    "http://localhost:8080/sse",
);
```

2. 创建 registry manager 并发现工具：

```rust,ignore
use awaken::ext_mcp::McpToolRegistryManager;

let manager = McpToolRegistryManager::connect(vec![stdio_config, http_config])
    .await
    .expect("failed to connect MCP servers");

let registry = manager.registry();
for id in registry.ids() {
    println!("discovered: {id}");
}
```

MCP 工具的 ID 格式通常是 `mcp__{server}__{tool}`。

3. 把工具注册进 runtime：

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

`mcp` 插件会在 agent 解析时 snapshot 当前 MCP registry，并把发现到的工具作为
plugin tools 注册进去。`plugin_ids` 中必须包含 `"mcp"`，这些工具才会加载到该
agent。

4. 如有需要，开启周期性刷新：

```rust,ignore
use std::time::Duration;

manager.start_periodic_refresh(Duration::from_secs(60));
```

周期刷新会更新 manager registry。新的 run 会重新解析 agent 并看到最新 snapshot；
正在运行中的 run 保持其解析时的工具集。

## 验证

1. 运行 agent，并让它调用来自 MCP server 的工具
2. 检查后端日志里的 MCP 工具调用
3. 返回结果中应带有 `mcp.server` 与 `mcp.tool` 元数据

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `McpError::TransportError` | MCP server 未启动或不可达 | 检查进程和 URL / 命令 |
| 没发现任何工具 | server 返回空工具列表 | 确认 server 实现了 `tools/list` |
| 调用超时 | server 响应太慢 | 调大 transport timeout |
| feature 不存在 | 没开 cargo feature | 启用 `mcp` |
| 找不到 `mcp__server__tool` | agent 未加载 MCP 插件或未发现工具 | 在 `plugin_ids` 中加入 `"mcp"`，注册 `McpPlugin::new(manager.registry())`，并检查 discovery |

## 相关示例

- `crates/awaken-ext-mcp/tests/`

## 关键文件

- `crates/awaken-ext-mcp/src/lib.rs`
- `crates/awaken-ext-mcp/src/manager.rs`
- `crates/awaken-ext-mcp/src/config.rs`
- `crates/awaken-ext-mcp/src/plugin.rs`
- `crates/awaken-ext-mcp/src/transport.rs`
- `crates/awaken-ext-mcp/tests/mcp_tests.rs`

## 相关

- [添加 Tool](./add-a-tool.md)
- [添加 Plugin](./add-a-plugin.md)
- [使用 Skills 子系统](./use-skills-subsystem.md)
