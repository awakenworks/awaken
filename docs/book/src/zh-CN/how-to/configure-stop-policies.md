# 配置停止策略

当你需要根据轮数、token 用量、耗时或连续错误来决定 agent run 何时终止时，使用本页。

## 前置条件

- 已添加 `awaken`
- 了解 `Plugin` 与 `AgentRuntimeBuilder`

## 概览

stop policy 会在每次推理步骤结束后判断 run 是否继续。内置策略包括：

| 策略 | 触发条件 |
|---|---|
| `MaxRoundsPolicy` | 步数超过上限 |
| `TokenBudgetPolicy` | 输入+输出 token 超过预算 |
| `TimeoutPolicy` | 墙钟时间超过上限 |
| `ConsecutiveErrorsPolicy` | 连续推理错误达到阈值 |

## 步骤

1. 以编程方式构造策略：

```rust,ignore
use std::sync::Arc;
use awaken::policies::{
    MaxRoundsPolicy, TokenBudgetPolicy, TimeoutPolicy,
    ConsecutiveErrorsPolicy, StopPolicy,
};

let policies: Vec<Arc<dyn StopPolicy>> = vec![
    Arc::new(MaxRoundsPolicy::new(25)),
    Arc::new(TokenBudgetPolicy::new(100_000)),
    Arc::new(TimeoutPolicy::new(300_000)),
    Arc::new(ConsecutiveErrorsPolicy::new(3)),
];
```

2. 把 `StopConditionPlugin` 注册到 runtime：

```rust,ignore
use awaken::policies::StopConditionPlugin;
use awaken::AgentRuntimeBuilder;

let mut spec = spec;
spec.plugin_ids.push("stop-condition".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("stop-condition", Arc::new(StopConditionPlugin::new(policies)))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

如果只需要限制轮数，也可以直接用 `MaxRoundsPlugin`：

```rust,ignore
use awaken::policies::MaxRoundsPlugin;

let mut spec = spec;
spec.plugin_ids.push("stop-condition:max-rounds".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("stop-condition:max-rounds", Arc::new(MaxRoundsPlugin::new(10)))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

自定义 stop-condition 插件必须列在 `AgentSpec.plugin_ids` 中。内置
`AgentSpec.max_rounds` 保护仍会自动注入；只有需要额外 policy 类型时才使用这些
插件。

3. 用声明式 `StopConditionSpec`：

```rust,ignore
use awaken_contract::contract::lifecycle::StopConditionSpec;
use awaken::policies::{policies_from_specs, StopConditionPlugin};

let specs = vec![
    StopConditionSpec::MaxRounds { rounds: 10 },
    StopConditionSpec::Timeout { seconds: 300 },
    StopConditionSpec::TokenBudget { max_total: 100_000 },
    StopConditionSpec::ConsecutiveErrors { max: 3 },
];

let policies = policies_from_specs(&specs);
let plugin = StopConditionPlugin::new(policies);
```

完整的 `StopConditionSpec` 还包含 `StopOnTool`、`ContentMatch`、`LoopDetection`，但这些目前只有契约定义，尚未在 `policies_from_specs` 中实现。

## StopPolicy Trait

你也可以实现自定义策略：

```rust,ignore
use awaken::policies::{StopPolicy, StopDecision, StopPolicyStats};

pub struct MyCustomPolicy {
    pub threshold: u64,
}

impl StopPolicy for MyCustomPolicy {
    fn id(&self) -> &str {
        "my_custom"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if stats.total_output_tokens > self.threshold {
            StopDecision::Stop {
                code: "my_custom".into(),
                detail: format!("output tokens {} exceeded {}", stats.total_output_tokens, self.threshold),
            }
        } else {
            StopDecision::Continue
        }
    }
}
```

## StopPolicyStats

内置 `StopConditionHook` 会为每次评估准备一份 `StopPolicyStats`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `step_count` | `u32` | 已完成推理步数 |
| `total_input_tokens` | `u64` | 累计输入 token |
| `total_output_tokens` | `u64` | 累计输出 token |
| `elapsed_ms` | `u64` | 从第一步开始经过的毫秒数 |
| `consecutive_errors` | `u32` | 连续推理错误计数 |
| `last_tool_names` | `Vec<String>` | 最近一轮里调用的工具名 |
| `last_response_text` | `String` | 最近一次推理返回的文本 |

## StopDecision

```rust,ignore
pub enum StopDecision {
    Continue,
    Stop { code: String, detail: String },
}
```

任何一个 policy 返回 `Stop`，该 run 就会以 `TerminationReason::Stopped` 结束。

## Stop policy 如何进入 agent loop

1. `StopConditionPlugin` 在 `Phase::AfterInference` 注册 hook
2. 每轮推理结束后，hook 会更新统计状态
3. 构造 `StopPolicyStats`
4. 依次调用每个策略的 `evaluate`
5. 如果有策略返回 `Stop`，则写入 `RunLifecycleUpdate::Done`

`max` 或 `max_total` 设为 `0` 的策略视为禁用，始终返回 `Continue`。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| run 永远不结束 | 没注册 stop policy，LLM 一直在调工具 | 至少加一个 `MaxRoundsPolicy` |
| `StateError::KeyAlreadyRegistered` | 同时注册了 `StopConditionPlugin` 和 `MaxRoundsPlugin` | 二选一 |
| Timeout 提前触发 | `TimeoutPolicy::new()` 传的是毫秒，`StopConditionSpec::Timeout` 是秒 | 注意单位 |

## 关键文件

- `crates/awaken-runtime/src/policies/mod.rs`
- `crates/awaken-runtime/src/policies/policy.rs`
- `crates/awaken-runtime/src/policies/plugin.rs`
- `crates/awaken-runtime/src/policies/state.rs`
- `crates/awaken-runtime/src/policies/hook.rs`
- `crates/awaken-contract/src/contract/lifecycle.rs`

## 相关

- [添加 Plugin](./add-a-plugin.md)
- [构建 Agent](./build-an-agent.md)
