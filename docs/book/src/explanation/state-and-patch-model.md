# Immutable State Management

`tirea-state` provides typed access to JSON state with automatic patch collection, enabling deterministic state transitions and full replay capability.

## The Patch Model

State is never mutated directly. Instead, changes are described as **patches** — serializable records of operations:

```text
State' = apply_patch(State, Patch)
```

A `Patch` contains a list of `Op` (operations), each targeting a specific path in the JSON document.

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

## Key Types

- **`Patch`** — A container of operations. Created via `Patch::new().with_op(...)` or collected automatically from typed state access.
- **`Op`** — A single atomic operation: `Set`, `Delete`, `Append`, `MergeObject`, `Increment`, `Decrement`, `Insert`, `Remove`.
- **`Path`** — A path into a JSON document, e.g., `path!("users", 0, "name")`.
- **`apply_patch`** / **`apply_patches`** — Pure functions that produce new state from old state + patches.

## StateManager

`StateManager` manages immutable state with patch history:

- Tracks all applied patches with timestamps
- Supports replay to any point in time via `apply_patches`
- Provides conflict detection between concurrent patches via `detect_conflicts`

## JsonWriter

For dynamic JSON manipulation without typed structs, use `JsonWriter`:

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

## Conflict Detection

When multiple patches modify overlapping paths, `detect_conflicts` identifies the conflicts:

- **`compute_touched`** — Determines which paths a patch affects
- **`detect_conflicts`** — Compares two sets of touched paths to find overlaps
- **`ConflictKind`** — Describes the type of conflict (e.g., both write to same path)
