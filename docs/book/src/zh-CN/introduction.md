> 本文档为中文翻译版本，英文原版请参阅 [Introduction](../introduction.md)

# 简介

**Tirea** 是一个用 Rust 构建的不可变状态驱动 Agent 框架。它将类型化 JSON 状态管理与 Agent 循环相结合，提供对状态变更的完整可追溯性、回放能力以及组件隔离。

## Crate 概览

| Crate | 描述 |
|-------|------|
| `tirea-state` | 核心库：类型化状态、JSON 补丁、应用、冲突检测 |
| `tirea-state-derive` | 用于 `#[derive(State)]` 的过程宏 |
| `tirea-contract` | 共享契约：Thread / 事件 / 工具 / 插件 / 运行时 / 存储 / 协议 |
| `tirea-agentos` | Agent 运行时：推理引擎、工具执行、编排、插件组合 |
| `tirea-extension-*` | 插件：权限、提醒、可观测性、技能、MCP、A2UI |
| `tirea-protocol-ag-ui` | AG-UI 协议适配器 |
| `tirea-protocol-ai-sdk-v6` | Vercel AI SDK v6 协议适配器 |
| `tirea-store-adapters` | 存储适配器：memory / file / postgres / nats-buffered |
| `tirea-agentos-server` | HTTP / SSE / NATS 网关服务器 |
| `tirea` | 重新导出核心模块的伞形 crate |

## 架构

```text
┌─────────────────────────────────────────────────────┐
│  Application Layer                                    │
│  - Register tools, define agents, call run_stream    │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  AgentOs                                             │
│  - Prepare run, execute phases, emit events          │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  Thread + State Engine                               │
│  - Thread history, RunContext delta, apply_patch     │
└─────────────────────────────────────────────────────┘
```

## 核心原则

所有状态转换均遵循确定性纯函数模型：

```text
State' = apply_patch(State, Patch)
```

- 相同的 `(State, Patch)` 始终产生相同的 `State'`
- `apply_patch` 不会修改其输入
- 完整的历史记录支持回放到任意时间点

## 本书内容

- **教程** — 通过构建第一个 Agent 和第一个工具来学习
- **操作指南** — 面向具体任务的集成与运维实现指南
- **参考手册** — API、协议、配置及 Schema 查询页面
- **原理解析** — 架构与设计决策说明

## 推荐阅读路径

如果您是首次接触本代码库，建议按以下顺序阅读：

1. 阅读 [First Agent](./tutorials/first-agent.md)，了解最小可运行流程。
2. 阅读 [First Tool](./tutorials/first-tool.md)，理解状态读写机制。
3. 在编写生产级工具前，阅读 [Typed Tool Reference](./reference/typed-tool.md)。
4. 将 [Build an Agent](./how-to/build-an-agent.md) 和 [Add a Tool](./how-to/add-a-tool.md) 作为实现检查清单使用。
5. 需要了解完整执行模型时，回头阅读 [Architecture](./explanation/architecture.md) 和 [Run Lifecycle and Phases](./explanation/run-lifecycle-and-phases.md)。

## 代码库目录结构

从文档转入代码时，以下路径最为重要：

| 路径 | 用途 |
|------|------|
| `crates/tirea-contract/` | 核心运行时契约：工具、事件、状态 / 运行时接口 |
| `crates/tirea-agentos/` | Agent 运行时：推理引擎、工具执行、编排、扩展 |
| `crates/tirea-agentos-server/` | HTTP / SSE / NATS 服务端接入层 |
| `crates/tirea-state/` | 不可变状态 Patch / Apply / 冲突引擎 |
| `examples/src/` | 工具、Agent 和状态的小型后端示例 |
| `examples/ai-sdk-starter/` | 最简浏览器端对端示例 |
| `examples/copilotkit-starter/` | 包含审批与持久化的完整端对端 UI 示例 |
| `docs/book/src/` | 本文档源文件 |

完整的 Rust API 文档请参阅 [API Reference](./reference/api.md)。
