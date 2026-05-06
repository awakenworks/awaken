# 运行与运维

当 happy path 已经跑通，这条路径用于把 Agent 服务加固到可运维状态。

## 推荐顺序

1. 先启用 [可观测性](./how-to/enable-observability.md)，把 run、tool 和 provider 变得可见。
2. 再启用 [工具权限 HITL](./how-to/enable-tool-permission-hitl.md)，为工具执行增加审批控制。
3. 通过 [配置停止策略](./how-to/configure-stop-policies.md) 把 agent loop 约束在可预测范围内。
4. 用 [上报 Tool 进度](./how-to/report-tool-progress.md) 和 [测试策略](./how-to/testing-strategy.md) 提升可观测性和上线信心。
5. 当瞬时 provider 故障不应该浮现为 run 错误时，参考 [流式 LLM 错误恢复](./how-to/recover-streaming-llms.md)。

## 加固 admin 与配置面

两个互不相关的开关，详见 [配置参考](./reference/config.md)：

- `AdminApiConfig.bearer_token`（或 `AWAKEN_ADMIN_API_BEARER_TOKEN`）保护
  `/v1/capabilities`、`/v1/config/*`、`/v1/agents*`、`/v1/system/info`、
  `/v1/audit-log` 和 runtime-stats 端点。
- `AdminApiConfig.expose_config_routes = false` 把 admin CRUD 路由整组卸下，
  适合配置由外部流水线管理的部署。

如果配置写入频繁碎小，给 `ConfigRuntimeManager::with_min_apply_interval(Duration)`
设一个窗口可以把 listener 触发的 apply 合并；hash 未变的 `ProviderSpec` 会
复用缓存的 executor。

## 建议搭配阅读

- [错误](./reference/errors.md)
- [取消](./reference/cancellation.md)
- [HITL 与 Mailbox](./explanation/hitl-and-mailbox.md)
- [配置](./reference/config.md)
