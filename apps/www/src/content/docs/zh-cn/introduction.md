---
title: "简介"
description: "Awaken — Rust 智能体运行时,框架本身就是平台。工具先行,提示词在线调,追踪 / Eval / HITL 内置。"
---

**Awaken** 是用 Rust 写的生产级 AI 智能体运行时。**框架就是平台**:服务启动后,追踪、重放、Eval、权限裁决、管理控制台都已经在跑。

依赖示例默认使用已发布的 `0.5` 版本线。如果你跟随 main 分支上的未发布 API，
请使用指向本仓库的 git dependency，而不是 crates.io 版本。

三条设计准则决定其它一切:

## 1 — 工具落在代码,提示词落在配置

代码定义工具(类型化 schema、状态写入、延迟加载)。Spec / 配置承载 Agent 系统提示、工具描述、Reminder、Skill 目录、权限规则。

改配置在**下一次 run** 生效。无需重启、无需重新部署、无需 schema 迁移。MCP server 通过 `tools/list_changed` 通知自动刷新;磁盘 Skill 包通过你在 bootstrap 启动一次的 `PeriodicRefresher` 刷新。Runtime 在每个新 run 重新从最新发布的配置快照解析。

## 2 — 一个配置 API,一个管理控制台

`/v1/config/*` 是 Agent、模型、Provider、插件、MCP server、Skill 包、权限、Trace 历史的**唯一**源。自带的管理控制台是其中一个消费者;你的 CI 可以是另一个。

控制台写什么,runtime 读什么。没有需要单独维护的运维项目。

## 3 — 可观测性 / Eval / HITL 跟着服务一起来

服务启动自动暴露:

- 覆盖每个 phase、工具、LLM 调用的 OpenTelemetry GenAI traces(`awaken-ext-observability`)。
- 管理控制台直接查询的持久化 trace store。
- 自带 fixture 回放、打分、baseline 对比的 Eval 框架(`awaken-eval`)。
- Permission gate + mailbox 实现的 HITL 挂起 / 恢复。

它们不是可选库,**就是** runtime。

## 派生出来的四项能力

上面三条结合,带来其它框架普遍不具备的四项性质:

- **快照隔离 + 确定性重放。** 每个 phase 读取不可变 `Snapshot`,emit `MutationBatch`;`commit` 原子应用。保存的 snapshot 逐字节重放 —— 调试、回归、用历史流量跑 Eval 全部无需重付 LLM 成本。
- **一套后端，多种协议适配器。** 单 runtime 同时承接 AI SDK v6、AG-UI(CopilotKit)、A2A、MCP HTTP 与 ACP stdio。客户端协议选择不渗透到 agent 代码。
- **权限裁决是 runtime primitive。** `Gate` phase 在工具决策与工具执行之间运行;`Allow` / `Deny` / `Ask` 规则匹配工具名 + 参数;`Ask` 通过 mailbox 挂起,回应后恢复。
- **生成式 UI 是流式 primitive。** Agent 在同一条事件流上 emit A2UI / JSON Render / OpenUI Lang。前端无需为每个工具写胶水。

## Crate 概览

| Crate | 说明 |
|-------|------|
| `awaken-contract` | 类型、trait、状态模型、智能体规约 |
| `awaken-runtime` | Phase 循环、插件系统、智能体循环、构建器 |
| `awaken-server` | HTTP/SSE 网关 + 协议适配器 |
| `awaken-stores` | 存储后端:内存、文件、Postgres、SQLite mailbox |
| `awaken-tool-pattern` | 工具名 glob/regex 匹配,用于权限与 reminder 规则 |
| `awaken-ext-permission` | 权限插件(allow/deny/ask) |
| `awaken-ext-observability` | OpenTelemetry traces + metrics |
| `awaken-ext-mcp` | MCP 客户端集成 |
| `awaken-ext-skills` | Skill 包发现与激活 |
| `awaken-ext-reminder` | 声明式 reminder 规则 |
| `awaken-ext-generative-ui` | A2UI / JSON Render / OpenUI Lang |
| `awaken-ext-deferred-tools` | 基于概率模型的延迟工具加载 |
| `awaken` | 门面 crate,重新导出核心模块 |

## 阅读路径

1. [快速上手](/awaken/zh-cn/get-started/) → [第一个 Agent](/awaken/zh-cn/tutorials/first-agent/)。
2. [构建 Agent](/awaken/zh-cn/build-agents/) —— 工具、MCP、Skill、Reminder、HITL、UI。
3. [服务与集成](/awaken/zh-cn/serve-and-integrate/) —— AI SDK / CopilotKit / A2A / MCP / ACP 客户端。
4. [状态与存储](/awaken/zh-cn/state-and-storage/)、[运行与运维](/awaken/zh-cn/operate/) —— 生产加固。
5. [设计哲学](/awaken/zh-cn/explanation/philosophy/) —— 三条准则背后的"为什么"。
