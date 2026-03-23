# Operations

Operations (`Op`) are atomic state mutations against JSON paths.

## Operation Types

### `Set`

Set a value at path. Creates intermediate objects when needed.

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{Op, path};
use serde_json::json;

let op = Op::set(path!("user", "name"), json!("Alice"));
```

### `Delete`

Delete value at path. No-op when path is absent.

```rust
# extern crate tirea_state;
use tirea_state::{Op, path};

let op = Op::delete(path!("user", "temp_field"));
```

### `Append`

Append to array. Creates array when absent.

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{Op, path};
use serde_json::json;

let op = Op::append(path!("user", "roles"), json!("admin"));
```

Error: `AppendRequiresArray` when target exists but is not array.

### `MergeObject`

Shallow-merge object keys into target object.

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{Op, path};
use serde_json::json;

let op = Op::merge_object(path!("user", "settings"), json!({"theme": "dark"}));
```

Error: `MergeRequiresObject` when target is not object.

### `Increment` / `Decrement`

Numeric arithmetic on existing numeric value.

```rust
# extern crate tirea_state;
use tirea_state::{Op, path};

let inc = Op::increment(path!("counter"), 1i64);
let dec = Op::decrement(path!("counter"), 1i64);
```

Error: `NumericOperationOnNonNumber`.

### `Insert`

Insert into array index (shift right).

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{Op, path};
use serde_json::json;

let op = Op::insert(path!("items"), 0, json!("first"));
```

Error: `IndexOutOfBounds`.

### `Remove`

Remove first matching array element.

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{Op, path};
use serde_json::json;

let op = Op::remove(path!("tags"), json!("deprecated"));
```

Errors: `PathNotFound` when path does not exist; `TypeMismatch` when target exists but is not an array.

### `LatticeMerge`

Merge CRDT/lattice delta at path.

```rust
# extern crate tirea_state;
# extern crate serde_json;
use tirea_state::{Op, path};
use serde_json::json;

let op = Op::lattice_merge(path!("permission_policy", "allowed_tools"), json!(["search"]));
```

Behavior:

- with `LatticeRegistry`: performs registered lattice merge
- without registry: falls back to `Set` semantics

## Number Type

Numeric ops use `Number`:

```rust,edition2021
# extern crate tirea_state;
use tirea_state::Number;

let int_value = Number::from(1_i64);
let float_value = Number::from(1.5_f64);

assert!(int_value.is_int());
assert!(float_value.is_float());
```

`From` is implemented for `i32`, `i64`, `u32`, `u64`, `f32`, `f64`.

## Paths

`path!` builds path segments:

```rust
# extern crate tirea_state;
use tirea_state::path;

let p = path!("users", 0, "name");
let p = path!("settings", "theme");
```

## Apply Semantics

- `apply_patch` / `apply_patches`: plain op application
- `apply_patch_with_registry` / `apply_patches_with_registry`: enables lattice-aware merge for `Op::LatticeMerge`

## Serialization

`Op` serializes with `op` discriminator:

```json
{"op":"set","path":["user","name"],"value":"Alice"}
{"op":"increment","path":["counter"],"amount":1}
{"op":"lattice_merge","path":["permission_policy","allowed_tools"],"value":["search"]}
```
