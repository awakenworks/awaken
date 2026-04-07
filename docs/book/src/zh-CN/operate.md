# 运行与运维

当 happy path 已经跑通，这条路径用于把 Agent 服务加固到可运维状态。

## 推荐顺序

1. 先启用 [可观测性](./how-to/enable-observability.md)，把 run、tool 和 provider 变得可见。
2. 再启用 [工具权限 HITL](./how-to/enable-tool-permission-hitl.md)，为工具执行增加审批控制。
3. 通过 [配置停止策略](./how-to/configure-stop-policies.md) 把 agent loop 约束在可预测范围内。
4. 用 [上报 Tool 进度](./how-to/report-tool-progress.md) 和 [测试策略](./how-to/testing-strategy.md) 提升可观测性和上线信心。

## 建议搭配阅读

- [错误](./reference/errors.md)
- [取消](./reference/cancellation.md)
- [HITL 与 Mailbox](./explanation/hitl-and-mailbox.md)
