# 概览

`awaken` crate 是 Awaken 的公开门面。它把 `awaken-contract`、`awaken-runtime`、`awaken-stores` 以及若干扩展 crate 的公共 API 重新导出为一个统一依赖面。

## 模块再导出

| 门面路径 | 来源 crate | 内容 |
|---|---|---|
| `awaken::contract` | `awaken-contract` | tool、event、message、suspension、lifecycle 等契约 |
| `awaken::model` | `awaken-contract` | `Phase`、`EffectSpec`、`ScheduledActionSpec`、`JsonValue` |
| `awaken::registry_spec` | `awaken-contract` | `AgentSpec`、`ModelBindingSpec`、`ProviderSpec`、`McpServerSpec`、`PluginConfigKey` |
| `awaken::state` | `awaken-contract` + `awaken-runtime` | `StateKey`、`StateMap`、`Snapshot`、`StateStore`、`MutationBatch` |
| `awaken::agent` | `awaken-runtime` | agent 配置与状态 |
| `awaken::builder` | `awaken-runtime` | `AgentRuntimeBuilder`、`BuildError` |
| `awaken::context` | `awaken-runtime` | `PhaseContext` |
| `awaken::engine` | `awaken-runtime` | LLM 执行层抽象 |
| `awaken::execution` | `awaken-runtime` | `ExecutionEnv` |
| `awaken::extensions` | `awaken-runtime` | 内置扩展基础设施 |
| `awaken::loop_runner` | `awaken-runtime` | agent loop 执行器 |
| `awaken::phase` | `awaken-runtime` | `PhaseRuntime`、`PhaseHook` |
| `awaken::plugins` | `awaken-runtime` | `Plugin`、`PluginRegistrar` |
| `awaken::policies` | `awaken-runtime` | context window / retry policy |
| `awaken::registry` | `awaken-runtime` | `AgentResolver`、`ResolvedAgent` |
| `awaken::runtime` | `awaken-runtime` | `AgentRuntime` |
| `awaken::stores` | `awaken-stores` | file / postgres / memory store |

## 受 feature flag 控制的模块

| 门面路径 | feature flag | 来源 crate |
|---|---|---|
| `awaken::ext_permission` | `permission` | `awaken-ext-permission` |
| `awaken::ext_observability` | `observability` | `awaken-ext-observability` |
| `awaken::ext_mcp` | `mcp` | `awaken-ext-mcp` |
| `awaken::ext_skills` | `skills` | `awaken-ext-skills` |
| `awaken::ext_generative_ui` | `generative-ui` | `awaken-ext-generative-ui` |
| `awaken::ext_reminder` | `reminder` | `awaken-ext-reminder` |
| `awaken::server` | `server` | `awaken-server` |

## 根级再导出

常用类型还会直接从 crate root 导出，例如：

- 来自 `awaken-contract`：`AgentSpec`、`KeyScope`、`MergeStrategy`、`Phase`、`StateKey`、`StateMap`、`Snapshot`
- 来自 `awaken-runtime`：`AgentRuntime`、`AgentRuntimeBuilder`、`BuildError`、`RunRequest`、`RuntimeError`、`PhaseHook`

## Feature Flags

| Flag | 默认开启 | 说明 |
|---|---|---|
| `permission` | yes | 工具级权限控制与 HITL |
| `observability` | yes | tracing 与 metrics |
| `mcp` | yes | MCP 工具桥接 |
| `skills` | yes | 技能子系统 |
| `reminder` | yes | 工具执行后的提醒注入 |
| `server` | yes | HTTP / SSE / protocol server |
| `generative-ui` | yes | 生成式 UI 组件流 |
| `full` | yes | 上述功能全集 |

独立工作区扩展 crate 也可能存在但未接到门面 feature 上；当前包括 `awaken-ext-deferred-tools`。

## 相关

- [简介](../introduction.md)
- [Scheduled Actions](./scheduled-actions.md)
- [Effects](./effects.md)
