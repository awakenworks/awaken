# 简介

**Awaken** 是一个用 Rust 构建的模块化 AI 智能体运行时框架。它提供基于阶段的执行模型（含快照隔离与确定性重放）、带键作用域（`thread` / `run`）和合并策略（`exclusive` / `commutative`）的类型化状态引擎、用于可扩展性的插件生命周期系统，以及支持 AI SDK v6、AG-UI、A2A 和 MCP（HTTP 及 stdio）的多协议服务面，以及 ACP stdio 协议面。

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
| `awaken-ext-deferred-tools` | 基于概率模型的延迟工具加载 |
| `awaken` | 门面 crate，重新导出核心模块 |

## 架构

```text
应用代码
  注册 tool / model / provider / plugin / agent spec
        |
        v
AgentRuntime
  将 AgentSpec 解析为 ResolvedAgent
  从插件构建 ExecutionEnv
  执行 phase loop，并暴露 cancel / decision 控制面
        |
        v
服务与存储表面
  HTTP 路由、SSE 回放、mailbox、协议适配器、thread/run 持久化
```

## 核心原则

所有状态访问遵循快照隔离。阶段钩子看到的是不可变快照；变更收集在 `MutationBatch` 中，在收敛后原子性地应用。

## 本书内容

- **快速上手** — 用最小可运行流程建立整体心智模型
- **构建 Agent** — 添加 Tool、Plugin、MCP、Skills、Reminder、Handoff 和 UI 能力
- **服务与集成** — 暴露 HTTP 端点并接入 AI SDK 或 CopilotKit 前端
- **状态与存储** — 选择持久化、上下文裁剪和状态访问模式
- **运行与运维** — 用可观测性、权限、进度上报和测试加固生产行为
- **参考** — API、协议、配置和 Schema 查阅页面
- **架构** — 运行时分层、phase 执行与设计取舍

## 推荐阅读路径

如果你是第一次接触本项目，建议按以下顺序阅读：

1. 先阅读 [快速上手](./get-started.md)，并完成 [第一个 Agent](./tutorials/first-agent.md)。
2. 需要扩展能力时，进入 [构建 Agent](./build-agents.md)。
3. 需要对接 HTTP 客户端或前端时，进入 [服务与集成](./serve-and-integrate.md)。
4. 从演示走向生产时，阅读 [状态与存储](./state-and-storage.md) 和 [运行与运维](./operate.md)。
5. 需要精确契约或运行时内部细节时，回到 [参考概览](./reference/overview.md) 和 [架构](./explanation/architecture.md)。

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
