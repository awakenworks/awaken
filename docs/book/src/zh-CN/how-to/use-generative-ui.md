# 使用 Generative UI（A2UI）

当你希望 agent 把声明式 UI 组件发送给前端，而不是只返回文本时，使用本页。

## 前置条件

- 已有可运行的 runtime
- 前端能够消费 A2UI 消息（例如 CopilotKit 或 AI SDK 集成）
- 前端已经注册了组件目录（catalog）

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 注册 A2UI 插件：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_generative_ui::A2uiPlugin;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let plugin = A2uiPlugin::with_catalog_id("my-catalog");
let mut agent_spec = AgentSpec::new("ui-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Render structured UI when visual output helps.")
    .with_hook_filter("generative-ui");
agent_spec.plugin_ids.push("generative-ui".into());

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
    .with_plugin("generative-ui", Arc::new(plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

插件会注册一个 `render_a2ui` 工具，LLM 通过它把 A2UI 消息发给前端。
`plugin_ids` 负责加载插件，`with_hook_filter("generative-ui")` 在同一 agent
加载多个插件时保留 A2UI 的 prompt 注入 hook。

2. 理解 A2UI v0.8 消息类型：

| 消息类型 | 作用 |
|-------------|------|
| `surfaceUpdate` | 定义或更新组件树 |
| `dataModelUpdate` | 写入或更新数据模型 |
| `beginRendering` | 指定根组件并开始渲染 |
| `deleteSurface` | 删除 surface |

新 surface 的常见顺序是：先发 `surfaceUpdate`，需要数据时再发
`dataModelUpdate`，最后用 `beginRendering` 指定 root。

3. 定义组件树：

```rust,ignore
let message = serde_json::json!({
    "surfaceUpdate": {
        "surfaceId": "order-form-1",
        "components": [
            {
                "id": "root",
                "component": { "Card": { "child": "title" } }
            },
            {
                "id": "title",
                "component": {
                    "Text": { "text": { "literalString": "New Order" } }
                }
            }
        ]
    }
});
```

组件列表是扁平的。每个组件都要有 `id` 和 `component`；`component` 必须是
只包含一个 v0.8 组件类型的对象，例如 `{ "Text": {...} }`。父子关系通过组件
属性里的 `child` 或 `children.explicitList` 指向其他组件 ID。

4. 写入数据模型：

```rust,ignore
let message = serde_json::json!({
    "dataModelUpdate": {
        "surfaceId": "order-form-1",
        "path": "/order",
        "contents": [
            { "key": "customer", "valueString": "" },
            { "key": "quantity", "valueNumber": 1.0 }
        ]
    }
});
```

`contents` 是 key/value 数组，支持 `valueString`、`valueNumber`、
`valueBoolean` 和 `valueMap`。

5. 开始渲染：

```rust,ignore
let message = serde_json::json!({
    "beginRendering": {
        "surfaceId": "order-form-1",
        "root": "root"
    }
});
```

6. 删除 surface：

```rust,ignore
let message = serde_json::json!({
    "deleteSurface": {
        "surfaceId": "order-form-1"
    }
});
```

7. 一次 tool call 可以携带多条消息：

`render_a2ui` 接收 `messages` 数组，因此可以在一次调用中同时更新组件树并开始渲染：

```rust,ignore
let args = serde_json::json!({
    "messages": [
        { "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                { "id": "root", "component": { "Text": {
                    "text": { "literalString": "Hello" }
                }}}
            ]
        }},
        { "beginRendering": { "surfaceId": "s1", "root": "root" }}
    ]
});
```

8. 自定义插件指令：

```rust,ignore
let plugin = A2uiPlugin::with_catalog_and_examples(
    "my-catalog",
    "Example: create a card with a title and a button..."
);

let plugin = A2uiPlugin::with_custom_instructions(
    "You can render UI by calling render_a2ui...".to_string()
);

let agent_spec = agent_spec.with_section("generative-ui", serde_json::json!({
    "catalog_id": "my-catalog",
    "examples": "Example: render a compact order summary."
}));
```

`generative-ui` section 与 admin console 页面保存的是同一份配置，可覆盖
`catalog_id`、追加 `examples`，或用 `instructions` 完整替换默认指令。

## 验证

1. 注册插件后，给 agent 一个“请以可视化方式展示内容”的提示
2. 确认 agent 调用了 `render_a2ui`
3. 事件流里应出现成功结果：`{"a2ui": [...], "rendered": true}`
4. 前端上应看到对应 surface 和组件

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 缺少 A2UI 消息键 | tool 调用格式不对 | 传 `surfaceUpdate`、`dataModelUpdate`、`beginRendering`、`deleteSurface` 或 `{"messages": [...]}` |
| `messages array must not be empty` | 消息数组为空 | 至少传一条 A2UI 消息 |
| `surfaceUpdate.components is required` | 没有组件列表 | 提供非空 `components` |
| component payload 数量不为 1 | `component` 不是 `{ "Text": {...} }` 这类结构 | 每个组件只放一个 v0.8 组件类型 |
| `dataModelUpdate.contents must not be empty` | 数据模型更新为空 | 添加带 `key` 和 value 字段的条目 |
| `beginRendering.root is required` | 未指定根组件 | 把 `root` 指向已存在的组件 ID |
| LLM 不调用工具 | 插件未加载或 hook 被过滤 | 在 `plugin_ids` 中加入 `"generative-ui"`，使用 hook filter 时再加 `with_hook_filter("generative-ui")` |

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
