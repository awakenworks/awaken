---
title: "第一个 Agent"
---

## 目标

端到端运行一个智能体，并检查最终结果。

## 前置条件

```toml
[dependencies]
awaken = { version = "0.5" }
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

```rust
use std::sync::Arc;
use serde_json::{json, Value};
use async_trait::async_trait;
use awaken::contract::tool::{Tool, ToolDescriptor, ToolResult, ToolOutput, ToolError, ToolCallContext};
use awaken::contract::message::Message;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::AgentSpec;
use awaken::registry_spec::ModelSpec;
use awaken::{AgentRuntimeBuilder, RunActivation};

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
        .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
        .build()?;

    let request = RunActivation::new(
        "thread-1",
        vec![Message::user("Say hello using the echo tool")],
    )
    .with_agent_id("assistant");

    // 本教程只需要最终结果；需要向 SSE、WebSocket、协议适配器或测试流式发送事件时，
    // 使用 run(..., sink)。
    let result = runtime.run_to_completion(request).await?;
    println!("response: {}", result.response);
    println!("termination: {:?}", result.termination);

    Ok(())
}
```

## 2. 运行

```bash
cargo run
```

## 3. 验证

预期输出包括：

- `response: ...`
- `termination: NaturalEnd`

## 你创建了什么

本示例创建了一个进程内的 `AgentRuntime` 并立即执行一个请求。

核心对象是：

```rust
let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_tool("echo", Arc::new(EchoTool))
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
    .build()?;
```

之后，标准入口点是：

```rust
let result = runtime.run_to_completion(request).await?;
```

常见使用模式：

- 一次性 CLI 程序：构造 `RunActivation`，调用 `runtime.run_to_completion(...)`，打印结果
- 应用服务：当调用方需要流式事件时，使用带 `EventSink` 的 `runtime.run(...)`
- HTTP 服务器：将 `Arc<AgentRuntime>` 存储在应用状态中，暴露协议路由

## 另一种方式:从配置加载 agent

上面例子把 `system_prompt`、`model_id`、`max_rounds` 都写死在 Rust 里。这是一次性 CLI 最简单的路径。任何需要长跑的 agent —— 任何想不重新编译就改 prompt 的地方 —— 把 spec 移到配置里。

`EchoTool` 和 provider 留在代码,去掉 `with_agent_spec`:

```rust
let runtime = AgentRuntimeBuilder::new()
    .with_tool("echo", Arc::new(EchoTool))
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini"))
    .build()?;  // agent "assistant" 从配置 snapshot 解析
```

然后[暴露 HTTP/SSE](/awaken/zh-cn/how-to/expose-http-sse/),把 agent spec PUT 一次:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "assistant",
    "model_id": "gpt-4o-mini",
    "system_prompt": "你是一个有帮助的助理。被要求时使用 echo 工具。",
    "max_rounds": 5
  }'
```

之后想改 prompt,用同一个 id PUT 新的 `system_prompt`。下一次 `POST /v1/runs` 读到的就是新 snapshot —— 不重新构建、不重启。完整循环见[在线调优 Prompt](/awaken/zh-cn/how-to/hot-tune-prompts/)。

## 下一步阅读

根据你的需求选择下一页：

- 添加类型化状态和有状态工具：[第一个 Tool](/awaken/zh-cn/../tutorials/first-tool/)
- 了解事件如何映射到智能体循环：[事件参考](/awaken/zh-cn/../reference/events/)
- 通过 HTTP 暴露智能体：[暴露 HTTP SSE](/awaken/zh-cn/../how-to/expose-http-sse/)

## 常见错误

- 模型/提供商不匹配：`gpt-4o-mini` 需要兼容的 OpenAI 风格提供商配置。
- 缺少密钥：在 `cargo run` 之前设置 `OPENAI_API_KEY` 或 `DEEPSEEK_API_KEY`。
- 工具未被选中：确保提示词明确要求使用 `echo`。
- 过早终止：检查 `with_max_rounds` 设置是否足够高，以便模型完成执行。

## 下一步

- [第一个 Tool](/awaken/zh-cn/../tutorials/first-tool/)
- [事件参考](/awaken/zh-cn/../reference/events/)
- [暴露 HTTP SSE](/awaken/zh-cn/../how-to/expose-http-sse/)
