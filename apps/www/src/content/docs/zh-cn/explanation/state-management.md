---
title: "状态管理"
description: "Awaken 提供四层状态管理,各自面向不同的作用域、访问方式与生命周期组合。本页讲解每一层何时用、怎么用。"
---

Awaken 提供四层状态管理,各自面向不同的作用域、访问方式与生命周期组合。本页讲解每一层
何时用、怎么用。

> 本页是**「该用哪一层」**的选择指南。这些层背后的引擎内部机制 —— `StateKey` trait、
> 快照隔离、merge 策略与变更生命周期 —— 见
> [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/)。

## 概览

| 层 | Trait | 作用域 | 访问 | 生命周期 | 主要用途 |
|-------|-------|-------|--------|-----------|------------------|
| Run State | `StateKey`(`KeyScope::Run`) | 仅当前 run | 同步(snapshot) | run 开始时清空 | 临时计数器、标志、步骤状态 |
| Thread State | `StateKey`(`KeyScope::Thread`) | 同一 thread、跨 run | 同步(snapshot) | 跨 run 自动导出/恢复 | 工具调用状态、active agent、权限 |
| Shared State | `ProfileKey` + `StateScope` | 动态(全局、父 thread、agent 类型、自定义) | 异步(`ProfileAccess`) | 持久化于 `ProfileStore` | 跨边界共享、全局配置 |
| Profile State | `ProfileKey` + `key: &str` | 按 key(agent/system) | 异步(`ProfileAccess`) | 持久化于 `ProfileStore` | 用户/agent 偏好、locale |

## 插件上下文与命令

Plugin 不直接修改 state 或 prompt。phase hook 会收到 `PhaseContext`，其中包含 active `AgentSpec`、`RunIdentity`、当前 messages、冻结的 `Snapshot`、可选 `ProfileAccess`，以及 tool call 数据或 LLM response 等 phase-specific 字段。hook 返回 `StateCommand`。

`StateCommand` 是 plugin 和 tool 表达副作用的唯一命令通道：

- `patch` 通过 `MutationBatch` 更新已注册的 `StateKey`。
- `scheduled_actions` 请求 runtime handler 执行 runtime-owned 工作。
- `effects` 把终态副作用分发给已注册的 effect handler。

做 context 注入时，plugin 通过 `AddContextMessage` 调度一个 `ContextMessage`。runtime handler 会把接受的消息写入 `ContextMessageStore`、更新 `ContextThrottleState`，然后 loop 在 inference 前把消息插入 system、session、conversation 或 suffix-system band。这样比直接改 prompt 更好，因为注入内容有 key、顺序、节流和清理规则，且由 loop 统一执行。

## Run State

Run state 是最瞬时的一层。它完全存在内存中,通过 `Snapshot` 同步访问,并在新 run 开始时
清空为默认值。

写入通过 `MutationBatch` 进行,它收集 phase hook 产生的更新。当多个 hook 并行运行时,
runtime 用 `MergeStrategy` 判断对同一 key 的并发写是否能安全合并(`Commutative`)还是必须
串行化(`Exclusive`)。这使 run state 成为唯一参与事务性 merge 协议的层。

典型例子有 `RunLifecycle`(跟踪当前 run phase)、`PendingWorkKey`(统计未完成的异步工作)、
`ContextThrottleState`(限流上下文注入)。

### 何时用

- 不需要跨 run 存活的、单次推理内的临时状态
- 必须参与并行 merge 的状态(`MutationBatch` + `MergeStrategy`)
- 计数器、标志、步骤跟踪元数据

### 示例

```rust
struct StepCounter;
impl StateKey for StepCounter {
    const KEY: &'static str = "step_counter";
    type Value = usize;
    type Update = usize;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

// Register in plugin
r.register_key::<StepCounter>(StateKeyOptions::default())?;

// Read via snapshot
let count = ctx.snapshot.get::<StepCounter>().copied().unwrap_or(0);

// Write via MutationBatch
cmd.update::<StepCounter>(1);
```

## Thread State

Thread state 与 run state 共用访问模型 —— 通过 `Snapshot` 同步读、通过 `MutationBatch`
事务写、经 `MergeStrategy` 保证 merge 安全。区别在生命周期:thread 作用域的 key 在同一
thread 上跨 run 持久。

runtime 透明地处理这一点。run 结束时,thread 作用域的 key 被导出(序列化)。同一 thread 上
下一个 run 开始时,它们恢复为之前的值而非重置为默认。从 hook 作者的视角,这个 key 就像在
run 之间「记住」了它的值。

典型例子有 `ToolCallStates`(跨 run 恢复挂起的工具调用)和 `ActiveAgentKey`(持久化 agent
handoff 状态)。

### 何时用

- 必须在同一 thread 上跨 run 存活的状态
- 在每个 run 内需要同步访问与事务性 merge 保证的状态
- 生命周期应由 runtime 自动管理的状态

### 示例

```rust
r.register_key::<ToolCallStates>(StateKeyOptions {
    scope: KeyScope::Thread,
    persistent: true,
    ..StateKeyOptions::default()
})?;
```

## Shared State

Shared state 是构建在 `ProfileStore` 后端上的持久、异步层。它面向必须跨 thread 与 agent
边界的数据 —— 这是 run state 和 thread state 都做不到的。

Shared state 用 `ProfileKey` 把一个编译期命名空间绑定到一个值类型,再用 `key: &str` 参数
标识运行时实例。`(ProfileKey::KEY, key)` 一起唯一标识一条 shared state 记录。不同 agent 与
thread 只要用相同的 key 字符串,就能读写同一条记录。同一套 `ProfileAccess` 方法(`read`、
`write`、`delete`)同时服务 shared 与 profile state —— 它们都接收 `key: &str`。

因为 shared state 绕过了 snapshot/mutation-batch 流程,所以它不参与事务性 merge。并发写遵循
last-write-wins 语义。访问是异步的,经 `PhaseContext` 中的 `ProfileAccess`。

### StateScope —— 便捷 key 构造器

| 构造器 | Key 字符串 | 场景 |
|-------------|-----------|----------|
| `StateScope::global()` | `"global"` | 所有 agent 共享一个实例 |
| `StateScope::parent_thread(id)` | `"parent_thread::{id}"` | 父子 agent 在一棵委派树内共享 |
| `StateScope::agent_type(name)` | `"agent_type::{name}"` | 某 agent 类型的所有实例共享 |
| `StateScope::thread(id)` | `"thread::{id}"` | thread 本地持久状态 |
| `StateScope::new(s)` | `"{s}"` | 任意分组(租户、地域等) |

key 就是一个普通 `&str` —— 无需改代码即可完全扩展。`StateScope` 只是可选的便捷工具;任意
原始字符串都行。

### 何时用

- 跨 thread 边界共享的状态
- 跨 agent 边界共享的状态
- 编译期无法确定的动态作用域
- 充当类数据库索引存储的数据

### 示例

```rust
use awaken_runtime_contract::ProfileKey;

struct TeamContextKey;
impl ProfileKey for TeamContextKey {
    const KEY: &'static str = "team_context";
    type Value = TeamData;
}

// In a hook -- share context with child agents
let scope = StateScope::parent_thread(&ctx.run_identity.parent_thread_id.unwrap());
let mut team = access.read::<TeamContextKey>(scope.as_str()).await?;
team.goals.push("new goal".into());
access.write::<TeamContextKey>(scope.as_str(), &team).await?;
```

## Profile State

Profile state 是面向「按实体偏好」的持久、异步层。和 shared state 一样,它用 `ProfileStore`
后端、经 `ProfileAccess` 访问。区别在 key 约定:profile state 通常用 agent 名或 `"system"`
作为 key,而不是 `StateScope` 字符串。

`ProfileKey` 把一个静态命名空间字符串绑定到一个值类型。key 参数标识数据属于哪个 agent 或
system 实体。

### 何时用

- 按 agent 的持久偏好(locale、显示名、自定义设置)
- 所有 agent 共享的系统级配置
- 属于某个具体 agent 身份、而非动态分组的数据

### 示例

```rust
struct Locale;
impl ProfileKey for Locale {
    const KEY: &'static str = "locale";
    type Value = String;
}

let locale = access.read::<Locale>("alice").await?;
access.write::<Locale>("system", &"en-US".into()).await?;
```

## 决策指南

```text
单个 run 内需要状态?
  +-- 是,要同步 + 事务 --> Run State (StateKey, KeyScope::Run)
  +-- 否,需要持久
       +-- 仅同一 thread,同步 + 事务 --> Thread State (StateKey, KeyScope::Thread)
       +-- 跨边界、动态 key --> Shared State (ProfileKey + StateScope)
       +-- 按 agent/用户偏好 --> Profile State (ProfileKey + agent/system key)
```

## 对比:Shared State vs Thread State

两者都跨 run 持久数据。关键区别:

| 维度 | Thread State | Shared State |
|--------|-------------|--------------|
| 访问 | 同步(snapshot) | 异步(ProfileAccess) |
| 作用域 | 固定为当前 thread | 动态(任意字符串) |
| Merge 安全 | MutationBatch + 策略 | Last-write-wins |
| 跨边界 | 否 | 是 |
| 生命周期 | 自动导出/恢复 | 始终持久 |

当你需要 run 内的同步访问与事务保证时,用 **Thread State**。
当你需要跨边界共享或动态作用域时,用 **Shared State**。

## 另见

- [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/) —— 内部架构
- [State Keys](/awaken/zh-cn/reference/state-keys/) —— API 参考
- [使用 Shared State](/awaken/zh-cn/how-to/use-shared-state/) —— 实操 how-to
