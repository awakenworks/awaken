# 集成 CopilotKit（AG-UI）

当你有一个 CopilotKit React 前端，并想通过 AG-UI 协议接入 awaken agent server 时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `server`
- Node.js 项目中已安装 `@copilotkit/react-core` 和 `@copilotkit/react-ui`

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["server"] }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
tracing-subscriber = "0.3"
```

## 步骤

1. 启动后端 server：

```rust,ignore
use std::sync::Arc;

use awaken::engine::GenaiExecutor;
use awaken::contract::storage::ThreadRunStore;
use awaken::registry_spec::{AgentSpec, ModelSpec};
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};
use awaken::AgentRuntimeBuilder;
use awaken::server::app::{AppState, ServerConfig};
use awaken::server::mailbox::{Mailbox, MailboxConfig};
use awaken::server::routes::build_router;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(true).init();

    let agent_spec = AgentSpec::new("copilotkit-agent")
        .with_model("gpt-4o-mini")
        .with_system_prompt(
            "You are a CopilotKit-powered assistant. \
             Update shared state and suggest actions when appropriate.",
        )
        .with_max_rounds(10);

    let runtime = AgentRuntimeBuilder::new()
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model(
            "gpt-4o-mini",
            ModelSpec {
                id: "gpt-4o-mini".into(),
                provider: "openai".into(),
                model: "gpt-4o-mini".into(),
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
        format!("copilotkit:{}", std::process::id()),
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server crashed");
}
```

服务器会自动在以下路径注册 AG-UI 路由：

AG-UI 路由包括：

- `POST /v1/ag-ui/run`
- `POST /v1/ag-ui/threads/:thread_id/runs`
- `POST /v1/ag-ui/agents/:agent_id/runs`
- `POST /v1/ag-ui/threads/:thread_id/interrupt`
- `GET /v1/ag-ui/threads/:id/messages`

2. 安装 CopilotKit：

```bash
npm install @copilotkit/react-core @copilotkit/react-ui
```

3. 用 `CopilotKit` provider 包裹应用：

```tsx
import { CopilotKit } from "@copilotkit/react-core";
import { CopilotChat } from "@copilotkit/react-ui";
import "@copilotkit/react-ui/styles.css";

export default function App() {
  return (
    <CopilotKit runtimeUrl="http://localhost:3000/v1/ag-ui">
      <CopilotChat
        labels={{ title: "Agent", initial: "How can I help?" }}
      />
    </CopilotKit>
  );
}
```

4. 分别启动后端和前端。

## 验证

1. 打开页面
2. 在 CopilotChat 中发送消息
3. 确认聊天 UI 中有流式回复
4. 查看后端日志里的 `RunStart` / `RunFinish`

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 浏览器 CORS 错误 | 未配置 CORS 中间件 | 给 axum router 加 CORS |
| CopilotKit 提示 connection failed | `runtimeUrl` 错了 | 指向 `http://localhost:3000/v1/ag-ui` |
| 有事件但 UI 不更新 | AG-UI 事件格式不兼容 | 确认 CopilotKit 版本匹配 |
| `/v1/ag-ui/run` 返回 404 | 没开 `server` feature | 在 `Cargo.toml` 里启用 |

## 相关示例

- `examples/copilotkit-starter/agent/src/main.rs`

## 关键文件

| 路径 | 作用 |
|------|------|
| `crates/awaken-server/src/protocols/ag_ui/http.rs` | AG-UI 路由 |
| `crates/awaken-server/src/protocols/ag_ui/encoder.rs` | AG-UI encoder |
| `crates/awaken-server/src/routes.rs` | 总路由 |
| `crates/awaken-server/src/app.rs` | `AppState` / `ServerConfig` |
| `examples/copilotkit-starter/agent/src/main.rs` | CopilotKit starter 后端入口 |

## 相关

- [通过 SSE 暴露 HTTP](./expose-http-sse.md)
- [AG-UI 协议](../reference/protocols/ag-ui.md)
- [集成 AI SDK 前端](./integrate-ai-sdk-frontend.md)
