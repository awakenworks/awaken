---
title: "添加 Tool"
description: "当你需要给 agent 暴露一个自定义能力时，使用本页。"
---

当你需要给 agent 暴露一个自定义能力时，使用本页。

## 目的

Tool 是模型请求外部动作时的代码边界。把能力放在类型化 descriptor、参数校验和 `ToolOutput` 后面，比只靠 prompt 说明更好，因为 runtime 可以校验输入、流式返回结果，并通过受控通道提交 state。

## 前置条件

- `Cargo.toml` 里已经加入 `awaken`
- 已添加 `async-trait` 和 `serde_json`

## 步骤

1. 实现 `Tool` trait。

```rust
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};

async fn fetch_weather(_city: &str) -> Result<String, ToolError> {
    Ok("晴,22°C".to_string())
}

pub struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("get_weather", "Get Weather", "Fetch current weather for a city")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name"
                    }
                },
                "required": ["city"]
            }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let city = args["city"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'city'".into()))?;

        let weather = fetch_weather(city).await?;

        Ok(ToolResult::success("get_weather", json!({ "forecast": weather })).into())
    }
}
```

2. 如有需要，重写参数校验：

```rust
fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
    if !args.get("city").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
        return Err(ToolError::InvalidArguments("'city' must be a non-empty string".into()));
    }
    Ok(())
}
```

`validate_args` 会在 `execute` 之前运行，可以让你提前拒绝格式错误的输入。

3. 在 builder 中注册 tool：

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::AgentRuntimeBuilder;

let runtime = AgentRuntimeBuilder::new()
    .with_tool("get_weather", Arc::new(WeatherTool))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"))
    .build()?;
```

`with_tool` 的字符串 ID 必须和 `ToolDescriptor::new` 里的 `id` 一致。

4. 或者在插件中注册：

   工具也可以在 `Plugin::register` 方法中通过 `PluginRegistrar` 注册：

```rust
fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
    registrar.register_tool("get_weather", Arc::new(WeatherTool))?;
    Ok(())
}
```

通过插件注册的工具仅对激活了该插件的 agent 可见。

## 控制 agent 能调用哪些工具

`with_tool` 把工具放进 runtime *registry* —— 任何运行中的 agent 都*可能*调用它。给定 agent *实际允许*用哪些工具,由 `AgentSpec` 的两个字段决定:`allowed_tools`(白名单)和 `excluded_tools`(黑名单)。这两个字段你可以在**代码里**、通过 **REST 配置 API**、或在**管理控制台界面**里设置——三处用的是同两个字段,且配置会在下一个 run 覆盖代码里的默认值。

开关只能在已注册的对象之间做选择,所以两边都必须先注册:

- **工具** —— 通过 `with_tool`、插件的 `registrar.register_tool`,或 MCP 自动注册。没注册的工具根本无法被调用。
- **Agent** —— 通过代码里的 `with_agent_spec`,或通过配置发布它的 spec(runtime 会合并本地与已发布的 agent)。下面引用的 `assistant` 必须以这种方式已存在,才能按 id 解析到;见 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)。

往这些注册表里注册是 runtime 的核心装配——同一套模型覆盖 tool、agent、model、provider 和 plugin。见 [智能体解析](/awaken/zh-cn/explanation/agent-resolution/)。

### 在代码里

把默认值写进你构建的 `AgentSpec`。这两个字段就是普通的 `Option<Vec<String>>`:

```rust
use awaken::AgentSpec;

let mut spec = AgentSpec::new("assistant")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("你帮用户处理天气问题。");
spec.allowed_tools = Some(vec!["get_weather".into()]); // None = 所有已注册工具
spec.excluded_tools = None;
// builder.with_agent_spec(spec)
```

### 通过配置动态覆盖

server 模式下,runtime 在调用时解析*托管*的 spec,所以发布一次改动即可覆盖代码里构建的默认值,不重新构建、不重启:

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "assistant",
    "model_id": "gpt-4o-mini",
    "system_prompt": "你帮用户处理天气问题。",
    "allowed_tools": ["get_weather"],
    "excluded_tools": []
  }'
```

### 在管理控制台界面

打开 agent 编辑器,使用 **Tools** 区:选择 **All tools** 或 **Custom selection**,用 source 过滤器筛选 built-in / plugin / MCP 工具,保存前先 validate 一次预览。分步:[通过配置调优 Agent 行为 → 收窄工具目录](/awaken/zh-cn/how-to/configure-agent-behavior/#收窄工具目录);编辑器导览:[使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)。

无论用哪个入口,`allowed_tools` 都是白名单、`excluded_tools` 都是黑名单,且改动在下一个 run 生效。工具在代码里加一次;按 agent 的开关可以在代码、配置或界面里做。

需要更细的按调用控制(基于参数形状的 allow/deny/ask,不只是工具名),用 [Permission 插件](/awaken/zh-cn/how-to/enable-tool-permission-hitl/)。

## 验证

发送一条应当触发该 tool 的消息，确认 run 结果里出现了预期的 tool 调用和返回值。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `ToolError::InvalidArguments` | LLM 传了错误 JSON | 收紧参数 schema，给模型更明确约束 |
| tool 从未被调用 | descriptor 的 `id` 与注册 ID 不一致 | 保证两者完全一致 |
| `ToolError::ExecutionFailed` | `execute` 内部运行时错误 | 返回清晰错误信息，让 agent 能据此调整 |

## 相关示例

`examples/src/research/tools.rs`

## 关键文件

- `crates/awaken-runtime-contract/src/contract/tool.rs`
- `crates/awaken-runtime/src/builder.rs`

## 相关

- [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)
- [添加 Plugin](/awaken/zh-cn/how-to/add-a-plugin/)
