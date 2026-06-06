---
title: "状态与存储"
description: "Build Agent 中关于 runtime state、config、mailbox job、profile/shared state 与持久化后端的开发指南。"
---

这组 Build Agents 文档面向已经不满足无状态演示、需要认真设计状态与持久化的团队。

## 目的

状态和存储是开发期选择，因为它们决定一个已构建的 agent 能安全记住什么、恢复什么、调优什么，以及能否分布式执行。在暴露给运营者前，应先由代码接好这些边界：managed config、mailbox dispatch、thread state、profile state 和 event history 都依赖已注册的 store。

## 你可以在这里决定

- thread / run 数据放在哪里
- runtime config、mailbox job 与 profile/shared state 放在哪里
- 哪些 state 属于 `StateKey`、Thread state、`ProfileKey` 或外部存储
- 哪些 context 应通过插件注入，而不是硬编码进 prompt
- 状态键和合并策略怎么组织
- 每一轮究竟把多少上下文送给模型
- sub-agent 派生子 thread 时，父子层级如何建模

## Thread 父子层级

Thread 携带可选的 `parent_thread_id`。当 sub-agent run 第一次物化子 thread
时，runtime 会用 `RunActivationSnapshot.trace.parent_thread_id`（或旧记录里的
`RunRequestSnapshot.parent_thread_id`）填充该字段。
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

1. 先读 [状态管理](/awaken/zh-cn/explanation/state-management/)，选择 run、thread、shared 或 profile state。
2. 再读 [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/)，理解插件如何通过 `PhaseContext`、`StateCommand` 和 snapshot mutation 管理状态。
3. 从 [使用文件存储](/awaken/zh-cn/how-to/use-file-store/) 或 [使用 Postgres 存储](/awaken/zh-cn/how-to/use-postgres-store/) 确定持久化后端。
4. 需要多副本时，用 [NATS Store](/awaken/zh-cn/how-to/use-nats-stores/) 做分布式 mailbox 与缓冲式 thread 写入。
5. 用 [Shared State](/awaken/zh-cn/how-to/use-shared-state/) 跨 thread 与 agent 类型共享持久状态。

当前内置 store 覆盖 memory、file、PostgreSQL、SQLite mailbox 与 NATS
JetStream。按需要的持久化边界选择最小后端：

| 能力 | Memory | File | PostgreSQL | SQLite | NATS |
|---|---|---|---|---|---|
| Thread/run projections | yes | yes | yes | no | 通过 `NatsBufferedThreadStore` decorator |
| Managed config | yes | yes | yes | no | no |
| Profile/shared state | yes | yes | no | no | no |
| Canonical events | yes | no | yes | no | no |
| Protocol replay log | yes | no | yes | no | no |
| Outbox/checkpoint repair | yes | no | yes | no | no |
| Stream checkpoints | yes | no | yes | no | no |
| Versioned registry | yes | yes | yes | no | no |
| Mailbox jobs | yes | no | no | 单节点持久 | 分布式持久 |

`NatsBufferedThreadStore` 可以包裹任意 thread/run 后端，通过 JetStream WAL 合并
checkpoint 写入。

## 存储边界

Awaken 区分 runtime execution state 和 server control plane。Runtime 开发可以
只用进程内 `AgentRuntime`、commit coordinator，以及 profile/shared state store。
Server 开发会在同一个 runtime 外围增加 mailbox dispatch、canonical events、
protocol replay、config versioning、audit，以及 eval/trace 持久化。

| 数据 | 契约 | Runtime-only 使用 | Server 使用 |
|---|---|---|---|
| Thread 与 run 投影 | `ThreadRunStore` + `CommitCoordinator` | `AgentRuntime` 的 checkpoint 读写边界 | 同一批投影，通常通过 server staged coordinator 提交 |
| 待处理用户输入与 dispatch 生命周期 | `MailboxStore` | 除非应用自己构建队列，否则不需要 | 持久后台 run、resume、cancel、interrupt、HITL、protocol delivery |
| Canonical events | `EventStore` | 基础进程内运行不需要 | 持久 event list/SSE resume 与 protocol replay |
| Outbox/staged ids | `StagedCommitCoordinator` / `ThreadCommitStagedOutcome` | Runtime 不观察 event/outbox ids | Server/store 实现提交后发布 event 与 outbox ids |
| 托管 registry config | `ConfigStore`、`ConfigRuntimeManager` | 可选；代码可以直接构造 registry | `/v1/config/*`、管理控制台编辑、audit restore、hot publication |
| Admin audit | `AuditLogStore` | 可选 | 版本历史、restore 与操作者追踪 |
| Profile/shared state | `ProfileStore`、shared-state store | 跨 run memory 与 learned priors | 通常由所有 served runs 共享 |
| Trace/eval 数据 | trace store、eval stores | 可选测试/运维工具 | Admin trace views、trace-to-fixture curation、eval datasets/runs |

Runtime commit outcome 会刻意保持窄边界：`ThreadCommitOutcome` 只表示 runtime
commit 成功/失败。需要 canonical event ids、server event ids 或 outbox ids 的
server-side 实现应使用 server-contract staged outcome。

## 构建自定义 store 时

只实现你需要的边界，并确保 runtime 写入走拥有同一份底层数据的 coordinator：

- `ThreadRunStore`：thread messages、run records、projections 和 checkpoint reads。
- `CommitCoordinator`：durable runtime writes；不要用不相关的 handle 写 runtime checkpoint。
- `ConfigStore` 与 `VersionedRegistryStore`：managed config、publication、restore 和 pinned registry replay。
- `ProfileStore` 或 shared-state store：跨 run memory。
- `MailboxStore` 加 `PendingMessageStore`：resumable dispatch、HITL 和 pending input steering。

新增后端前，先参考已有 store 和测试：
`crates/awaken-doctest/examples/thread_store_trait.rs`、
`crates/awaken-stores/src/memory/`、`crates/awaken-stores/src/postgres.rs`、
`crates/awaken-stores/src/pending_message_store.rs` 和
`crates/awaken-stores/tests/`。

## Mailbox 后端选择

Mailbox job 是 run-dispatch 控制面记录，和 thread/run checkpoint store 是两套边界。因此可以组合使用，例如 PostgreSQL 保存 thread/run 数据，同时用 NATS mailbox 负责分布式调度。

Mailbox dispatch status 是 delivery lifecycle。`Acked` 表示 dispatch 已被接受或
消费；执行是否成功要看关联的 `RunRecord.status`、termination reason 和
canonical events。

| 后端 | 适用场景 | 边界 |
| --- | --- | --- |
| `InMemoryMailboxStore` | 测试、本地开发、嵌入式单进程运行。 | 只在进程内有效；进程退出后 queued dispatch 会丢失。 |
| `SqliteMailboxStore` | 单节点服务需要持久 mailbox job，但不想运行 NATS。 | 使用本地存储持久化，但不是水平扩展 mailbox 后端。 |
| `NatsMailboxStore` | 多个 server 实例需要共享 dispatch 所有权、wakeup 和 lease recovery。 | 需要 JetStream 和 KV；所有实例必须使用同一组 stream、bucket 和 durable consumer。 |

## 相关内部机制

- [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/)
- [Run 生命周期与 Phases](/awaken/zh-cn/explanation/run-lifecycle-and-phases/)
