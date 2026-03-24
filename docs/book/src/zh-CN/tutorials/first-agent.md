# 第一个 Agent

> 本文档为中文翻译版本，英文原版请参阅 [First Agent](../../tutorials/first-agent.md)

## 目标

端到端地运行一个 Agent，并确认能收到完整的事件流。

## 前置条件

```toml
[dependencies]
tirea = "0.5.0"
tirea-agentos-server = "0.5.0"
tirea-store-adapters = "0.5.0"
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
futures = "0.3"
serde_json = "1"
```

运行前，请先设置模型服务商的密钥：

```bash
# OpenAI-compatible models (for gpt-4o-mini)
export OPENAI_API_KEY=<your-key>

# Or DeepSeek models
export DEEPSEEK_API_KEY=<your-key>
```

## 1. 创建 `src/main.rs`

```rust,ignore
use futures::StreamExt;
use serde_json::{json, Value};
use tirea::contracts::{AgentEvent, Message, RunOrigin, RunRequest, ToolCallContext};
use tirea::composition::{tool_map, AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea::prelude::*;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input")
            .with_parameters(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "text": text })))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
let os = AgentOsBuilder::new()
    .with_tools(tool_map([EchoTool]))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("gpt-4o-mini")
            .with_system_prompt("You are a helpful assistant.")
            .with_allowed_tools(vec!["echo".to_string()]),
    ))
    .build()?;

    let run = os
        .run_stream(RunRequest {
            agent_id: "assistant".to_string(),
            thread_id: Some("thread-1".to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::default(),
            state: None,
            messages: vec![Message::user("Say hello using the echo tool")],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        })
        .await?;

    let events: Vec<_> = run.events.collect().await;
    println!("events: {}", events.len());

    let finished = events.iter().any(|e| matches!(e, AgentEvent::RunFinish { .. }));
    println!("run_finish_seen: {}", finished);

    Ok(())
}
```

## 2. 运行

```bash
cargo run
```

## 3. 验证

预期输出包含：

- `events: <n>`，其中 `n > 0`
- `run_finish_seen: true`

## 你创建了什么

本示例在进程内创建一个 `AgentOs`，并立即执行一次请求。

这意味着该 Agent 已经可以通过三种方式使用：

1. 在你自己的 Rust 应用代码中直接调用 `os.run_stream(...)`。
2. 以本地 CLI 风格的二进制程序运行，使用 `cargo run`。
3. 将同一个 `AgentOs` 挂载到 HTTP 服务器，供浏览器或远程客户端调用。

本教程演示选项 1 和 2。生产环境的集成通常会转向选项 3。

## 创建之后如何使用

你实际操作的核心对象是：

```rust,ignore
let os = AgentOsBuilder::new()
    .with_tools(tool_map([EchoTool]))
    .with_agent_spec(...)
    .build()?;
```

之后，常规入口为：

```rust,ignore
let run = os.run_stream(RunRequest { ... }).await?;
```

常见使用模式：

- 一次性 CLI 程序：构造 `RunRequest`，收集事件，打印结果
- 应用服务：将 `os.run_stream(...)` 封装在你自己的业务逻辑中
- HTTP 服务器：将 `Arc<AgentOs>` 存入应用状态，并暴露协议路由

## 如何启动

在本教程中，二进制入口为 `main()`，因此启动方式非常简单：

```bash
cargo run
```

如果 Agent 位于工作区内的某个包中，请使用：

```bash
cargo run -p your-package-name
```

启动成功后，你的进程将：

- 构建工具注册表
- 注册 Agent 定义
- 发送一次 `RunRequest`
- 流式接收事件直至完成
- 退出

因此，本教程是一个可运行的冒烟测试，而非长期运行的服务进程。

## 如何将其转换为服务器

若要通过 HTTP 暴露同一个 Agent，保留 `AgentOsBuilder` 的配置，并将其移入服务器状态：

```rust,ignore
use std::sync::Arc;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadStore};
use tirea_agentos_server::service::{AppState, MailboxService};
use tirea_agentos_server::{http, protocol};
use tirea_store_adapters::FileStore;

let file_store = Arc::new(FileStore::new("./sessions"));
let agent_os = AgentOsBuilder::new()
    .with_tools(tool_map([EchoTool]))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("gpt-4o-mini")
            .with_system_prompt("You are a helpful assistant.")
            .with_allowed_tools(vec!["echo".to_string()]),
    ))
    .with_agent_state_store(file_store.clone() as Arc<dyn ThreadStore>)
    .build()?;

let os = Arc::new(agent_os);
let read_store: Arc<dyn ThreadReader> = file_store.clone();
let mailbox_store: Arc<dyn MailboxStore> = file_store;
let mailbox_svc = Arc::new(MailboxService::new(os.clone(), mailbox_store, "my-agent"));

let app = axum::Router::new()
    .merge(http::health_routes())
    .merge(http::thread_routes())
    .merge(http::run_routes())
    .nest("/v1/ag-ui", protocol::ag_ui::http::routes())
    .nest("/v1/ai-sdk", protocol::ai_sdk_v6::http::routes())
    .with_state(AppState::new(os, read_store, mailbox_svc));
```

然后使用 Axum 监听器启动服务器，而不是直接调用 `run_stream(...)`。

## 下一步阅读

根据你的需求选择下一篇文档：

- 继续从 Rust 代码调用 Agent：[构建一个 Agent](../../how-to/build-an-agent.md)
- 将 Agent 暴露给浏览器或远程客户端：[暴露 HTTP SSE](../../how-to/expose-http-sse.md)
- 接入 AI SDK 或 CopilotKit：[集成 AI SDK 前端](../../how-to/integrate-ai-sdk-frontend.md) 和 [集成 CopilotKit (AG-UI)](../../how-to/integrate-copilotkit-ag-ui.md)

## 常见错误

- 模型与服务商不匹配：`gpt-4o-mini` 需要配置兼容 OpenAI 风格的服务商。
- 密钥缺失：在执行 `cargo run` 前，请先设置 `OPENAI_API_KEY` 或 `DEEPSEEK_API_KEY`。
- 工具未被选用：请确保提示词中明确要求使用 `echo` 工具。

## 后续

- [第一个工具](./first-tool.md)
- [构建一个 Agent](../../how-to/build-an-agent.md)
- [暴露 HTTP SSE](../../how-to/expose-http-sse.md)
- [事件参考](../../reference/events.md)
