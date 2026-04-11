# 第一个 Agent

## 目标

端到端运行一个智能体，并确认你收到完整的事件流。

## 前置条件

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde_json = "1"
```

运行之前，请设置一个模型提供商的 API 密钥：

```bash
# OpenAI 兼容模型（用于 gpt-4o-mini）
export OPENAI_API_KEY=<your-key>

# 或 DeepSeek 模型
export DEEPSEEK_API_KEY=<your-key>
```

## 1. 创建 `src/main.rs`

```rust,no_run
use std::sync::Arc;
use serde_json::{json, Value};
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
use awaken::contract::message::Message;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::VecEventSink;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::AgentSpec;
use awaken::registry::ModelBinding;
use awaken::{AgentRuntimeBuilder, RunRequest};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input back to the caller")
            .with_parameters(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> Result<ToolOutput, ToolError> {
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "echoed": text })).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent_spec = AgentSpec::new("assistant")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant. Use the echo tool when asked.")
        .with_max_rounds(5);

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_tool("echo", Arc::new(EchoTool))
        .with_provider("openai", Arc::new(GenaiExecutor::new()))
        .with_model_binding("gpt-4o-mini", ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        })
        .build()?;

    let request = RunRequest::new(
        "thread-1",
        vec![Message::user("Say hello using the echo tool")],
    )
    .with_agent_id("assistant");

    let sink = Arc::new(VecEventSink::new());
    runtime.run(request, sink.clone()).await?;

    let events = sink.take();
    println!("events: {}", events.len());

    let finished = events
        .iter()
        .any(|e| matches!(e, AgentEvent::RunFinish { .. }));
    println!("run_finish_seen: {}", finished);

    Ok(())
}
```

## 2. 运行

```bash
cargo run
```

## 3. 验证

预期输出包括：

- `events: <n>`，其中 `n > 0`
- `run_finish_seen: true`

事件流将至少包含 `RunStart`、一个或多个 `TextDelta` 或 `ToolCallStart`/`ToolCallDone` 事件，以及最终的 `RunFinish`。

## 你创建了什么

本示例创建了一个进程内的 `AgentRuntime` 并立即执行一个请求。

核心对象是：

```rust,ignore
let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("echo", Arc::new(EchoTool))
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding("gpt-4o-mini", ModelBinding {
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    })
    .build()?;
```

之后，标准入口点是：

```rust,ignore
let sink = Arc::new(VecEventSink::new());
runtime.run(request, sink.clone()).await?;
let events = sink.take();
```

常见使用模式：

- 一次性 CLI 程序：构造 `RunRequest`，通过 `VecEventSink` 收集事件，打印结果
- 应用服务：将 `runtime.run(...)` 封装在你自己的应用逻辑中
- HTTP 服务器：将 `Arc<AgentRuntime>` 存储在应用状态中，暴露协议路由

## 下一步阅读

根据你的需求选择下一页：

- 添加类型化状态和有状态工具：[第一个 Tool](../../tutorials/first-tool.md)
- 了解事件如何映射到智能体循环：[事件参考](../../reference/events.md)
- 通过 HTTP 暴露智能体：[暴露 HTTP SSE](../../how-to/expose-http-sse.md)

## 常见错误

- 模型/提供商不匹配：`gpt-4o-mini` 需要兼容的 OpenAI 风格提供商配置。
- 缺少密钥：在 `cargo run` 之前设置 `OPENAI_API_KEY` 或 `DEEPSEEK_API_KEY`。
- 工具未被选中：确保提示词明确要求使用 `echo`。
- 没有 `RunFinish` 事件：检查 `with_max_rounds` 设置是否足够高，以便模型完成执行。

## 下一步

- [第一个 Tool](../../tutorials/first-tool.md)
- [事件参考](../../reference/events.md)
- [暴露 HTTP SSE](../../how-to/expose-http-sse.md)
