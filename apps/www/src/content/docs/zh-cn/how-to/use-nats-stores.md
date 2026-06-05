---
title: "使用 NATS Store"
description: "当你需要一个持久、可水平扩展的消息队列后端来承载 mailbox 派发,或一个用于 thread checkpoint 的缓冲写路径时使用。"
---

当你需要一个持久、可水平扩展的消息队列后端来承载 mailbox 派发,或一个用于 thread
checkpoint 的缓冲写路径时使用本页。

`awaken-stores` 的 `nats` feature 下有两个后端:

- **`NatsMailboxStore`** —— `MailboxStore` 实现,用 JetStream 做投递信号、用 NATS KV 作为
  派发状态的事实来源。
- **`NatsBufferedThreadStore<T>`** —— `ThreadRunStore` 装饰器,把 `checkpoint()` 写入缓冲在
  JetStream + KV 中,并异步刷入内层 store(如 `InMemoryStore`、`PostgresStore`)。按 thread
  合并写入以降低 DB 负载。

## 前置条件

- 启用 `awaken-stores` 的 `nats` feature
- 一个开启 JetStream 的 NATS server(`nats-server -js`)
- `tokio` runtime

## 启用 feature

```toml
[dependencies]
awaken-stores = { git = "https://github.com/AwakenWorks/awaken", features = ["nats"] }
```

## NatsMailboxStore

```rust
use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};

let config = NatsMailboxConfig::new("nats://localhost:4222");
let store = NatsMailboxStore::connect(config).await?;
// Use wherever a `MailboxStore` is expected.
```

默认会创建:

- Stream `DISPATCH`(subjects `dispatch.*`,WorkQueue retention)
- KV bucket `dispatch-state`(事实来源)、`thread-epoch`、`thread-index`
- 持久 consumer `dispatch-worker`

store 通过 `kv.watch_all()` 维护一个内存中的 list/query 索引。claim 与 interrupt 路径使用
权威的、按 thread 的 `thread-index` KV 记录,并从 KV 逐条加载派发记录,因此不依赖本地
watcher 的完整性。

`NatsMailboxConfig` 上的运维旋钮:

- `credentials`:认证集群用的可选 NATS credentials 文件内容。
- `sweeper_interval`:多久检查一次排队派发是否缺失唤醒信号。
- `sweeper_republish_after`:一个排队派发信号被压制多久后由 sweeper 重发。
- `dedup_window`:派发信号发布的 JetStream 去重窗口。
- `watcher_initial_scan_timeout`:从 KV 重建本地与按 thread 索引的启动超时。
- `authoritative_scan_timeout`:权威维护扫描的超时。
- `nats_request_timeout`:实时命令投递回退到持久派发前的 request/reply 超时。

信号循环的调优放在 server 环境变量里,这样现有的 `MailboxConfig` struct 字面量仍与 0.2.x
源码兼容:

| 变量 | 默认 | 用途 |
|----------|---------|---------|
| `AWAKEN_DISPATCH_SIGNAL_BATCH_SIZE` | `32` | 每次 pull 最多取的 JetStream 派发信号数。 |
| `AWAKEN_DISPATCH_SIGNAL_FETCH_EXPIRES_MS` | `500` | pull fetch 过期时间。 |
| `AWAKEN_DISPATCH_SIGNAL_NACK_BASE_DELAY_MS` | `500` | 被活跃 thread claim 阻塞的排队派发的初始延迟 NAK。 |
| `AWAKEN_DISPATCH_SIGNAL_NACK_MAX_DELAY_MS` | `30000` | 重投退避后的最大延迟 NAK。 |
| `AWAKEN_DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS` | `32` | 每个 pull batch 内最多活跃的信号 handler 任务数。 |

当一个排队派发可用但因 thread 已有活跃 claim 而无法运行时,信号循环使用带上限指数退避的
延迟 NAK。这避免了 JetStream 立刻重投的循环,同时保持至少一次的唤醒行为。

### 运维指标

NATS mailbox 指标通过全局 `metrics` recorder 发出:

- `awaken_mailbox_dispatch_signal_pulled_total`
- `awaken_mailbox_dispatch_signal_ack_total`
- `awaken_mailbox_dispatch_signal_nack_total{delayed}`
- `awaken_mailbox_dispatch_signal_redelivery_total`
- `awaken_mailbox_dispatch_signal_republish_total`
- `awaken_mailbox_claim_attempt_total{result}`
- `awaken_mailbox_claim_scan_keys_total`
- `awaken_mailbox_claim_scan_duration_ms`
- `awaken_mailbox_authoritative_scan_keys_total`
- `awaken_mailbox_authoritative_scan_duration_ms`
- `awaken_mailbox_queued_without_signal_age_ms`
- `awaken_mailbox_claimed_dispatch_lease_age_ms`
- `awaken_mailbox_expired_claim_reclaimed_total`
- `awaken_mailbox_dedupe_lock_reconciled_total`
- `awaken_mailbox_dedupe_lock_conflict_total`
- `awaken_mailbox_live_delivery_total{result}`
- `awaken_mailbox_index_rebuild_keys_total`
- `awaken_mailbox_index_rebuild_duration_ms`

建议的告警:

- 排队派发年龄持续高于服务的恢复目标。
- 派发信号延迟 NAK 或重投率突增。
- 过期的已 claim 派发被反复回收。
- claim 扫描 p95/p99 随无关的全局派发量增长。
- 去重锁冲突或对账突增。
- watcher 初始索引重建时长超出启动容忍度。

### 故障模式运维

排队派发卡住:

- 检查 `awaken_mailbox_queued_without_signal_age_ms`、
  `awaken_mailbox_dispatch_signal_republish_total`,以及 JetStream 持久 consumer 的
  pending/redelivery 计数。
- 确认派发记录在派发 KV bucket 中为 `Queued` 且在其 thread 的 `thread-index` 中存在。重启中
  的 store 会在 watcher 初始扫描时从派发 KV 重建缺失的按 thread 索引项。

已 claim 派发租约过期:

- 运行或等待 `reclaim_expired_leases`。NATS 回收扫描权威的 thread-claim guard 记录,然后点读
  派发 KV(而非本地 watcher 索引),因此本地缓存不完整的节点仍能回收过期 claim,而无需扫描
  历史终态派发记录。
- 终态回收路径在写 `DeadLetter` 或 `Superseded` 记录前会清空 `claim_token`、`claimed_by`、
  `lease_until`。

去重锁孤儿:

- 用相同 `(thread_id, dedupe_key)` 的新入队会把锁对账到权威派发 KV 与 thread epoch。缺失、
  终态或陈旧的排队持有者会在带 revision 检查后清除并重试。

consumer 滞后或重投压力:

- 检查持久 consumer 的 pending 消息与重投。
- 只有在 mailbox worker 与 NATS 能承受额外并发时才调大 `AWAKEN_DISPATCH_SIGNAL_BATCH_SIZE`。
  当长时 claim 造成反复阻塞重投时,调大延迟 NAK 上限。

watcher 初始扫描问题:

- 派发 KV 很大时调大 `watcher_initial_scan_timeout`。
- 把反复的启动超时当作信号:在 `gc_ttl` 后清理旧的终态派发,并复查
  `awaken_mailbox_index_rebuild_duration_ms`。

安全重启:

- 停止接收新请求,让活跃 claim 完成或过期,再重启节点。持久派发信号与派发 KV 在进程退出后
  存活;未 ack 的信号会重投,过期 claim 会被回收。

实时投递回退:

- 当活跃 runner 缺席或未在 `nats_request_timeout` 前 ack 时,
  `awaken_mailbox_live_delivery_total{result="no_subscriber"}` 属正常。它与排队派发年龄一起
  持续增长,说明实时命令在回退,但持久派发恢复在滞后。

### 压力与混沌测试

NATS 压力覆盖编译在 `crates/awaken-stores/tests/nats_mailbox_stress.rs`,默认被忽略。用
Docker 后端的 testcontainers 显式运行:

```bash
cargo test -p awaken-stores --features nats --test nats_mailbox_stress -- --ignored
```

设 `AWAKEN_NATS_STRESS_RECORDS` 可把记录数放大到 10k 或 100k 派发记录的跑法。

## NatsBufferedThreadStore

包裹任意现有 `ThreadRunStore` 来缓冲写入:

```rust
use std::sync::Arc;
use awaken_stores::{InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore};

let inner = Arc::new(InMemoryStore::new());
let config = NatsBufferedThreadConfig::new("nats://localhost:4222");
let buffered = NatsBufferedThreadStore::connect(inner, config).await?;
```

该 store 实现 `ThreadRunStore`,因此可插入任何接受该 trait 的位置。

默认会创建:

- Stream `THREADLOG`(subjects `thread.>`,文件存储,24h retention)
- KV bucket `thread-hot`(latest_seq、flushed_seq、缓存的 run 记录)
- 持久 consumer `thread-flusher`,ack_wait 为 30s

后台 flusher 每隔 `flush_interval`(默认 500ms)把每个 thread 的 checkpoint 合并进内层
store。WAL 消息仅在内层 checkpoint 与 `flushed_seq` 水位写入都成功后才 ACK,因此短暂的水位
失败会重投,而不会让 `force_flush()` 卡在旧水位之后。

`NatsBufferedThreadConfig::credentials` 接收认证集群用的可选 NATS credentials 文件内容。

### 读一致性

通过 `NatsBufferedThreadConfig::read_consistency` 配置:

- `ReadYourWrites`(默认)—— 当 `latest_seq > flushed_seq` 时,读在 DB 之上叠加 WAL 尾部
- `Strong` —— 读在查询 DB 前触发 `force_flush()`
- `Eventual` —— 读直接走 DB,忽略 WAL

### 显式 flush

```rust
store.force_flush("thread-123").await?;
```

阻塞直到后台 flusher 把该 thread 的每条 WAL 项都排空进内层 store。用于管理操作或关键读取。

### 毒消息隔离

一条持续无法应用的 WAL 项(例如不兼容升级后的反序列化错误)会被隔离,而不是永远重试。
flusher 对该项计算稳定哈希,带上限退避地 NAK,超过配置阈值后把它停到旁路通道,让 WAL stream
继续流动。运维者看到的是一次指标 tick,而非静默卡住的 checkpoint。通过 JetStream 管理工具
查看被隔离的项,并在底层缺陷修复后重放它们。

## 何时选哪个

| 需求 | 用 |
|------|-----|
| 多实例 mailbox + 分布式 claim | `NatsMailboxStore` |
| 降低热点 thread 的 DB 写放大 | `NatsBufferedThreadStore` 包 Postgres |
| 缓冲写入的同时靠 DB 索引维持分页 | `NatsBufferedThreadStore` |
| 单实例、无 NATS | `InMemoryMailboxStore` + `InMemoryStore` |

## 分布式部署

### 共享 NATS、共享 DB

运行多个 awaken-server 实例时,每个实例都必须连接:

- **同一个 NATS 集群**,且 `stream_name`、`consumer_name`、bucket 名都相同。JetStream
  WorkQueue 投递是至少一次的唤醒信号;重复执行由派发 KV CAS 与 thread claim guard 防止。
- **同一个内层 `ThreadRunStore`**(如共享 PostgreSQL)。每条 WAL 项只由一个实例的 flusher
  处理;其产生的 DB 写入必须对所有实例可见。

把两个实例指向不同的内层 store 会导致 DB 内容分叉。

### 多实例负载下验证的保证

NATS 集成测试套(`tests/nats_mailbox_behavior.rs`、`tests/nats_mailbox_conformance.rs`、
`tests/nats_mailbox_stress.rs`、`tests/nats_buffered_thread_*.rs`)验证:

- **Mailbox claim 互斥**:多个实例对同一派发并发 `claim_dispatch` 恰好一个赢家(KV CAS)。
- **租约恢复**:当持有 claim 的实例崩溃时,另一个实例在租约过期后经 `reclaim_expired_leases`
  回收派发。
- **Interrupt 传播**:实例 A 的 interrupt 在 flush 窗口内经 `kv.watch_all()` 被实例 B 的内存
  索引观察到。
- **写可见性**:实例 A 的 checkpoint 在 DB flush 完成前,即可经 WAL 叠加(read-your-writes)
  从实例 B 读到。
- **并发写者**:不同实例对同一 thread 的并行 `checkpoint()` 产生单调唯一的 `thread_seq`
  (对 `latest_seq` 的 KV CAS),且所有不同 run 都落到共享 DB。

### consumer 命名

共享一个 mailbox 或 buffered thread store 的所有实例必须使用相同的 `consumer_name`。不同的
consumer 名会创建各自独立的 consumer,每个都收到每条消息的完整副本 —— 这会破坏合并并重复
DB 写入。
