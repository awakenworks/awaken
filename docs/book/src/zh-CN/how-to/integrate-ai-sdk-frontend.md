# 集成 AI SDK 前端

当你有一个基于 Vercel AI SDK v6 的 React 前端，并希望把它接到 awaken agent server 上时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `server`
- Node.js 项目中已安装 `@ai-sdk/react`

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["server"] }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
tracing-subscriber = "0.3"
```

## 步骤

1. 先启动后端 server：

```rust,ignore
use std::sync::Arc;

use awaken::engine::GenaiExecutor;
use awaken::contract::storage::ThreadRunStore;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};
use awaken::AgentRuntimeBuilder;
use awaken::server::app::{AppState, ServerConfig};
use awaken::server::mailbox::{Mailbox, MailboxConfig};
use awaken::server::routes::build_router;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(true).init();

    let agent_spec = AgentSpec::new("my-agent")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant.")
        .with_max_rounds(10);

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
        .build()
        .expect("failed to build runtime");
    let runtime = Arc::new(runtime);

    let store = Arc::new(InMemoryStore::new());
    let resolver = runtime.resolver_arc();

    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store as Arc<dyn awaken::contract::MailboxStore>,
        format!("ai-sdk:{}", std::process::id()),
        MailboxConfig::default(),
    ));

    let state = AppState::new(
        runtime,
        mailbox,
        store as Arc<dyn ThreadRunStore>,
        resolver,
        ServerConfig {
            address: "127.0.0.1:3000".into(),
            ..Default::default()
        },
    );

    let app = build_router().with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

AI SDK v6 相关路由：

- `POST /v1/ai-sdk/chat`
- `GET /v1/ai-sdk/chat/:thread_id/stream`
- `GET /v1/ai-sdk/threads/:thread_id/stream`
- `GET /v1/ai-sdk/threads/:id/messages`

2. 安装前端依赖：

```bash
npm install ai @ai-sdk/react
```

3. 在前端里使用 `useChat`：

```tsx
import { useChat } from "@ai-sdk/react";

export default function Chat() {
  const { messages, input, handleInputChange, handleSubmit } = useChat({
    api: "http://localhost:3000/v1/ai-sdk/chat",
    id: "thread-1",
  });

  return (
    <div>
      {messages.map((m) => (
        <div key={m.id}>
          <strong>{m.role}:</strong> {m.content}
        </div>
      ))}
      <form onSubmit={handleSubmit}>
        <input value={input} onChange={handleInputChange} />
        <button type="submit">Send</button>
      </form>
    </div>
  );
}
```

4. 分别启动后端和前端。

## 验证

1. 打开前端页面
2. 发送一条消息
3. 确认文本是流式出现的
4. 确认后端日志中出现 `RunStart` / `RunFinish`

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 浏览器 CORS 错误 | 未配置 CORS 中间件 | 给 axum router 加 `tower-http` CORS |
| `useChat` 收不到事件 | URL 配错 | 确认 `api` 指向 `/v1/ai-sdk/chat` |
| `stream closed unexpectedly` | SSE 缓冲溢出 | 增大 `ServerConfig.sse_buffer_size` |
| `/v1/ai-sdk/chat` 返回 404 | 没开 `server` feature | 在 `Cargo.toml` 里启用 |

## 相关示例

- `examples/ai-sdk-starter/agent/src/main.rs`

## 关键文件

| 路径 | 作用 |
|------|------|
| `crates/awaken-server/src/protocols/ai_sdk_v6/http.rs` | AI SDK v6 路由 |
| `crates/awaken-server/src/protocols/ai_sdk_v6/encoder.rs` | AI SDK v6 SSE encoder |
| `crates/awaken-server/src/routes.rs` | 总路由 |
| `crates/awaken-server/src/app.rs` | `AppState` / `ServerConfig` |
| `examples/ai-sdk-starter/agent/src/main.rs` | AI SDK starter 后端入口 |

## 相关

- [通过 SSE 暴露 HTTP](./expose-http-sse.md)
- [AI SDK v6 协议](../reference/protocols/ai-sdk-v6.md)
- [集成 CopilotKit (AG-UI)](./integrate-copilotkit-ag-ui.md)
