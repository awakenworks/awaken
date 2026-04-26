# 状态与存储

这条路径面向已经不满足无状态演示、需要认真设计状态与持久化的团队。

## 你可以在这里决定

- thread / run 数据放在哪里
- runtime config、mailbox job 与 profile/shared state 放在哪里
- 状态键和合并策略怎么组织
- 每一轮究竟把多少上下文送给模型
- sub-agent 派生子 thread 时，父子层级如何建模

## Thread 父子层级

Thread 携带可选的 `parent_thread_id`。当 sub-agent run 第一次物化子 thread
时，runtime 会用 `RunRequestSnapshot.parent_thread_id` 填充该字段。
`ThreadStore` 暴露 `list_child_threads`、`validate_thread_hierarchy` 和
`delete_thread_with_strategy(reject | detach | cascade)`，让调用方显式选择子
线程的处理策略。默认 `Detach` 会保留子线程并清空它们的 `parent_thread_id`。
默认的 `delete_thread_with_strategy` 在「子线程更新 + 最终删除」之间不是原子
操作；并发写场景下应当用事务或栅栏化的实现覆盖；file、PostgreSQL 与
NATS-buffered 后端已经有原生覆盖。

分页：`list_threads_query(&ThreadQuery)` 支持 `parent_filter`（`Any`、`Root`
或 `Parent(parent_id)`）与 `resource_id` 过滤，游标在 decode 时会校验原始
query 形状。`list_message_records(thread_id, &MessageQuery)` 提供带序号窗口、
`asc`/`desc` 排序、可见性过滤与产生 run 过滤的消息分页。

## 推荐顺序

1. 从 [使用文件存储](./how-to/use-file-store.md) 或 [使用 Postgres 存储](./how-to/use-postgres-store.md) 开始，先确定持久化后端。
2. 阅读 [状态键](./reference/state-keys.md) 和 [线程模型](./reference/thread-model.md)，理解状态布局和生命周期。
3. 当上下文规模开始成为问题时，再阅读 [优化上下文窗口](./how-to/optimize-context-window.md)。

当前内置 store 覆盖：thread/run 的内存、文件、PostgreSQL；config 的内存、文件、PostgreSQL；profile/shared state 的内存和文件；以及 mailbox job 的内存、SQLite 或 NATS JetStream。`NatsBufferedThreadStore` 还可以包裹任意 thread/run 后端，通过 JetStream WAL 合并 checkpoint 写入。

## Mailbox 后端选择

Mailbox job 是 run-dispatch 控制面记录，和 thread/run checkpoint store 是两套边界。因此可以组合使用，例如 PostgreSQL 保存 thread/run 数据，同时用 NATS mailbox 负责分布式调度。

| 后端 | 适用场景 | 边界 |
| --- | --- | --- |
| `InMemoryMailboxStore` | 测试、本地开发、嵌入式单进程运行。 | 只在进程内有效；进程退出后 queued dispatch 会丢失。 |
| `SqliteMailboxStore` | 单节点服务需要持久 mailbox job，但不想运行 NATS。 | 使用本地存储持久化，但不是水平扩展 mailbox 后端。 |
| `NatsMailboxStore` | 多个 server 实例需要共享 dispatch 所有权、wakeup 和 lease recovery。 | 需要 JetStream 和 KV；所有实例必须使用同一组 stream、bucket 和 durable consumer。 |

## 相关内部机制

- [状态与快照模型](./explanation/state-and-snapshot-model.md)
- [Run 生命周期与 Phases](./explanation/run-lifecycle-and-phases.md)
