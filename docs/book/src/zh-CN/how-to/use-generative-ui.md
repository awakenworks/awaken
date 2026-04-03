# 使用 Generative UI（A2UI）

当你希望 agent 把声明式 UI 消息发给前端，而不是只返回说明文本时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- 前端能够从事件流中渲染 A2UI 消息
- 前端已经注册了和模型可用组件名一致的组件目录

```toml
[dependencies]
awaken = { version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 注册 A2UI 插件。

```rust,ignore
use std::sync::Arc;

use awaken::{AgentRuntimeBuilder, Plugin};
use awaken::ext_generative_ui::{A2uiPlugin, DEFAULT_A2UI_CATALOG_ID};

let plugin = A2uiPlugin::default();
assert_eq!(
    plugin.instructions().contains(DEFAULT_A2UI_CATALOG_ID),
    true
);

let runtime = AgentRuntimeBuilder::new()
    .with_agent_spec(agent_spec)
    .with_plugin("generative-ui", Arc::new(plugin) as Arc<dyn Plugin>)
    .build()
    .expect("failed to build runtime");
```

插件会：

- 注册 `render_a2ui` 工具
- 在每次推理前把 A2UI 协议指引注入到模型上下文

通常前端会读取工具**输入**中的 A2UI 消息来渲染；工具**输出**只是确认信息，例如 `{"rendered": true, "count": 3}`。

2. 理解当前 A2UI 消息格式。

每次 tool call 可以传：

- 单个顶层消息键：`surfaceUpdate`、`dataModelUpdate`、`beginRendering`、`deleteSurface`
- 或兼容旧格式的 `messages` 数组

推荐顺序：

1. `surfaceUpdate`
2. `dataModelUpdate`
3. `beginRendering`

3. 用 `surfaceUpdate` 发送扁平组件列表。

```rust,ignore
let args = serde_json::json!({
    "surfaceUpdate": {
        "surfaceId": "ops_request",
        "components": [
            {"id": "root", "component": {"Card": {"child": "content"}}},
            {"id": "content", "component": {"Column": {"children": {"explicitList": ["title", "requester", "submit_label", "submit_button"]}}}},
            {"id": "title", "component": {"Text": {"usageHint": "h2", "text": {"literalString": "Operations request"}}}},
            {"id": "requester", "component": {"TextField": {"label": {"literalString": "Requester"}, "text": {"path": "/request/requester"}, "textFieldType": "shortText"}}},
            {"id": "submit_label", "component": {"Text": {"text": {"literalString": "Submit"}}}},
            {"id": "submit_button", "component": {"Button": {"child": "submit_label", "primary": true, "action": {"name": "ops_request.submit", "context": [{"key": "requester", "value": {"path": "/request/requester"}}]}}}}
        ]
    }
});
```

规则：

- `components` 是扁平列表
- 每个组件都必须包含 `id` 和 `component`
- `component` 必须是类似 `{"Text": {...}}` 的对象
- 父子关系通过嵌套 props 中的组件 ID 表达

4. 用 `dataModelUpdate` 写入绑定值。

```rust,ignore
let args = serde_json::json!({
    "dataModelUpdate": {
        "surfaceId": "ops_request",
        "path": "/request",
        "contents": [
            {"key": "requester", "valueString": ""},
            {"key": "status", "valueString": "draft"}
        ]
    }
});
```

5. 用 `beginRendering` 激活 surface。

```rust,ignore
let args = serde_json::json!({
    "beginRendering": {
        "surfaceId": "ops_request",
        "root": "root"
    }
});
```

6. 流程结束后用 `deleteSurface` 清理。

```rust,ignore
let args = serde_json::json!({
    "deleteSurface": {
        "surfaceId": "ops_request"
    }
});
```

7. 通过配置覆盖 catalog hint 或注入指令。

可以在创建插件时设默认值：

```rust,ignore
use awaken::ext_generative_ui::A2uiPlugin;

let plugin = A2uiPlugin::with_catalog_and_examples(
    "catalog://ops-ui",
    "Example: build a request intake card with a submit button."
);
```

也可以按 agent 覆盖：

```rust,ignore
use awaken::registry_spec::AgentSpec;
use awaken::ext_generative_ui::{A2uiPromptConfig, A2uiPromptConfigKey};

let agent = AgentSpec::new("a2ui")
    .with_model("default")
    .with_system_prompt("You create operational UIs with render_a2ui.")
    .with_config::<A2uiPromptConfigKey>(A2uiPromptConfig {
        catalog_id: Some("catalog://ops-ui".into()),
        examples: Some("Example: render an approval form.".into()),
        instructions: None,
    })?;
```

如果设置了 `instructions`，就会整段替换默认注入指令。

8. starter backend 也支持通过配置文件统一覆盖 agent 提示词和渲染器 catalog。

使用环境变量：

```bash
AGENT_CONFIG=/path/to/agents.json
```

所有 agent（包括 generative UI 渲染器）都在 `agents.<id>` 下统一配置：

```json
{
  "agents": {
    "default": {
      "system_prompt": "You are a concise assistant for the starter backend."
    },
    "a2ui": {
      "system_prompt": "You build procurement UIs with render_a2ui.",
      "catalog": "Catalog component hints:\n- TextField: 用于 requester、ticket ID 或 budget code 的单行输入框\n- MultipleChoice: 用于优先级、审批状态或部门选择的选项输入\n\nCatalog examples:\n- Use TextField for requester and budget code.\n- Use a nearby Text component as the visible label for MultipleChoice."
    },
    "json-render": {
      "system_prompt": "Use render_json_ui when the user asks for dashboards, forms, or review workspaces."
    },
    "json-render-ui": {
      "catalog": "{\n  \"Card\": \"用于审批摘要、表单分组或 KPI 区块的容器\",\n  \"Table\": \"用于请求明细、审计历史或任务队列的表格\",\n  \"Badge\": \"简短状态标签，例如 Pending / Approved / Blocked\"\n}"
    },
    "openui": {
      "system_prompt": "Use render_openui_ui for operational review panels and forms."
    },
    "openui-ui": {
      "catalog": "Available components:\n- Card(children, variant?) — 分组展示请求摘要、审批说明或编辑表单\n- Tag(text, variant?) — 状态标签\n- Buttons(buttons, direction?) — 工作流操作按钮栏"
    }
  }
}
```

- `system_prompt` — 完全替换 agent 的默认系统提示词。
- `catalog` — 自由格式文本，注入渲染器的提示词模板。格式取决于渲染器（json-render 用 JSON 对象，openui 用 DSL 签名，a2ui 用组件提示）。同时设置 `system_prompt` 时，`system_prompt` 优先。

## 验证

1. 启用 `generative-ui` 插件
2. 让 agent 生成表单、工作流面板或审阅界面
3. 确认模型调用了 `render_a2ui`
4. 确认前端能根据工具输入正确渲染

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `expected at least one A2UI message key` | tool 调用里没有支持的顶层 A2UI 键 | 传入 `surfaceUpdate`、`dataModelUpdate`、`beginRendering`、`deleteSurface` 之一 |
| `A2UI validation failed: message[0]: multiple message types...` | 一条消息里混入了多个消息类型 | 一条对象只保留一个 A2UI 消息键 |
| `components[0].id is required` | 组件缺少 `id` | 给每个组件补上 `id` |
| `components[0].component is required` | 组件缺少 payload | 补上类似 `{"Text": {...}}` 的 payload |
| `beginRendering.root is required` | `beginRendering` 没有引用根组件 | 传正确的根组件 ID |
| 模型不调用工具 | 插件没启用，或覆盖后的指令不正确 | 检查 agent 是否启用了 `generative-ui` 以及 `A2uiPromptConfigKey` 配置 |

## 关键文件

| 路径 | 作用 |
|---|---|
| `crates/awaken-ext-generative-ui/src/a2ui/plugin.rs` | `A2uiPlugin`、提示词注入与 typed config 覆盖 |
| `crates/awaken-ext-generative-ui/src/a2ui/tool.rs` | `render_a2ui` 的 schema、归一化、校验与执行 |
| `crates/awaken-ext-generative-ui/src/a2ui/types.rs` | A2UI payload 类型 |
| `crates/awaken-ext-generative-ui/src/a2ui/validation.rs` | A2UI 消息结构校验 |
| `examples/src/starter_backend/generative_ui_config.rs` | starter backend 的 agent 配置加载（system prompt 和 catalog 覆盖） |

## 相关

- [集成 CopilotKit / AG-UI](./integrate-copilotkit-ag-ui.md)
- [集成 AI SDK 前端](./integrate-ai-sdk-frontend.md)
- [添加 Plugin](./add-a-plugin.md)
