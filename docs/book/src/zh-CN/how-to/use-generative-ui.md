# 使用 Generative UI（A2UI）

当你希望 agent 把声明式 UI 组件发送给前端，而不是只返回文本时，使用本页。

## 前置条件

- 已有可运行的 runtime
- 前端能够消费 A2UI 消息（例如 CopilotKit 或 AI SDK 集成）
- 前端已经注册了组件目录（catalog）

```toml
[dependencies]
awaken = { version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 注册 A2UI 插件：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_generative_ui::A2uiPlugin;
use awaken::registry_spec::{AgentSpec, ModelSpec};
use awaken::{AgentRuntimeBuilder, Plugin};

let plugin = A2uiPlugin::with_catalog_id("my-catalog");
let agent_spec = AgentSpec::new("ui-agent")
    .with_model("gpt-4o-mini")
    .with_system_prompt("Render structured UI when visual output helps.")
    .with_hook_filter("generative-ui");

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
    .with_plugin("generative-ui", Arc::new(plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

插件会注册一个 `render_a2ui` 工具，LLM 通过它把 A2UI 消息数组发给前端。

2. 理解 A2UI v0.8 消息类型：

| 消息类型 | 作用 |
|-------------|------|
| `createSurface` | 创建渲染 surface |
| `updateComponents` | 定义或更新组件树 |
| `updateDataModel` | 写入或更新数据模型 |
| `deleteSurface` | 删除 surface |

消息顺序通常是：先创建 surface，再定义组件，最后填充数据。

3. 创建 surface：

```rust,ignore
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "createSurface": {
                "surfaceId": "order-form-1",
                "catalogId": "my-catalog"
            }
        }
    ]
});
```

4. 定义组件树：

组件列表是扁平的，通过 `child` / `children` 表示父子关系。必须有一个 `"id": "root"` 作为入口。

5. 用 JSON path 绑定数据：

组件属性里可以写 `{"path": "/json/pointer"}`，前端会在渲染时从 data model 里解析。

6. 删除 surface：

```rust,ignore
let messages = serde_json::json!({
    "messages": [
        {
            "version": "v0.8",
            "deleteSurface": {
                "surfaceId": "order-form-1"
            }
        }
    ]
});
```

7. 一次 tool call 可以携带多条消息：

`render_a2ui` 接收的是一个消息数组，所以可以在一次调用中同时创建 surface、更新组件树和写入数据。

8. 自定义插件指令：

```rust,ignore
let plugin = A2uiPlugin::with_catalog_and_examples(
    "my-catalog",
    "Example: create a card with a title and a button..."
);

let plugin = A2uiPlugin::with_custom_instructions(
    "You can render UI by calling render_a2ui...".to_string()
);
```

## 验证

1. 注册插件后，给 agent 一个“请以可视化方式展示内容”的提示
2. 确认 agent 调用了 `render_a2ui`
3. 事件流里应出现成功结果：`{"a2ui": [...], "rendered": true}`
4. 前端上应看到对应 surface 和组件

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 缺少 `messages` 字段 | tool 调用格式不对 | 传 `{"messages": [...]}` |
| `messages array must not be empty` | 消息数组为空 | 至少传一条 A2UI 消息 |
| `unsupported version` | 版本不是 `v0.8` | 每条消息都设为 `"version": "v0.8"` |
| 单条消息里混入多个类型键 | 一条消息同时含 `createSurface` 和 `updateComponents` 等 | 一条消息只允许一个类型键 |
| 组件缺少 `id` 或 `component` | 组件结构不完整 | 补齐必需字段 |
| LLM 不调用工具 | 插件已注册但未激活或 prompt 指令不足 | 检查 hook filter 和插件指令 |

## 相关示例

- `crates/awaken-ext-generative-ui/src/a2ui/tests.rs`

## 关键文件

- `crates/awaken-ext-generative-ui/src/a2ui/mod.rs`
- `crates/awaken-ext-generative-ui/src/a2ui/plugin.rs`
- `crates/awaken-ext-generative-ui/src/a2ui/tool.rs`
- `crates/awaken-ext-generative-ui/src/a2ui/types.rs`
- `crates/awaken-ext-generative-ui/src/a2ui/validation.rs`

## 相关

- [集成 CopilotKit (AG-UI)](./integrate-copilotkit-ag-ui.md)
- [集成 AI SDK 前端](./integrate-ai-sdk-frontend.md)
- [添加 Plugin](./add-a-plugin.md)
