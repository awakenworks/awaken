# 使用 Agent Handoff

当你需要在同一个 run、同一条 thread 内，动态切换 agent 的 system prompt、model 或工具集，而不终止 run 时，使用本页。

## 前置条件

- 已添加 `awaken`
- 了解 `Plugin`、`StateKey` 和 `AgentRuntimeBuilder`

## 概览

handoff 的核心不是启动一个全新 agent run，而是在当前 run 内应用 `AgentOverlay`，覆盖基础 agent 的一部分配置。

关键类型：

- `HandoffPlugin`
- `AgentOverlay`
- `HandoffState`
- `HandoffAction`

## 步骤

1. 定义 overlay：

```rust,ignore
use awaken::extensions::handoff::AgentOverlay;

let researcher = AgentOverlay {
    system_prompt: Some("You are a research specialist. Find and cite sources.".into()),
    model_id: Some("claude-sonnet".into()),
    allowed_tools: Some(vec!["web_search".into(), "read_document".into()]),
    excluded_tools: None,
};

let writer = AgentOverlay {
    system_prompt: Some("You are a technical writer. Produce clear documentation.".into()),
    model_id: None,
    allowed_tools: None,
    excluded_tools: Some(vec!["web_search".into()]),
};
```

`model_id` 字段使用的也是模型注册表 ID。

2. 用 overlays 构建 `HandoffPlugin`：

```rust,ignore
use std::collections::HashMap;
use awaken::extensions::handoff::HandoffPlugin;

let mut overlays = HashMap::new();
overlays.insert("researcher".to_string(), researcher);
overlays.insert("writer".to_string(), writer);

let handoff = HandoffPlugin::new(overlays);
```

3. 把插件注册进 runtime：

```rust,ignore
use std::sync::Arc;
use awaken::AgentRuntimeBuilder;

let mut spec = spec;
spec.plugin_ids.push("agent_handoff".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("agent_handoff", Arc::new(handoff))
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

插件 ID 必须是 `"agent_handoff"`（导出为 `HANDOFF_PLUGIN_ID`），并且必须列在
`AgentSpec.plugin_ids` 中。该插件会在 `Phase::RunStart` 和 `Phase::StepEnd`
注册 hook，同步 handoff 状态。

4. 在 tool 或 hook 中请求 handoff：

```rust,ignore
use awaken::extensions::handoff::{request_handoff, activate_handoff, clear_handoff, ActiveAgentKey};
use awaken::state::StateCommand;

let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(request_handoff("researcher"));

let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(activate_handoff("writer"));

let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(clear_handoff());
```

5. 从插件状态里读取 overlay：

```rust,ignore
let overlay = handoff.overlay("researcher");
```

## 它是如何工作的

`HandoffState` 有两部分：

- `active_agent`
- `requested_agent`

内部同步 hook 会在 `RunStart` 和 `StepEnd` 检测 `requested_agent`，并在安全边界上把它提升为 `active_agent`。

## Handoff vs Delegation

| | Handoff | Delegation |
|---|---|---|
| Thread | 同一 thread、同一 run | 通常会产生子 agent 执行上下文 |
| 状态 | 共享，原地覆盖 | 一般隔离 |
| 适用场景 | 切换角色、人设或工具集 | 拆分独立子任务 |
| 开销 | 很低 | 更高 |

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| overlay 没生效 | `request_handoff` 的名字和 overlays map 不一致 | 保证字符串完全一致 |
| `StateError::KeyAlreadyRegistered` | 其他插件也注册了 `ActiveAgentKey` | 每个 runtime 只保留一个 `HandoffPlugin` |
| hook 没有执行 | 插件未激活 | 把 `"agent_handoff"` 加到 hook filter |

## 关键文件

- `crates/awaken-runtime/src/extensions/handoff/mod.rs`
- `crates/awaken-runtime/src/extensions/handoff/plugin.rs`
- `crates/awaken-runtime/src/extensions/handoff/types.rs`
- `crates/awaken-runtime/src/extensions/handoff/state.rs`
- `crates/awaken-runtime/src/extensions/handoff/action.rs`

## 相关

- [添加 Plugin](./add-a-plugin.md)
- [构建 Agent](./build-an-agent.md)
