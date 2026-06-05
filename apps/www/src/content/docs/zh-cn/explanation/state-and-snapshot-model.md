---
title: "状态与快照模型"
description: "Awaken 使用带快照隔离的类型化状态引擎。本页解释状态原语、作用域、合并策略以及状态变更生命周期。"
---

Awaken 使用带快照隔离的类型化状态引擎。本页解释状态原语、作用域、合并策略以及状态变更生命周期。

> 本页讲**引擎机制**。要选用四层中的哪一层(run / thread / shared / profile)以及何时用,
> 先看[状态管理](/awaken/zh-cn/explanation/state-management/)。

## StateKey Trait

每一类运行时状态都由一个实现了 `StateKey` 的零大小类型来声明：

```rust
pub trait StateKey: 'static + Send + Sync {
    const KEY: &'static str;
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
    type Update: Send + 'static;

    fn apply(value: &mut Self::Value, update: Self::Update);
}
```

`StateKey` 同时把字符串键名、值类型、更新类型和合并规则绑定在一起。

插件在 `Plugin::register` 中通过 `PluginRegistrar::register_key::<K>(options)` 注册;`StateKeyOptions` 携带 per-key scope(`Run` / `Thread`)等元数据。

## KeyScope

```rust
pub enum KeyScope {
    Run,
    Thread,
}
```

- `Run`：run 开始时重置
- `Thread`：在同一 thread 的多次 run 间保留

## MergeStrategy

```rust
pub enum MergeStrategy {
    Exclusive,
    Commutative,
}
```

- `Exclusive`：并发写同一键会冲突
- `Commutative`：更新可交换，允许并行合并

## Snapshot

`Snapshot` 是某个时刻的不可变状态视图。phase hook 和 tool 只读取 `Snapshot`，不能直接修改它。

```text
Phase hook 读取: &Snapshot
Phase hook 返回: StateCommand { patch: MutationBatch, scheduled_actions, effects }
```

这样同一 phase 内的所有 hook 都能看到一致状态。

## MutationBatch

`MutationBatch` 收集一次 hook 执行产生的状态更新：

```rust
pub struct MutationBatch {
    base_revision: Option<u64>,
    ops: Vec<Box<dyn MutationOp>>,
    touched_keys: Vec<String>,
}
```

`touched_keys` 用于并发冲突检测。

## 变更生命周期

```text
1. Phase 开始
2. 运行时拍一份 Snapshot
3. 每个 hook 读取 Snapshot，产出 MutationBatch
4. gather 完成
5. 运行时检查 MutationBatch 是否冲突
6. 合并后的 batch 原子提交到 live state
7. 为下一个 phase 创建新 Snapshot
```

## StateMap

`StateMap` 是类型擦除的状态容器，内部保存所有状态键对应的值。

- 持久化键会在 checkpoint 时序列化
- 非持久化键只在内存里存在

## StateStore

`StateStore` 在 `StateMap` 之上提供：

- 快照创建
- 带 revision 的 batch apply
- commit hooks
- `StateCommand` 处理

## 另见

- [状态键](/awaken/zh-cn/reference/state-keys/)
- [Run 生命周期与 Phases](/awaken/zh-cn/explanation/run-lifecycle-and-phases/)
- [架构](/awaken/zh-cn/explanation/architecture/)
