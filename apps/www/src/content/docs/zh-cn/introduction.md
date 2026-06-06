---
title: "简介"
description: "Awaken — 用 Rust 写一次 Agent 能力，把行为调优交给在线配置，并让同一个 runtime 服务所有客户端。"
---

**Awaken** 是用 Rust 写的生产级 AI 智能体后端。Tools、state、plugins
在代码里写一次；agents、models、prompts 通过在线配置热调优；同一个 runtime
可以服务进程内应用、生产 API、多协议前端和管理控制台。涉及存储、密钥或策略的能力由
对应模块 / 插件显式接入。

本站依赖示例跟随当前 main 分支 API。在下一个 crates.io 版本发布前，请使用示例中的
git dependency；从已发布 `0.5` 版本线升级时，请同时阅读迁移指南。

三条设计准则决定其它一切。

## 1 — 工具落在代码,提示词落在配置

代码定义工具(类型化 schema、状态写入、延迟加载)。Spec / 配置承载 Agent 系统提示、
工具描述、Reminder、ToolSearch 策略、Skill 目录、显式 delegates 和权限规则。

改配置在**下一次 run** 生效。无需重启、无需重新部署、无需 schema 迁移。MCP server 通过 `tools/list_changed` 通知自动刷新;磁盘 Skill 包通过你在 bootstrap 启动一次的 `PeriodicRefresher` 刷新。Runtime 在每个新 run 重新从最新发布的配置快照解析。

启用 audit store 与 versioned-registry store 后，这些改动可以通过 record
revision 与 audit restore 追溯；已发布 runtime snapshot 是不可变的；durable
run 会携带 `resolution_id`，用于 resume/replay 时重新选择同一个 graph。

## 2 — 一个配置 API,一个管理控制台

`/v1/config/*` 是 Agent、模型、Provider、model pool、MCP server、Skill 和插件策略 section 的统一修改表面。自带的管理控制台是其中一个消费者;你的 CI 可以是另一个。

控制台写什么,runtime 读什么。没有需要单独维护的运维项目。

## 3 — 可观测性 / Eval / HITL 是运行时模块

服务可以接入:

- 覆盖每个 phase、工具、LLM 调用的 OpenTelemetry GenAI traces(`awaken-ext-observability`)。
- 管理控制台直接查询的持久化 trace store；trace HTTP 路由需要显式开启。
- 自带 fixture 回放、打分、baseline 对比的 Eval 框架(`awaken-eval`)。
- Permission gate + mailbox 实现的 HITL 挂起 / 恢复。

它们是一等 runtime / server 模块,不是额外拼接的 sidecar。

## 派生出来的四项能力

上面三条结合,带来其它框架普遍不具备的四项性质:

- **快照隔离 + 确定性重放。** 每个 phase 读取不可变 `Snapshot`,emit `MutationBatch`;`commit` 原子应用。保存的 snapshot 逐字节重放 —— 调试、回归、用历史流量跑 Eval 全部无需重付 LLM 成本。
- **一套后端，多种协议适配器。** 单 runtime 同时承接 AI SDK v6、AG-UI(CopilotKit)、A2A、MCP HTTP 与 ACP stdio。客户端协议选择不渗透到 agent 代码。
- **权限裁决是 runtime primitive。** `Gate` phase 在工具决策与工具执行之间运行;`Allow` / `Deny` / `Ask` 规则匹配工具名 + 参数;`Ask` 通过 mailbox 挂起,回应后恢复。
- **生成式 UI 是流式 primitive。** Agent 在同一条事件流上 emit A2UI / JSON Render / OpenUI Lang。前端无需为每个工具写胶水。

## 两种编程模式

Awaken 既可以作为进程内 library，也可以作为服务运行。两种模式使用同一个
`AgentRuntime`、`RunActivation`、`AgentSpec`、工具、插件和事件流；差别在于
谁负责 IO 边界和配置控制面。

| 模式 | 运行方式 | 适合场景 |
|---|---|---|
| 进程内 runtime | 你的 Rust 进程用 `AgentRuntimeBuilder` 构造 `AgentRuntime`，在代码里注册 tools / providers / plugins，然后直接调用 `runtime.run_to_completion(...)` 或 `runtime.run(..., EventSink)`。 | CLI、后台 worker、测试，或本身已经管理 IO 边界的应用服务。 |
| Server 控制面 | `awaken-server` 持有 `Arc<AgentRuntime>`，通过 mailbox 持久化分发 run，并暴露 HTTP/SSE 以及 AI SDK、AG-UI、A2A、MCP、ACP 适配器。普通 `/v1/config/*` 写入会校验配置、编译候选 registry，并把发布后的 snapshot 热替换给后续 run。 | 共享 agent 后端、浏览器前端、在线管理 providers / models / agents、审计、HITL、Eval 和运维控制。 |

两种模式里，Rust 代码负责可执行能力(`Tool` 实现、插件、provider factory、store、backend factory);托管配置负责 agent 行为(提示词、工具描述覆盖、reminders、`model_id`、model pool、允许/排除工具、插件 sections、MCP servers、skills、delegates、权限规则)。管理控制台只是 server 模式上的浏览器界面,不替代 runtime。Server 模式补上直接调用 runtime 时需要应用自己做的部分:HTTP/SSE、协议适配、mailbox 派发、可恢复后台 run、托管配置发布、版本恢复、审计、scoped stores。

进程内模式仍然是标准 Tokio/`std` async library,**不是** `no_std` 或无 Tokio 的嵌入式
目标:`awaken-runtime` 依赖 Tokio 处理 timer、timeout 与 provider 执行。`*-contract`
crate 只是 `std` 类型面;MCP、Skills、Stores、Observability exporter 与 Server 才是
明确的 IO 层。

## Crate 概览

| Crate | 说明 |
|-------|------|
| `awaken-runtime-contract` | runtime-facing contract：spec、tool、event、state、commit coordinator |
| `awaken-server-contract` | server/store-facing contract：query、scoped store、mailbox/outbox、staged commit |
| `awaken-runtime` | Phase 循环、插件系统、智能体循环、构建器 |
| `awaken-server` | HTTP/SSE 网关 + 协议适配器 |
| `awaken-stores` | 存储后端:内存、文件、Postgres、SQLite mailbox |
| `awaken-tool-pattern` | 工具名 glob/regex 匹配,用于权限与 reminder 规则 |
| `awaken-ext-permission` | 权限插件(allow/deny/ask) |
| `awaken-ext-observability` | OpenTelemetry traces + metrics |
| `awaken-eval` | Fixture 回放、打分与 baseline diff |
| `awaken-ext-mcp` | MCP 客户端集成 |
| `awaken-ext-skills` | Skill 包发现与激活 |
| `awaken-ext-reminder` | 声明式 reminder 规则 |
| `awaken-ext-generative-ui` | A2UI / JSON Render / OpenUI Lang |
| `awaken-ext-deferred-tools` | 基于概率模型的延迟工具加载 |
| `awaken` | 门面 crate,重新导出核心模块 |

## 阅读路径

1. [快速上手](/awaken/zh-cn/get-started/) → [第一个 Agent](/awaken/zh-cn/tutorials/first-agent/)。
2. [开发 Agent](/awaken/zh-cn/build-agents/) —— 在 Rust 中实现 tool、plugin、state、sub-agent 调用、UI stream、存储边界和 server integration。
3. [状态与存储](/awaken/zh-cn/state-and-storage/) —— 让 runtime 可持久化、可恢复、可分布式执行。
4. [服务与集成](/awaken/zh-cn/serve-and-integrate/) —— 通过 HTTP、AI SDK、CopilotKit、A2A、MCP、ACP、mailbox 和 admin surfaces 暴露 runtime。
5. [调优与运营](/awaken/zh-cn/operate/) —— 用管理控制台或配置 API 管理 prompt、model、MCP、Skill、策略、trace、dataset 和 eval。
6. [设计哲学](/awaken/zh-cn/explanation/philosophy/) —— 三条准则背后的"为什么"。
