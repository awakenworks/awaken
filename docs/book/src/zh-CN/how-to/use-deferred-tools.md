# 使用延迟加载工具

当你的代理拥有大量工具，且希望通过在 LLM 需要时才暴露工具 schema 来减少上下文窗口占用时，可以使用此功能。延迟加载工具插件将工具分为 Eager（始终发送）和 Deferred（隐藏直到被请求）两类。LLM 可通过 `ToolSearch` 工具按需发现延迟加载的工具。

## 前置条件

- 一个可运行的 awaken 代理运行时（参见 [第一个代理](../tutorials/first-agent.md)）
- `awaken-ext-deferred-tools` crate

```toml
[dependencies]
awaken-ext-deferred-tools = { version = "0.1" }
awaken = { package = "awaken-agent", version = "0.1" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 创建插件并注册。

收集代理暴露的所有工具描述符，然后将它们传给 `DeferredToolsPlugin::new`。插件会在激活时使用这些描述符进行工具分类并填充延迟注册表。

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::{AgentSpec, ModelSpec};
use awaken::{AgentRuntimeBuilder, Plugin};
use awaken_ext_deferred_tools::DeferredToolsPlugin;

// Collect descriptors from all tools registered on the agent.
let seed_tools = vec![
    weather_tool.descriptor(),
    search_tool.descriptor(),
    debug_tool.descriptor(),
    // ... all tool descriptors
];
let agent_spec = AgentSpec::new("deferred-agent")
    .with_model("gpt-4o-mini")
    .with_system_prompt("Search for tools only when needed.")
    .with_hook_filter("ext-deferred-tools");

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
    .with_plugin(
        "ext-deferred-tools",
        Arc::new(DeferredToolsPlugin::new(seed_tools)) as Arc<dyn Plugin>,
    )
    .build()
    .expect("failed to build runtime");
```

2. 配置工具加载规则。

通过代理规格中的 `deferred_tools` 配置键设置规则。规则按顺序求值，首次匹配即生效。未匹配任何规则的工具将回退到 `default_mode`。

```rust,ignore
use awaken_ext_deferred_tools::{
    DeferredToolsConfig, DeferralRule, ToolLoadMode,
};

let config = DeferredToolsConfig {
    rules: vec![
        DeferralRule { tool: "get_weather".into(), mode: ToolLoadMode::Eager },
        DeferralRule { tool: "debug_*".into(), mode: ToolLoadMode::Deferred },
    ],
    default_mode: ToolLoadMode::Deferred,
    ..Default::default()
};
```

`tool` 字段支持精确名称和 glob 模式（通过 `wildcard_match` 实现）。常见模式如下：

| 模式 | 匹配对象 |
|------|----------|
| `get_weather` | 精确工具 ID |
| `debug_*` | 所有以 `debug_` 开头的工具 |
| `mcp__github__*` | 所有 GitHub MCP 工具 |

3. 了解自动启用机制。

`DeferredToolsConfig` 上的 `enabled` 字段控制激活行为：

| 值 | 行为 |
|----|------|
| `Some(true)` | 始终启用延迟加载工具 |
| `Some(false)` | 始终禁用 |
| `None`（默认） | 当所有延迟工具的总 token 节省量超过 `beta_overhead`（默认 1136 tokens）时自动启用 |

在自动启用模式下，插件将每个工具的 schema 成本估算为 `len(parameters_json) / 4` tokens，并对所有可延迟工具的节省量求和。如果总节省量超过维护 `ToolSearch` 工具及上下文中延迟工具列表的开销，则自动激活延迟加载。

4. 了解 ToolSearch 的工作原理。

插件会自动注册一个 `ToolSearch` 工具。LLM 通过传入查询字符串来发现延迟加载的工具：

| 查询格式 | 行为 |
|----------|------|
| `"select:Tool1,Tool2"` | 按精确 ID 获取指定工具 |
| `"+required rest terms"` | 要求包含某个关键词，按其余词项排序 |
| `"plain keywords"` | 在 id、名称、描述中进行通用关键词搜索 |

当 `ToolSearch` 返回结果时，匹配的工具会在当前会话的后续过程中被提升为 Eager。该工具最多返回 `max_results`（默认 5）个匹配的工具 schema，格式为 `<functions>` 块。

```text
LLM: I need to check the weather. Let me search for relevant tools.
     -> calls ToolSearch { query: "weather forecast" }

ToolSearch returns matching schemas, promotes get_weather to Eager.
Next inference: get_weather schema is included in the tool list.
```

5. 配置自动重新延迟（DiscBeta）。

插件使用折扣 Beta 分布跟踪每个工具的使用统计。通过 `ToolSearch` 提升但不再被使用的工具会自动重新延迟。此过程是自适应的，无需手动调优。

重新延迟在以下条件全部满足时触发：
- 工具当前为 Eager（从 Deferred 提升而来）
- 工具未在规则中配置为始终 Eager
- 工具已空闲至少 `defer_after` 轮
- 后验上可信区间（90%）低于 `breakeven_p * thresh_mult`

盈亏平衡频率为 `(c - c_bar) / gamma`，其中 `c` 为完整 schema 成本，`c_bar` 为仅名称成本。只有当工具的使用频率足够高，使得避免 `ToolSearch` 调用带来的节省超过每轮开销时，保持 Eager 才是划算的。

DiscBetaParams 的关键参数：

| 参数 | 默认值 | 用途 |
|------|--------|------|
| `omega` | 0.95 | 每轮折扣因子。有效记忆约为 `1/(1-omega)` = 20 轮 |
| `n0` | 5.0 | 先验强度，以等价观测数表示 |
| `defer_after` | 5 | 考虑重新延迟前的最小空闲轮数 |
| `thresh_mult` | 0.5 | 盈亏平衡频率的延迟阈值乘数 |
| `gamma` | 2000.0 | ToolSearch 调用的估计 token 成本 |

这些参数位于 `DeferredToolsConfig.disc_beta` 下：

```rust,ignore
use awaken_ext_deferred_tools::{DeferredToolsConfig, DiscBetaParams};

let config = DeferredToolsConfig {
    disc_beta: DiscBetaParams {
        omega: 0.95,
        n0: 5.0,
        defer_after: 5,
        thresh_mult: 0.5,
        gamma: 2000.0,
    },
    beta_overhead: 1136.0,
    ..Default::default()
};
```

6. 启用跨会话学习。

通过 `AgentToolPriors`（一个 ProfileKey），使用频率通过 EWMA（指数加权移动平均）在会话间持久化。会话结束时，`PersistPriorsHook` 将每个工具的存在频率写入配置存储；下次会话开始时，`LoadPriorsHook` 读取这些数据，并用学习到的先验（而非默认的 0.01）初始化 Beta 分布。

这需要在运行时配置 `ProfileStore`。除了插件注册外无需额外代码——钩子会自动接入。

EWMA 平滑因子为 `lambda = max(0.1, 1/(n+1))`，其中 `n` 为会话计数。早期会话贡献相等；10 次会话后因子稳定在 0.1，即 90% 的权重来自历史数据。

## 验证

1. 运行代理并触发一次推理。检查日志中的 `deferred_tools.list` 上下文消息，其中列出了所有延迟加载的工具名称。

2. 从运行时快照中读取 `DeferralState`，查看每个工具的当前模式：

```rust,ignore
use awaken_ext_deferred_tools::state::{DeferralState, DeferralStateValue};

let state: &DeferralStateValue = snapshot.state::<DeferralState>()
    .expect("DeferralState not found");

for (tool_id, mode) in &state.modes {
    println!("{tool_id}: {mode:?}");
}
```

3. 向 LLM 提一个需要使用延迟工具的问题。确认 `ToolSearch` 被调用，且该工具在后续轮次中被提升为 Eager。

4. 经过数轮不活跃后，通过检查快照中工具是否恢复为 `Deferred` 模式来验证重新延迟功能。

## 常见错误

| 症状 | 原因 | 修复方法 |
|------|------|----------|
| 所有工具都发送给 LLM（无延迟加载） | `enabled: Some(false)` 或总节省量低于 `beta_overhead` | 设置 `enabled: Some(true)` 或添加更多工具使节省量超过开销 |
| 插件已注册但无工具被延迟 | 所有规则都解析为 `Eager` | 设置 `default_mode: ToolLoadMode::Deferred` 或添加 `Deferred` 规则 |
| LLM 无法使用 ToolSearch | 插件未注册 | 使用种子工具描述符注册 `DeferredToolsPlugin` |
| 工具从未被重新延迟 | `defer_after` 过高或工具使用频繁 | 降低 `defer_after` 或增大 `thresh_mult` |
| 跨会话先验未加载 | 未配置 `ProfileStore` | 在运行时接入配置存储 |
| ToolSearch 无返回结果 | 工具不在延迟注册表中 | 检查该工具是否包含在传给插件的 `seed_tools` 列表中 |

## 关键文件

| 路径 | 用途 |
|------|------|
| `crates/awaken-ext-deferred-tools/src/lib.rs` | 模块根及公共再导出 |
| `crates/awaken-ext-deferred-tools/src/config.rs` | `DeferredToolsConfig`、`DeferralRule`、`ToolLoadMode`、`DiscBetaParams` |
| `crates/awaken-ext-deferred-tools/src/plugin/plugin.rs` | `DeferredToolsPlugin` 注册 |
| `crates/awaken-ext-deferred-tools/src/plugin/hooks.rs` | 阶段钩子（BeforeInference、AfterToolExecute、AfterInference、RunStart、RunEnd） |
| `crates/awaken-ext-deferred-tools/src/tool_search.rs` | `ToolSearchTool` 实现与查询解析 |
| `crates/awaken-ext-deferred-tools/src/policy.rs` | `ConfigOnlyPolicy` 与 `DiscBetaEvaluator` |
| `crates/awaken-ext-deferred-tools/src/state.rs` | 状态键：`DeferralState`、`DeferralRegistry`、`DiscBetaState`、`ToolUsageStats`、`AgentToolPriors` |

## 相关文档

- [添加插件](./add-a-plugin.md)
- [添加工具](./add-a-tool.md)
- [优化上下文窗口](./optimize-context-window.md)
