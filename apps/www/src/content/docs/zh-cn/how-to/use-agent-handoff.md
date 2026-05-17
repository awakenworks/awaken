---
title: "使用 Agent Handoff"
description: "当你需要在同一个 run、同一条 thread 内切换到另一个已注册 agent ID，而不终止 run 或创建新 thread 时，使用本页。"
---

当你需要在同一个 run、同一条 thread 内切换到另一个已注册 agent ID，而不终止 run 或创建新 thread 时，使用本页。

## 前置条件

- 已添加 `awaken`
- 了解 `Plugin`、`StateKey` 和 `AgentRuntimeBuilder`

## 概览

handoff 会把请求的 active agent 写入状态。下一个 step 边界时，loop 读取 `ActiveAgentIdKey`，通过 `AgentResolver` 重新解析该 agent ID，停用旧插件、激活新插件，并在同一条 thread 历史上继续执行。

需要切换到的目标应注册为具体的 `AgentSpec`。`AgentOverlay` 是 `HandoffPlugin` 保存并可通过 `overlay()` 查询的可选元数据；内置 loop 不会把 overlay 字段合并进基础 `AgentSpec`。

关键类型：

- `HandoffPlugin`：把 handoff 状态同步到 active agent ID
- `AgentOverlay`：供集成方查看的可选 variant 元数据
- `HandoffState`：记录当前 active variant 和待处理请求
- `HandoffAction`：`Request`、`Activate`、`Clear` reducer action

## 步骤

1. 把 variant 定义成已注册 agent spec：

```rust
use awaken::registry_spec::AgentSpec;

let mut base = AgentSpec::new("assistant")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a helpful assistant.");

let mut researcher = AgentSpec::new("researcher")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a research specialist. Find and cite sources.");
researcher.allowed_tools = Some(vec!["web_search".into(), "read_document".into()]);

let writer = AgentSpec::new("writer")
    .with_model_id("claude-sonnet")
    .with_system_prompt("You are a technical writer. Produce clear documentation.");
```

`request_handoff()` 传入的字符串必须匹配这些 agent ID。

2. 构建 `HandoffPlugin`：

```rust
use awaken::extensions::handoff::HandoffPlugin;

let handoff = HandoffPlugin::new(Default::default());
```

3. 把插件注册进 runtime：

```rust
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;
use awaken::AgentRuntimeBuilder;

let mut spec = spec;
spec.plugin_ids.push("agent_handoff".into());

let runtime = AgentRuntimeBuilder::new()
    .with_plugin("agent_handoff", Arc::new(handoff))
    .with_agent_spec(base)
    .with_agent_spec(researcher)
    .with_agent_spec(writer)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
    .build()?;
```

插件 ID 必须是 `"agent_handoff"`（导出为 `HANDOFF_PLUGIN_ID`），并且必须列在
`AgentSpec.plugin_ids` 中。该插件会在 `Phase::RunStart` 和 `Phase::StepEnd`
注册 hook，同步 handoff 状态。

4. 在 tool 或 hook 中请求 handoff：

```rust
use awaken::extensions::handoff::{request_handoff, activate_handoff, clear_handoff, ActiveAgentKey};
use awaken::state::StateCommand;

let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(request_handoff("researcher"));

let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(activate_handoff("writer"));

let mut cmd = StateCommand::new();
cmd.update::<ActiveAgentKey>(clear_handoff());
```

5. 可选：读取 overlay 元数据：

```rust
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
| 状态 | 同一 thread state；在 step 边界重新解析 active agent | 一般隔离 |
| 适用场景 | 切换角色、人设或工具集 | 拆分独立子任务 |
| 开销 | 很低 | 更高 |

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| handoff resolve failed | `request_handoff` 的名字不是已注册 agent ID | 注册同名 `AgentSpec` |
| `StateError::KeyAlreadyRegistered` | 其他插件也注册了 `ActiveAgentKey` | 每个 runtime 只保留一个 `HandoffPlugin` |
| hook 没有执行 | agent hook filter 排除了插件 | 把 `"agent_handoff"` 加到 hook filter，或保持 filter 为空 |

## 关键文件

- `crates/awaken-runtime/src/extensions/handoff/mod.rs`
- `crates/awaken-runtime/src/extensions/handoff/plugin.rs`
- `crates/awaken-runtime/src/extensions/handoff/types.rs`
- `crates/awaken-runtime/src/extensions/handoff/state.rs`
- `crates/awaken-runtime/src/extensions/handoff/action.rs`

## 相关

- [添加 Plugin](/add-a-plugin/)
- [构建 Agent](/build-an-agent/)
