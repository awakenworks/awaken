> 本文档为中文翻译版本，英文原版请参阅 [Immutable State Management](../../explanation/state-and-patch-model.md)

# 不可变状态管理

`tirea-state` 提供对 JSON 状态的类型化访问，并自动收集补丁（patch），从而实现确定性的状态转换与完整的重放能力。

## 补丁模型

状态从不被直接修改。变更以**补丁**的形式描述——补丁是操作的可序列化记录：

```text
State' = apply_patch(State, Patch)
```

一个 `Patch` 包含一组 `Op`（操作），每个操作针对 JSON 文档中的特定路径。

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{apply_patch, Patch, Op, path};
use serde_json::json;

let state = json!({"count": 0, "name": "counter"});

let patch = Patch::new()
    .with_op(Op::set(path!("count"), json!(10)))
    .with_op(Op::set(path!("updated"), json!(true)));

let new_state = apply_patch(&state, &patch).unwrap();

assert_eq!(new_state["count"], 10);
assert_eq!(new_state["updated"], true);
assert_eq!(state["count"], 0); // Original unchanged
```

## 核心类型

- **`Patch`** — 操作的容器。通过 `Patch::new().with_op(...)` 创建，或由类型化状态访问自动收集。
- **`Op`** — 单个原子操作：`Set`、`Delete`、`Append`、`MergeObject`、`Increment`、`Decrement`、`Insert`、`Remove`、`LatticeMerge`。
- **`Path`** — JSON 文档中的路径，例如 `path!("users", 0, "name")`。
- **`apply_patch`** / **`apply_patches`** — 纯函数，由旧状态和补丁生成新状态。

## StateManager

`StateManager` 管理带有补丁历史的不可变状态：

- 追踪所有已应用的补丁（时间戳为可选项，由调用方通过 `TrackedPatch` 提供，而非由 `StateManager` 自动生成）
- 支持通过 `replay_to(index: usize)` 回放到指定历史索引
- 通过 `detect_conflicts` 提供并发补丁间的冲突检测

## JsonWriter

若需在不使用类型化结构体的情况下动态操作 JSON，可使用 `JsonWriter`：

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{JsonWriter, path};
use serde_json::json;

let mut w = JsonWriter::new();
w.set(path!("user", "name"), json!("Alice"));
w.append(path!("user", "roles"), json!("admin"));
w.increment(path!("user", "login_count"), 1i64);

let patch = w.build();
```

## 冲突检测

当多个补丁修改了重叠路径时，`detect_conflicts` 会识别出相应的冲突：

- **`compute_touched`** — 确定一个补丁影响了哪些路径
- **`detect_conflicts`** — 比较两组受影响路径，找出重叠部分
- **`ConflictKind`** — 描述冲突的类型（例如，两个操作同时写入同一路径）
