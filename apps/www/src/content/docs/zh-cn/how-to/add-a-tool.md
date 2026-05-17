---
title: "添加 Tool"
description: "当你需要给 agent 暴露一个自定义能力时，使用本页。"
---

当你需要给 agent 暴露一个自定义能力时，使用本页。

## 前置条件

- `Cargo.toml` 里已经加入 `awaken`
- 已添加 `async-trait` 和 `serde_json`

## 步骤

1. 实现 `Tool` trait。

```rust
use async_trait::async_trait;
use serde_json::{Value, json};
use awaken::contract::tool::{Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult, ToolOutput};

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
use awaken::registry::ModelBinding;
use awaken::AgentRuntimeBuilder;

let runtime = AgentRuntimeBuilder::new()
    .with_tool("get_weather", Arc::new(WeatherTool))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
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

- `crates/awaken-contract/src/contract/tool.rs`
- `crates/awaken-runtime/src/builder.rs`

## 相关

- [构建 Agent](/build-an-agent/)
- [添加 Plugin](/add-a-plugin/)
