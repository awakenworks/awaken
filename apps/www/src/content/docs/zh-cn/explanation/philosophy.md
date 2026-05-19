---
title: "设计哲学"
description: "塑造 Awaken 的三条准则,以及由此解锁、其它智能体框架普遍缺失的四项能力。"
---

Awaken 围绕三条准则构建。每一条都是硬边界,不是建议。三条合起来产生其它智能体框架普遍缺失的四项性质。

## 准则 1 —— 代码管工具,配置管提示词

工具是 Rust 产物:类型化输入 schema、可选的状态写入、可选的延迟加载钩子。工具需要编译期检查,改动很少。

提示词、工具描述、Reminder、权限规则、Skill 目录是**内容**。它们不断变化,需要快反馈循环。

两类严格分开。

| 层 | 落在 | 重载触发 |
|---|---|---|
| Tool、Plugin、Schema | Rust 代码 | 构建并部署 |
| Agent 系统提示、工具描述 | 通过配置 API 写入 `AgentSpec` | 下一次 run |
| 权限规则(allow/deny/ask) | 插件配置 | 下一次 run |
| Reminder 规则(工具模式 → 消息) | 插件配置 | 下一次 run |
| Skill 包(磁盘 YAML) | 文件系统 | `PeriodicRefresher`(显式调用 `start_periodic_refresh(interval)` 开启) |
| MCP server 工具 | 远端 MCP server | `tools/list_changed` 通知(自动) |

自动重载由 runtime 自己做。需要显式开启的(Skill)在 bootstrap 里调一次 `start_periodic_refresh` 即可 —— 你仍然不写 watcher 代码。

智能体工作的内循环 —— **改 prompt → 看效果** —— 由"一次 CI run"变成"一次 config API 往返"。

## 准则 2 —— 一个配置面,一个管理控制台

`/v1/config/*` 是 runtime 状态的**唯一**修改 API。Agent、模型、Provider、插件、MCP server、Skill 包、权限规则、Trace 历史全部经它暴露。

管理控制台是这个 API 的一个消费者,CI 流水线是另一个。runtime 读的是控制台写的同一个源。

没有独立"运维 UI"子项目,没有生产里和运行配置漂移的影子 YAML,没有跟着跑的带外缓存。

## 准则 3 —— Runtime 就是平台

启动服务自动开启,无需任何配置:

- 每个 phase、每次工具调用、每次 LLM 调用的 OpenTelemetry GenAI traces。
- 管理控制台直接查询的持久化 trace store。
- 自带 fixture 回放、打分、baseline 对比的 Eval 框架。
- Permission gate + mailbox 实现的 HITL 挂起 / 恢复。

它们不是用户拼装的可选库,**就是** runtime。Day-one 项目用的是最大型部署同一套表面。

---

## 派生的四项性质

### 快照隔离 + 确定性重放

每个 phase 读取不可变 `Snapshot`,emit 类型化 `MutationBatch`。`commit` 原子应用 batch,即使工具并行运行也是。

两个推论:

- **并行工具不会污染状态。** 每个类型化 state key 声明 `MergeStrategy`(`Exclusive`、`Commutative`)。合并在编译期校验。
- **任何 snapshot 都是时间机器。** 历史 run 从保存的状态逐字节重放 —— 调试事故、回归、用昨天的流量跑 Eval,全部无需重付 LLM 成本。

常见替代是加锁的可变共享状态(或强制串行)。两个插件碰到同一字段的那一刻,两种方案都悄无声息地坏。

### 一套后端,四种协议

同一个 `/v1/runs` 同时暴露为:

- **AI SDK v6**(Vercel `useChat()`)
- **AG-UI**(CopilotKit:chat + 生成式 UI + HITL)
- **A2A**(智能体对智能体调用)
- **MCP HTTP**(Claude / Cursor / Zed)

Runtime emit 一条 `AgentEvent` 流;协议适配器编码到对应线协议。换前端不动 agent 代码;同时承接多种前端不会让 runtime 翻倍。

常见替代是"挑一种协议,在每个客户端建适配器"。这把 agent 代码绑死在你下个季度可能后悔的前端选择上。

### 权限裁决是 runtime primitive

权限不是 UI prompt 也不是 middleware hook。它运行在类型化 `ToolGate` phase 中(`awaken-contract/src/model/phase.rs` 的 `Phase` enum 变体),位于"决定调用工具"和"执行工具"之间 —— runtime 在任何工具执行前都会进入这个 phase。

`awaken-ext-permission` 对每次调用匹配规则:

- `Allow` —— 放行。
- `Deny` —— 短路,返回结构化错误。
- `Ask` —— 通过 mailbox 挂起 run、持久化问题、回应后恢复(Web UI、Slack bot、CLI 任选)。

规则把工具名上的 glob/regex 与参数上的 JSON-path 表达式组合起来。规则落在配置里(准则 1 —— 在线调)。

常见替代是"工具抛异常 + 前端弹框"。这把 HITL 锁死在某一种前端,丢掉长生命周期 run 需要的挂起 / 恢复语义。

### 生成式 UI 是流式 primitive

Agent 在同一条 `AgentEvent` 流上 emit 声明式 UI(A2UI 组件、JSON Render 树、OpenUI Lang 文档)。协议适配器转发到前端;前端无需为每个工具写胶水。

UI surface 是一等状态 —— 有 ID、更新合并、子树和其它工具输出一样能通过 trace store 调试。

常见替代是"工具返回 JSON,前端按形状写 React"。这把 UI 迭代绑死在前端发布周期上,只要涉及 UI 就破坏'在线调'循环。

---

## 另见

- [架构](/awaken/zh-cn/explanation/architecture/) —— 三层 runtime 结构
- [Run 生命周期与 Phases](/awaken/zh-cn/explanation/run-lifecycle-and-phases/) —— 九个阶段
- [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/) —— 合并策略详解
- [HITL 与 Mailbox](/awaken/zh-cn/explanation/hitl-and-mailbox/) —— 挂起 / 恢复语义
- [设计取舍](/awaken/zh-cn/explanation/design-tradeoffs/) —— 我们考虑过的替代方案
