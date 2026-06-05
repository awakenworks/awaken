---
title: "概览"
description: "awaken crate 是 Awaken 的公开门面。它重新导出 runtime contract、server contract、runtime API、stores 和扩展 crate，让下游可以从一个依赖开始。"
---

`awaken` crate 是 Awaken 的公开门面。它重新导出 runtime contract、server
contract、runtime API、stores 和扩展 crate，让下游可以从一个依赖开始。

## 参考地图

| 领域 | 页面 |
|---|---|
| **核心 API** | [Tool Trait](/awaken/zh-cn/reference/tool-trait/) · [事件](/awaken/zh-cn/reference/events/) · [错误](/awaken/zh-cn/reference/errors/) · [配置](/awaken/zh-cn/reference/config/) · [Provider 与 Model 配置](/awaken/zh-cn/reference/provider-model-config/) |
| **行为** | [Scheduled Actions](/awaken/zh-cn/reference/scheduled-actions/) · [Effects](/awaken/zh-cn/reference/effects/) · [取消](/awaken/zh-cn/reference/cancellation/) · [工具执行模式](/awaken/zh-cn/reference/tool-execution-modes/) |
| **状态** | [状态键](/awaken/zh-cn/reference/state-keys/) · [线程模型](/awaken/zh-cn/reference/thread-model/) |
| **表面** | [HTTP API](/awaken/zh-cn/reference/http-api/) · [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/) |
| **协议** | [AI SDK v6](/awaken/zh-cn/reference/protocols/ai-sdk-v6/) · [AG-UI](/awaken/zh-cn/reference/protocols/ag-ui/) · [A2A](/awaken/zh-cn/reference/protocols/a2a/) · [MCP HTTP](/awaken/zh-cn/reference/protocols/mcp/) · [ACP](/awaken/zh-cn/reference/protocols/acp/) |

本页其余部分是 crate **门面地图** —— 哪个 `awaken::*` 路径重导出哪个底层 crate。

## 模块再导出

| 门面路径 | 来源 crate | 内容 |
|---|---|---|
| `awaken::contract` | `awaken-runtime-contract` | runtime-facing tool、event、message、suspension、lifecycle、commit coordinator |
| `awaken::server_contract` | `awaken-server-contract` | server/store-facing storage query、scoped store、staged commit |
| `awaken::model` | `awaken-runtime-contract` | `Phase`、`EffectSpec`、`ScheduledActionSpec`、`JsonValue` |
| `awaken::registry_spec` | `awaken-runtime-contract` | `AgentSpec`、`ModelSpec`、`ProviderSpec`、`McpServerSpec`、`PluginConfigKey` |
| `awaken::state` | `awaken-runtime-contract` + `awaken-runtime` | `StateKey`、`StateMap`、`Snapshot`、`StateStore`、`MutationBatch` |
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
| `awaken::registry` | `awaken-runtime` | `AgentResolver`、`ResolvedAgent`、`ResolvedBackendAgent` |
| `awaken::runtime` | `awaken-runtime` | `AgentRuntime` |
| `awaken::stores` | `awaken-stores` | memory / file / PostgreSQL / SQLite-backed store |

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

- 来自 `awaken-runtime-contract`：`AgentSpec`、`KeyScope`、`MergeStrategy`、`Phase`、`StateKey`、`StateMap`、`Snapshot`
- 来自 `awaken-runtime`：`AgentRuntime`、`AgentRuntimeBuilder`、`BuildError`、`RunActivation`、`RuntimeError`、`PhaseHook`、`ToolGateHook`

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

- [简介](/awaken/zh-cn/introduction/)
- [Scheduled Actions](/awaken/zh-cn/reference/scheduled-actions/)
- [Effects](/awaken/zh-cn/reference/effects/)
