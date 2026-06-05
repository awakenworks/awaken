---
title: "使用 Shared State"
description: "当 agent 需要跨 thread 边界、agent 类型或委派树共享持久状态时使用。Shared state 存在 ProfileStore 里,由类型化命名空间寻址……"
---

当 agent 需要跨 thread 边界、agent 类型或委派树共享持久状态时使用本页。Shared state 存在
`ProfileStore` 里,由类型化**命名空间**(`ProfileKey`)和一个 **key**(`&str`)寻址,让你
对「谁能看到什么」有细粒度控制。

## 前置条件

- 一个可运行的 awaken agent runtime(见 [First Agent](/awaken/zh-cn/tutorials/first-agent/))
- runtime 上配置了 `ProfileStore` 后端(如 file store 或 Postgres)

## 概念

Shared state 有两个维度:

| 维度 | 类型 | 用途 |
|-----------|------|---------|
| **命名空间** | `ProfileKey` | 定义*存什么* —— 静态字符串 key(`KEY`)与类型化 `Value` 的编译期绑定。每个 key 每个插件经 `register_profile_key` 注册一次。 |
| **Key** | `&str`(或 `StateScope` 辅助) | 定义*哪个实例* —— 划分存储的运行时字符串。不同 key 在 agent 与 thread 间隔离或共享数据。 |

`(ProfileKey::KEY, key: &str)` 一起在 profile store 里唯一标识一条 shared state 记录。

## 步骤

### 1. 定义一个 shared state key

创建一个实现 `ProfileKey` 的 struct。`KEY` 常量是命名空间;`Value` 类型是被序列化的内容。

```rust
use serde::{Deserialize, Serialize};
use awaken::contract::profile_store::ProfileKey;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TeamContext {
    pub goal: String,
    pub constraints: Vec<String>,
}

pub struct TeamContextKey;

impl ProfileKey for TeamContextKey {
    const KEY: &'static str = "team_context";
    type Value = TeamContext;
}
```

### 2. 在插件中注册

在插件的 `register` 方法里,对 registrar 调用 `register_profile_key`。

```rust
use serde::{Deserialize, Serialize};
use awaken::contract::profile_store::ProfileKey;
use awaken::{Plugin, PluginDescriptor, PluginRegistrar, StateError};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TeamContext {
    pub goal: String,
    pub constraints: Vec<String>,
}

pub struct TeamContextKey;

impl ProfileKey for TeamContextKey {
    const KEY: &'static str = "team_context";
    type Value = TeamContext;
}

pub struct TeamPlugin;

impl Plugin for TeamPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "team" }
    }

    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_profile_key::<TeamContextKey>()?;
        Ok(())
    }
}
```

### 3. 在 hook 中读写

在任意 phase hook 里,从 context 获取 `ProfileAccess`,用 key 字符串调用 `read` / `write`。
`StateScope` 是常见 key 模式的便捷构造器 —— 调用 `.as_str()` 取得 key。

```rust
use serde::{Deserialize, Serialize};
use awaken::contract::shared_state::StateScope;
use awaken::PhaseContext;
use awaken::contract::profile_store::ProfileKey;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TeamContext {
    pub goal: String,
    pub constraints: Vec<String>,
}

pub struct TeamContextKey;

impl ProfileKey for TeamContextKey {
    const KEY: &'static str = "team_context";
    type Value = TeamContext;
}

async fn execute(ctx: &mut PhaseContext) -> Result<(), Box<dyn std::error::Error>> {
    let profile = ctx.profile().expect("ProfileStore not configured");
    let identity = &ctx.run_identity;

    // Build a scope key from the current agent's parent thread
    let scope = match &identity.parent_thread_id {
        Some(pid) => StateScope::parent_thread(pid),
        None => StateScope::global(),
    };

    // Read (returns TeamContext::default() if missing)
    let mut team: TeamContext = profile.read::<TeamContextKey>(scope.as_str()).await?;

    // Mutate and write back
    team.goal = "Ship the feature".into();
    profile.write::<TeamContextKey>(scope.as_str(), &team).await?;

    Ok(())
}
```

### 4. 选对作用域

`StateScope` 有多个构造器。挑一个匹配你共享模式的:

| 场景 | Scope | 例子 |
|----------|-------|---------|
| 所有 thread 上的所有 agent | `StateScope::global()` | 组织级配置 |
| 同一父 thread 派生的所有 agent | `StateScope::parent_thread(id)` | 一棵委派树共享上下文 |
| 同一 agent 类型的所有实例 | `StateScope::agent_type(name)` | planner agent 共享学到的启发式 |
| 仅单个 thread | `StateScope::thread(id)` | thread 本地草稿 |
| 自定义分区 | `StateScope::new("custom-key")` | 任意应用自定义分组 |

你也可以直接传任意原始 `&str` —— `StateScope` 只是可选便捷。

## 何时用 shared state

| 机制 | 生命周期 | 作用域 | 最适合 |
|-----------|----------|-------|----------|
| `StateKey` | 单个 run(内存快照) | 单个 agent thread | 单 run 内的临时状态(计数器、标志、累积上下文) |
| `ProfileKey` + agent/system key | 持久(profile store) | 按 agent 或 system | 不跨边界的 per-agent / per-user 设置 |
| `ProfileKey` + `StateScope` key | 持久(profile store) | 任意 `StateScope` 字符串 | 跨 agent、跨 thread 的持久状态 |

当状态必须跨 run 存活**且**对不同 thread 或不同类型的 agent 可见时,用带 `StateScope` key 的
`ProfileKey`。

## 常见错误

| 现象 | 原因 | 修复 |
|---------|-------|-----|
| `profile key not registered: <ns>` | key 未在任何插件注册 | 在插件 `register` 里调用 `r.register_profile_key::<YourKey>()` |
| 总是读到 `Value::default()` | 写和读用了不同的 key 字符串 | 确认两侧构造相同的 `StateScope` 或用相同的 `&str` key |
| 数据在作用域间泄漏 | 该用更窄作用域时用了 `StateScope::global()` | 改用 `parent_thread`、`agent_type` 或 `thread` 作用域 |

## 关键文件

| 路径 | 用途 |
|------|---------|
| `crates/awaken-runtime-contract/src/contract/shared_state.rs` | `StateScope` 类型 |
| `crates/awaken-runtime-contract/src/contract/profile_store.rs` | `ProfileKey` trait、`ProfileOwner` enum |
| `crates/awaken-runtime/src/profile/mod.rs` | 带 `read`、`write`、`delete` 的 `ProfileAccess` |
| `crates/awaken-runtime/src/plugins/registry.rs` | `PluginRegistrar::register_profile_key` 注册 |

## 相关

- [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/)
- [State Keys](/awaken/zh-cn/reference/state-keys/)
- [添加 Plugin](/awaken/zh-cn/how-to/add-a-plugin/)
