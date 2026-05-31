---
title: "调优与运营"
description: "通过管理控制台和配置面调优已保存 Agent、检查 run、沉淀 trace、运行 eval，并加固生产行为。"
---

这条路径面向运行中的 Awaken server。开发者仍然在 Rust 中实现可执行能力；运营者
在线调优托管部分：prompt、工具描述、model、model pool、MCP server、Skill、
delegate、reminder、deferred-tool 策略、权限规则、trace、dataset 和 eval。

管理控制台是这条路径的主要 UI。REST 配置 API 是同一个控制面，可用于 CI 或内部工具。

## 推荐顺序

1. 先用 [管理控制台](/awaken/zh-cn/how-to/use-admin-console/) 连接运行中的 server，
   配置 provider-backed model，创建 Agent，预览草稿，并发布下一版 registry snapshot。
2. 用 [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/) 和
   [在线调优 Prompt](/awaken/zh-cn/how-to/hot-tune-prompts/) 理解完整可编辑表面。
3. 当 Agent 需要可发现或延迟加载能力时，接入 [MCP Tools](/awaken/zh-cn/how-to/use-mcp-tools/)、
   [Skills 子系统](/awaken/zh-cn/how-to/use-skills-subsystem/)、
   [Reminder 插件](/awaken/zh-cn/how-to/use-reminder-plugin/) 和
   [延迟加载工具](/awaken/zh-cn/how-to/use-deferred-tools/)。
4. 通过 [可观测性](/awaken/zh-cn/how-to/enable-observability/) 和
   [上报 Tool 进度](/awaken/zh-cn/how-to/report-tool-progress/) 把 run、tool 和 provider 变得可见。
5. 用 [工具权限 HITL](/awaken/zh-cn/how-to/enable-tool-permission-hitl/) 和
   [停止策略](/awaken/zh-cn/how-to/configure-stop-policies/) 约束行为并引入人工审核。
6. [测试策略](/awaken/zh-cn/how-to/testing-strategy/) 和
   [流式 LLM 错误恢复](/awaken/zh-cn/how-to/recover-streaming-llms/) 覆盖回归信心与 provider 故障处理。

## 重放与 Eval 循环

`awaken-eval` 会把保存的 fixture 通过 `RuntimeReplayer` 重放，对输出打分，
并与 NDJSON baseline 做 diff。它适合用保存的 prompt、tool output 与 provider
script 做回归检查，不需要重新支付 live provider 成本。Trace curation helper
可以把捕获到的 run 转成 fixture；当你要测量 provider 漂移本身时，也可以使用
live mode。

## 加固 admin 与配置面

两个互不相关的开关，详见 [配置参考](/awaken/zh-cn/reference/config/)：

- `AdminApiConfig.bearer_token`（或 `AWAKEN_ADMIN_API_BEARER_TOKEN`）保护
  `/v1/capabilities`、`/v1/config/*`、`/v1/agents*`、`/v1/system/info`、
  `/v1/audit-log` 和 runtime-stats 端点。
- `AdminApiConfig.expose_config_routes = false` 把 admin CRUD 路由整组卸下，
  适合配置由外部流水线管理的部署。

如果配置写入频繁碎小，给 `ConfigRuntimeManager::with_min_apply_interval(Duration)`
设一个窗口可以把 listener 触发的 apply 合并；hash 未变的 `ProviderSpec` 会
复用缓存的 executor。

## 建议搭配阅读

- [错误](/awaken/zh-cn/reference/errors/)
- [取消](/awaken/zh-cn/reference/cancellation/)
- [HITL 与 Mailbox](/awaken/zh-cn/explanation/hitl-and-mailbox/)
- [配置](/awaken/zh-cn/reference/config/)
