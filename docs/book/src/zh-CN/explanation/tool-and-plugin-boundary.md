# Tool 与 Plugin 的边界

Awaken 明确区分“给 LLM 使用的能力”（tools）和“运行时生命周期扩展”（plugins）。本页解释两者的边界、适用场景以及交互方式。

## Tools

tool 是暴露给 LLM 的能力：

```rust,ignore
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    fn validate_args(&self, args: &Value) -> Result<(), ToolError> { Ok(()) }
    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError>;
}
```

特征：

- 用 ID 注册
- 对 LLM 可见
- 会经过 `ToolGate`、`BeforeToolExecute`、真正执行以及 `AfterToolExecute`
- 只能访问参数和 `ToolCallContext`
- 适合文件操作、API 调用、数据库查询等业务能力

## Plugins

plugin 是运行时级扩展，不会出现在 LLM 工具列表中：

```rust,ignore
pub trait Plugin: Send + Sync + 'static {
    fn descriptor(&self) -> PluginDescriptor;
    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError>;
    fn config_schemas(&self) -> Vec<ConfigSchema> { vec![] }
}
```

特征：

- 用插件 ID 注册
- 对 LLM 不可见
- 在 phase 边界运行
- 可以访问更完整的 `PhaseContext`
- 适合权限、观察、提醒、请求变换、状态同步等横切逻辑

## PluginRegistrar

`Plugin::register()` 中，插件通过 `PluginRegistrar` 声明自己需要的能力：

| 注册方法 | 用途 |
|---------------------|------|
| `register_key::<K>()` | 注册状态键 |
| `register_tool_gate_hook()` | 注册纯判定的工具拦截钩子 |
| `register_phase_hook()` | 在指定 phase 添加 hook |
| `register_tool()` | 向运行时注入插件提供的工具 |
| `register_effect_handler()` | 处理 effects |
| `register_scheduled_action()` | 处理 scheduled actions |
| `register_request_transform()` | 在请求到达 LLM 前变换推理请求 |

插件也可以注册 tool，例如 `AgentTool`、MCP 工具、skills 工具。对于 LLM 来说，这些和用户手写 tool 没有区别。

## 何时使用 Tool

适合以下情况：

- 需要被 LLM 显式按名字调用
- 是领域能力，如搜索、计算、文件 I/O、API 调用
- 有结构化输入和可供 LLM 推理的结果

## 何时使用 Plugin

适合以下情况：

- 希望逻辑在 phase 边界自动运行
- 需要修改 inference request
- 需要在 LLM 看见之前检查或转换 tool result
- 属于权限、可观测性、提醒等横切关注点
- 需要注册 state key、effect handler 或 request transform

## Tool 和 Plugin 如何交互

插件可以影响 tool 执行，而 tool 自己并不需要感知：

- permission 插件在 `ToolGate` 决定放行、阻断或挂起调用
- observability 插件为 tool 执行包裹 trace
- reminder 插件在工具完成后注入上下文提示
- `ToolGate` 可在真正执行前替换结果、阻断调用或直接跳过执行

## 插件提供的 Tool 与用户 Tool

| 方面 | 用户 Tool | 插件 Tool |
|--------|-----------|-------------|
| 注册方式 | `AgentRuntimeBuilder::with_tool()` | `PluginRegistrar::register_tool()` |
| 生命周期 | 跟 runtime 一起存在 | 跟插件激活状态绑定 |
| 配置来源 | 直接构造 | 来自插件配置或 agent spec |
| 示例 | 业务工具 | `AgentTool`、MCP tools、skill tools |

## 另见

- [Tool Trait](../reference/tool-trait.md)
- [添加 Tool](../how-to/add-a-tool.md)
- [添加 Plugin](../how-to/add-a-plugin.md)
- [Run 生命周期与 Phases](./run-lifecycle-and-phases.md)
