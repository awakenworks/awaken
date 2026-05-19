---
title: "状态键"
description: "Awaken 的状态系统提供类型化、带作用域、可持久化的键值存储。插件和工具在编译期声明状态键，运行时负责快照、持久化和并行合并语义。"
---

Awaken 的状态系统提供类型化、带作用域、可持久化的键值存储。插件和工具在编译期声明状态键，运行时负责快照、持久化和并行合并语义。

## StateKey trait

每个状态槽位都由一个实现了 `StateKey` 的类型来标识。

```rust
pub trait StateKey: 'static + Send + Sync {
    const KEY: &'static str;
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
    type Update: Send + 'static;

    fn apply(value: &mut Self::Value, update: Self::Update);
    fn encode(value: &Self::Value) -> Result<JsonValue, StateError>;
    fn decode(value: JsonValue) -> Result<Self::Value, StateError>;
}
```

### 示例

```rust
struct Counter;

impl StateKey for Counter {
    const KEY: &'static str = "counter";
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}
```

## KeyScope

控制键值在 run 边界上的生命周期：

```rust
pub enum KeyScope {
    Run,
    Thread,
}
```

## MergeStrategy

决定并行 phase hook 或自定义并行 executor 集成下，多个 `MutationBatch` 如何合并：

```rust
pub enum MergeStrategy {
    Exclusive,
    Commutative,
}
```

## StateMap

状态值的类型擦除容器。

```rust
pub struct StateMap { /* ... */ }
```

### 方法

```rust
fn contains<K: StateKey>(&self) -> bool
fn get<K: StateKey>(&self) -> Option<&K::Value>
fn get_mut<K: StateKey>(&mut self) -> Option<&mut K::Value>
fn insert<K: StateKey>(&mut self, value: K::Value)
fn remove<K: StateKey>(&mut self) -> Option<K::Value>
fn get_or_insert_default<K: StateKey>(&mut self) -> &mut K::Value
```

## Snapshot

传给 hook 和 `ToolCallContext` 的不可变、带 revision 的状态视图：

```rust
pub struct Snapshot {
    pub revision: u64,
    pub ext: Arc<StateMap>,
}
```

### 方法

```rust
fn new(revision: u64, ext: Arc<StateMap>) -> Self
fn revision(&self) -> u64
fn get<K: StateKey>(&self) -> Option<&K::Value>
fn ext(&self) -> &StateMap
```

## StateKeyOptions

注册状态键时的选项：

```rust
pub struct StateKeyOptions {
    pub persistent: bool,
    pub retain_on_uninstall: bool,
    pub scope: KeyScope,
}
```

## PersistedState

存储后端使用的序列化状态格式：

```rust
pub struct PersistedState {
    pub revision: u64,
    pub extensions: HashMap<String, JsonValue>,
}
```

## 相关

- [状态与快照模型](/awaken/zh-cn/explanation/state-and-snapshot-model/)
