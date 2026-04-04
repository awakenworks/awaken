# 通过 SSE 暴露 HTTP

当你需要通过 HTTP + Server-Sent Events 对外提供 agent 服务，并挂上多种协议适配器（AI SDK、AG-UI、A2A、MCP）时，使用本页。

## 前置条件

- `awaken` 启用了 `server` feature
- `tokio` 启用了 `rt-multi-thread` 和 `signal`
- 已构建好一个 `AgentRuntime`

## 步骤

1. 添加依赖：

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "...", features = ["server"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
```

2. 构建 runtime：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::{AgentRuntimeBuilder, AgentSpec, ModelSpec};
use awaken::stores::InMemoryStore;

let store = Arc::new(InMemoryStore::new());

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(
        AgentSpec::new("assistant")
            .with_model("claude-sonnet")
            .with_system_prompt("You are a helpful assistant."),
    )
    .with_tool("search", Arc::new(SearchTool))
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model("claude-sonnet", ModelSpec {
        id: "claude-sonnet".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-20250514".into(),
    })
    .with_thread_run_store(store.clone())
    .build()?;

let runtime = Arc::new(runtime);
```

3. 创建应用状态：

```rust,ignore
use awaken::server::app::{AppState, ServerConfig};
use awaken::server::mailbox::{Mailbox, MailboxConfig};
use awaken::stores::InMemoryMailboxStore;

let mailbox_store = Arc::new(InMemoryMailboxStore::new());
let mailbox = Arc::new(Mailbox::new(
    runtime.clone(),
    mailbox_store,
    "default-consumer".to_string(),
    MailboxConfig::default(),
));

let state = AppState::new(
    runtime.clone(),
    mailbox,
    store,
    runtime.resolver_arc(),
    ServerConfig::default(),
);
```

4. 构建 router：

```rust,ignore
use awaken::server::routes::build_router;

let app = build_router().with_state(state);
```

`build_router()` 会注册：

- `/health`
- `/v1/threads`
- `/v1/runs`
- `/v1/config/*` 和 `/v1/capabilities`
- AI SDK v6、AG-UI、A2A、MCP 路由

5. 启动服务：

```rust,ignore
let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
axum::serve(listener, app).await?;
```

6. 配置 SSE buffer：

```rust,ignore
let config = ServerConfig {
    address: "0.0.0.0:8080".into(),
    sse_buffer_size: 128,
    ..ServerConfig::default()
};
```

## 验证

```bash
curl http://localhost:3000/health
```

应返回 `200 OK`。然后可以创建 thread 并启动 run。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 端口已占用 | 3000 已被其他进程使用 | 改 `ServerConfig.address` 或 `TcpListener::bind` |
| SSE 立即断开 | 客户端不支持 `text/event-stream` | 用 `curl --no-buffer` 或标准 SSE 客户端 |
| 路由缺失 | 没有启用 `server` feature | 确保 `awaken` 开启 `features = ["server"]` |

## 相关示例

`crates/awaken-server/tests/run_api.rs`

## 关键文件

- `crates/awaken-server/src/app.rs`
- `crates/awaken-server/src/routes.rs`
- `crates/awaken-server/src/http_sse.rs`
- `crates/awaken-server/src/mailbox.rs`

## 相关

- [构建 Agent](./build-an-agent.md)
- [使用文件存储](./use-file-store.md)
- [使用 Postgres 存储](./use-postgres-store.md)
