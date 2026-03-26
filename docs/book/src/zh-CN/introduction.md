# 简介

**Awaken** 是一个用 Rust 构建的模块化 AI 智能体运行时框架。它提供基于阶段的执行模型（含快照隔离与确定性重放）、带键作用域（thread/run）和合并策略（exclusive/commutative）的类型化状态引擎、用于可扩展性的插件生命周期系统，以及支持 AI SDK v6、AG-UI、A2A 和 ACP 多协议的服务器。

## Crate 概览

| Crate | 说明 |
|-------|------|
| `awaken-contract` | 核心契约：类型、trait、状态模型、智能体规约 |
| `awaken-runtime` | 执行引擎：阶段循环、插件系统、智能体循环、构建器 |
| `awaken-server` | HTTP/SSE 网关与协议适配器 |
| `awaken-stores` | 存储后端：内存、文件、PostgreSQL |
| `awaken-tool-pattern` | Glob/正则工具匹配，用于权限和提醒规则 |
| `awaken-ext-permission` | 权限插件，支持 allow/deny/ask 策略 |
| `awaken-ext-observability` | 基于 OpenTelemetry 的 LLM 和工具调用追踪 |
| `awaken-ext-mcp` | Model Context Protocol 客户端集成 |
| `awaken-ext-skills` | 技能包发现与激活 |
| `awaken-ext-reminder` | 声明式提醒规则，在工具执行后触发 |
| `awaken-ext-generative-ui` | 声明式 UI 组件（A2UI 协议） |
| `awaken` | 门面 crate，重新导出核心模块 |

## 架构

```text
┌─────────────────────────────────────────────────────┐
│  应用层                                              │
│  - 注册工具、定义智能体、调用 run_stream             │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  AgentRuntime                                        │
│  - 解析智能体、执行阶段、发射事件                     │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  Thread + 状态引擎                                   │
│  - 会话历史、快照隔离、状态键                         │
└─────────────────────────────────────────────────────┘
```

## 核心原则

所有状态访问遵循快照隔离。阶段钩子看到的是不可变快照；变更收集在 `MutationBatch` 中，在收敛后原子性地应用。

## 本书内容

- **教程** — 通过构建第一个智能体和第一个工具来学习
- **操作指南** — 面向任务的集成与运维实现指南
- **参考** — API、协议、配置和 Schema 查阅页面
- **解释** — 架构与设计原理

## 推荐阅读路径

> 以下链接指向英文文档，中文翻译正在进行中。

如果你是第一次接触本项目，建议按以下顺序阅读：

1. 阅读 [第一个 Agent](../tutorials/first-agent.md) 了解最小可运行流程。
2. 阅读 [第一个 Tool](../tutorials/first-tool.md) 理解状态读写。
3. 在编写生产工具前，阅读 [Tool Trait 参考](../reference/tool-trait.md)。
4. 使用 [构建 Agent](../how-to/build-an-agent.md) 和 [添加 Tool](../how-to/add-a-tool.md) 作为实现检查清单。
5. 需要完整执行模型时，返回阅读 [架构](../explanation/architecture.md) 和 [Run 生命周期与阶段](../explanation/run-lifecycle-and-phases.md)。

## 仓库导航

从文档进入代码时，以下路径最为重要：

| 路径 | 用途 |
|------|------|
| `crates/awaken-contract/` | 核心契约：工具、事件、状态接口 |
| `crates/awaken-runtime/` | 智能体运行时：执行引擎、插件、构建器 |
| `crates/awaken-server/` | HTTP/SSE 服务端 |
| `crates/awaken-stores/` | 存储后端 |
| `crates/awaken/examples/` | 小型运行时示例 |
| `examples/src/` | 全栈服务端示例 |
| `docs/book/src/` | 本文档源码 |
