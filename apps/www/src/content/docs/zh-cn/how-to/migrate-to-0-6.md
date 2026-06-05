---
title: "从 0.5 迁移到 0.6"
description: "0.6.0 拆分 contract surface，并收窄 runtime commit 边界。本文说明从 0.5.0 升级时的公开 API、wire 行为与存储行为变化。"
---

> **新用户可跳过本页。** 它只在升级已有 0.5 代码库时才相关。新手请从
> [快速上手](/awaken/zh-cn/get-started/) 开始。

0.6.0 对实现 storage、commit coordinator，或直接导入底层 contract 类型的用户是 breaking release。高层 runtime builder 和常用 tool API 仍然可以通过 `awaken` 与 `awaken::prelude::*` 使用，但若干 0.5 contract 路径和 public field 已变化。

## Contract Crate 拆分

0.5 只有一个 contract crate：

```text
awaken-contract
```

0.6 拆成两个边界：

```text
awaken-runtime-contract  # runtime-facing types and traits
awaken-server-contract   # server/store-facing types and traits
```

工具、推理、事件、状态、registry specs、`ThreadCommit`、`ThreadCommitOutcome`、`CommitCoordinator` 和 runtime checkpoint read port 应使用 `awaken-runtime-contract`。

`ThreadQuery`、`MessageQuery`、store traits、scoped store wrappers、audit/config stores、outbox、protocol replay、versioned registry 与 staged commit outcome 应使用 `awaken-server-contract`。

历史上的 `awaken-contract` crate 仍保留为过渡 facade，但它不保证保留每一个
0.5 module path。应把这次拆分视为 breaking change，并把 import 迁到对应的
新 contract crate。`awaken` facade 里的 `awaken::contract::*` 是 runtime-facing
contract module；server/store contract 通过 `awaken::server_contract::*` 暴露。

| 0.5 import | 0.6 import |
|---|---|
| `awaken_contract::contract::commit_coordinator::Checkpoint` | `awaken_runtime_contract::contract::commit_coordinator::ThreadCommit` |
| `awaken_contract::contract::commit_coordinator::CheckpointCommitOutcome` | `awaken_runtime_contract::contract::commit_coordinator::ThreadCommitOutcome` |
| `awaken_contract::contract::storage::ThreadQuery` | `awaken_server_contract::contract::storage::ThreadQuery` |
| `awaken::contract::storage::ThreadQuery` | `awaken::server_contract::storage::ThreadQuery` |
| `awaken_contract::contract::storage::ThreadRunStore` | `awaken_server_contract::contract::storage::ThreadRunStore` |
| `awaken_contract::contract::config_store::ConfigStore` | `awaken_server_contract::contract::config_store::ConfigStore` |
| `awaken_contract::contract::mailbox::MailboxStore` | `awaken_server_contract::contract::mailbox::MailboxStore` |
| `awaken_contract::contract::audit_log::AuditLogStore` | `awaken_server_contract::contract::audit_log::AuditLogStore` |
| `awaken_contract::contract::transport::Transcoder` | `awaken_server_contract::contract::transport::Transcoder` |

`awaken::prelude::*` 继续面向常见的 agent 构建代码。它不承诺导入 0.5 的每一个
storage、commit、backend 或 server 管理符号。底层实现者应直接从
`awaken_runtime_contract`、`awaken_server_contract`、`awaken_runtime` 或
`awaken_server` 导入。

## RunActivation

`RunRequest` 被 `RunActivation` 取代。旧的扁平 request 现在拆分为用户意图、
输入、选项、trace、runtime control、capture wiring、persistence hints 和继承
的 resolver state。

| 0.5 概念 | 0.6 位置 |
|---|---|
| request 的 thread / agent / kind | `RunActivation.intent`（`RunIntent`） |
| request messages | `RunActivation.input`（`RunInput::NewMessages` 或 `AlreadyPersisted`） |
| inference overrides 和 frontend tools | `RunActivation.options` |
| origin、run mode、adapter trace、父 thread/run | `RunActivation.trace` |
| cancellation、decisions、inbox、pending boundary | `RunActivation.control` |
| thread context cache | `RunActivation.capture` |
| run/dispatch identity hints 与幂等标记 | `RunActivation.persistence` |
| replayable sub-run 继承的 registry/resolver | `RunActivation.inherited` |

大多数调用方仍使用
`RunActivation::new(thread_id, messages).with_agent_id(agent_id)`。Server 和
mailbox 集成会使用更底层的字段来保留 dispatch id、恢复 HITL wait，并避免重复
持久化已经追加到 thread log 的消息。

## Model 与 Failover

0.5 的模型绑定 API 合并为统一的 `ModelSpec` surface。Builder 注册现在使用
`with_model(spec)`，model id 来自 `spec.id`。校验 helper、unknown-field policy
常量和 mock provider helper 也都使用 model-spec 命名。

| 0.5 概念 | 0.6 surface |
|---|---|
| provider/upstream model binding | `ModelSpec` |
| runtime provider/upstream pair | `ModelSpec` |
| model binding validation | `validate_model_spec` |
| model binding unknown-field policy | `MODEL_SPEC_UNKNOWN_FIELD_POLICY` |
| mock provider binding helper | `MockProviderProfile::model_spec()` |
| `fallback_upstream_models` | 带有有序成员的 `ModelPoolSpec` |

HTTP/config wire 中模型配置的 key 仍是 `models`；缺少 capability/pricing 字段的
旧持久化配置仍可解析。模型 failover 现在属于 `ModelPoolSpec`，不再放在 provider
级 `fallback_upstream_models`。

## Resolver 与 Backend

执行解析与 backend 边界现在围绕 `Resolver`、`ResolutionRequest`、
`ResolvedRunPlan`、`ExecutionPlan` 和 `BackendProfile` 建模。

| 0.5 API | 0.6 API |
|---|---|
| `ResolvedExecution` | `ExecutionPlan` / `ResolvedRunPlan` |
| 临时拼接的 resolver request state | `ResolutionRequest` |
| `BackendCapabilities` | `BackendProfile` |
| backend capability bools | `DecisionCapability`、`PersistenceCapability`、`TranscriptCapability`、`OutputCapability` 等 typed dimensions |

`AgentResolver::resolve_execution(&agent_id)` 仍用于 delegate/tool 解析兼容，但
root execution 使用 async `Resolver` trait：

```rust
async fn resolve(&self, request: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError>;
```

`ExecutionBackend::capabilities()` 现在返回 `BackendProfile`。Backend request
也携带更多 runtime/server wiring，例如 `commit`、`pending_boundary`、
`state_seed` 和 `thread_state`。父到子的 `state_seed` 只接受本地执行计划；非本地
backend 会拒绝带 seed 的 delegate request，而不是静默丢弃。

## Checkpoint 重命名

`Checkpoint` 只作为 `ThreadCommit` 的 deprecated 类型名 alias 保留。它不能兼容 0.5 的 struct literal 字段名或字段读取。

| 0.5 field | 0.6 field |
|---|---|
| `messages` | `message_delta` |
| `expected_message_version` | `expected_message_count` |
| `run` | `run_projection` |
| `thread_state` | `thread_state_snapshot` |

语义仍能对应的旧 constructor 名称会继续以 deprecated helper 形式保留：

| 0.5 helper | 0.6 helper |
|---|---|
| `Checkpoint::append(...)` | `ThreadCommit::append_messages(...)` |
| `Checkpoint::checkpoint_only(...)` | `ThreadCommit::run_projection_only(...)` |
| `checkpoint.with_thread_state(...)` | `thread_commit.with_thread_state_snapshot(...)` |

如果旧代码使用 `Checkpoint { ... }` struct literal，或读取 `checkpoint.messages`、`checkpoint.run`、`checkpoint.thread_state`，需要改成新的字段名。

## Commit Outcome

`CheckpointCommitOutcome` 只作为 `ThreadCommitOutcome` 的 deprecated 类型名 alias 保留。Runtime outcome 不再携带 server event 或 outbox ids。

| 0.5 outcome field | 0.6 location |
|---|---|
| `canonical_event_ids` | `awaken_server_contract::ThreadCommitStagedOutcome::canonical_event_ids` |
| `server_event_ids` | `awaken_server_contract::ThreadCommitStagedOutcome::server_event_ids` |
| `additional_outbox_ids` | `awaken_server_contract::ThreadCommitStagedOutcome::additional_outbox_ids` |

`CommitCoordinator::commit_checkpoint` 是 runtime-only durability boundary。需要 event/outbox ids 的 store 实现应实现 `awaken_server_contract::StagedCommitCoordinator::commit_checkpoint_staged`，它返回 `ThreadCommitStagedOutcome`。

## 持久化接线

Runtime checkpoint 写入现在通过 `CommitCoordinator` 进入持久化边界。0.5 中直接
接收 `ThreadRunStore` 的 builder/runtime setter 在 0.6 中不再是 durable write
boundary。

| 0.5 接线 | 0.6 接线 |
|---|---|
| `AgentRuntimeBuilder::with_thread_run_store(store)` | `AgentRuntimeBuilder::with_commit_coordinator(coordinator)` |
| `AgentRuntime::with_thread_run_store(store)` | `AgentRuntime::with_commit_coordinator(coordinator)` |
| `AgentRuntimeBuilder::thread_run_store()` | `AgentRuntimeBuilder::commit_coordinator()` / coordinator `reader()` |
| `AgentRuntime::thread_run_store()` | 来自 coordinator 的 runtime checkpoint read port |
| `AgentRuntimeBuilder::with_mailbox_store(store)` | server `Mailbox` 加 `MailboxStore` 接线 |
| 直接写 `ThreadRunStore` checkpoint | `MemoryCommitCoordinator`、`FileCommitCoordinator` 或 `PgCommitCoordinator` |

`CommitCoordinator::reader()` 会提供 runtime checkpoint read port，因此 runtime
读取的 store 与 coordinator 提交的 store 一致。File-backed coordinator 面向
开发/本地部署，release build 需要设置 `AWAKEN_ALLOW_DEV_FILE_COORDINATOR=true`
显式启用；严格的 thread/run、event、outbox 事务一致性应使用
`PgCommitCoordinator`。

## Server 嵌入 API

`awaken-server` 现在以 `ServerState` 作为嵌入边界。`AppState` 仍是 deprecated
alias，新代码和文档应导入 `ServerState`。

| 0.5 API | 0.6 API |
|---|---|
| `AppState` | `ServerState` |
| 旧示例中接收 owned app state 的 route builder | `build_router(&ServerState)` 或 `build_service_router(ServerState)` |
| 直接读取 app state 内部 public field | `run_routes_state()`、`config_routes_state()`、`admin_api_config()` 等模块访问器 |
| `AdminApiConfig { bearer_token, cors_allowed_origins, expose_config_routes }` | 还要配置 `expose_trace_routes` 与 `expose_eval_routes` |
| 不含 eval caps 的 `ServerConfig` | `ServerConfig { eval_limits, .. }` |

Admin 启动校验更严格：只要 config、trace 或 eval 任一 admin surface 被暴露，
server 就要求通过 `AdminApiConfig.bearer_token` 或
`AWAKEN_ADMIN_API_BEARER_TOKEN` 配置 admin bearer token。`expose_config_routes = false`
只隐藏 config/admin-run routes，不是“关闭所有 admin 功能”的总开关。Eval route
默认暴露；trace route 默认隐藏，因为 trace 可能包含 prompt 和 tool args。

## 托管配置与 Admin 行为

托管配置发布覆盖 agents、models、model pools、providers、MCP servers、skills、
plugin sections 和 permission rules。成功的 create、update、delete 或 override
写入会校验候选 registry，并为后续 run 发布新的 snapshot。

Restore 与普通配置写入不同。Restore 会把选中的 audit snapshot 复制回 editing
`ConfigStore`，并记录新的 audit event，但不会热替换 runtime registry。应先审查
恢复出的 payload；需要让它成为 active registry snapshot 时，再执行一次普通配置保存。

0.6 新增或明确的 server/admin route surface 包括 system module discovery、
带 admin 认证的 run summary、canonical thread/run event list 和 stream routes、
trace routes、eval dataset/run/online routes、provider removal preview，以及 agent
override validation。完整路由树和暴露开关见 HTTP API reference。

## Mailbox 与协议语义

Mailbox dispatch status 描述 delivery lifecycle，不等于业务执行成功。`Acked`
表示 dispatch 已被接受/消费；执行结果应查看关联 run status、termination reason
和 canonical events。

协议适配器现在复用 server routes 的 scope、mailbox、cursor 和 event-store 语义。
AI SDK 的数字 `Last-Event-ID` 只适用于 live replay buffer；持久 canonical event
resume 使用 `/v1/threads/:id/events` 或 `/v1/runs/:id/events` 返回的 opaque event
cursor。MCP HTTP session 通过 mailbox 承载 run delivery 和 cancellation。

## Cursor 行为

Thread listing cursor 现在会绑定生成它的 query 形状。

0.5 的裸数字 cursor 只在无筛选 thread listing 中继续接受。带 resource、parent/root 或 backend scope filter 的列表必须使用同一 query 返回的 opaque `next_cursor` 继续翻页。带筛选条件时传入裸数字 cursor 会返回 `cursor does not match thread query filters`。

`ThreadQuery.id_prefix` 是 backend-internal 字段。Scoped store wrapper 用它在 backend 分页前下推 tenant/scope filter。HTTP route 不会把它暴露成用户可控 query 参数。

## 发布前检查

发布 0.6.0 前，应把当前 public surface 与 0.5.0 对照：

```bash
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-contract
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-runtime
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-server
cargo semver-checks check-release --baseline-version 0.5.0 -p awaken-stores
```

`awaken-runtime-contract` 和 `awaken-server-contract` 是 0.6 新 crate 名；真正需要检查的是旧 crate 与 `awaken` facade 是否暴露了预期的迁移 surface。
