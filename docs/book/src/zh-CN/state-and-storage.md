# 状态与存储

这条路径面向已经不满足无状态演示、需要认真设计状态与持久化的团队。

## 你可以在这里决定

- thread / run 数据放在哪里
- 状态键和合并策略怎么组织
- 每一轮究竟把多少上下文送给模型

## 推荐顺序

1. 从 [使用文件存储](./how-to/use-file-store.md) 或 [使用 Postgres 存储](./how-to/use-postgres-store.md) 开始，先确定持久化后端。
2. 阅读 [状态键](./reference/state-keys.md) 和 [线程模型](./reference/thread-model.md)，理解状态布局和生命周期。
3. 当上下文规模开始成为问题时，再阅读 [优化上下文窗口](./how-to/optimize-context-window.md)。

## 相关内部机制

- [状态与快照模型](./explanation/state-and-snapshot-model.md)
- [Run 生命周期与 Phases](./explanation/run-lifecycle-and-phases.md)
