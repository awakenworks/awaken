# Derive Macro Reference

`#[derive(State)]` generates typed state refs, patch collection, and optional reducer wiring.

## Basic Usage

```rust,edition2021
# extern crate serde;
# extern crate serde_json;
# extern crate tirea_state;
# extern crate tirea_state_derive;
use serde::{Deserialize, Serialize};
use tirea_state::{State as StateTrait, StateScope, StateSpec};
use tirea_state_derive::State;

#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(path = "counter", action = "CounterAction", scope = "run")]
struct Counter {
    value: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CounterAction {
    Increment(i64),
}

impl Counter {
    fn reduce(&mut self, action: CounterAction) {
        match action {
            CounterAction::Increment(amount) => self.value += amount,
        }
    }
}

assert_eq!(<Counter as StateTrait>::PATH, "counter");
assert_eq!(<Counter as StateSpec>::SCOPE, StateScope::Run);
```

## Struct Attributes

### `#[tirea(path = "...")]`

Sets canonical state path used by `State::PATH` and `state_of::<T>()`.

### `#[tirea(action = "TypeName")]`

Generates `impl StateSpec for T` with `type Action = TypeName`, delegating reducer to inherent `fn reduce(&mut self, action)`.

### `#[tirea(scope = "thread|run|tool_call")]`

When `action` is set, also generates `StateSpec::SCOPE`.

- default: `thread`
- valid values: `thread`, `run`, `tool_call`

## Field Attributes

### `#[tirea(rename = "json_key")]`

Maps Rust field to different JSON key.

### `#[tirea(default = "expr")]`

Uses expression when field is missing.

### `#[tirea(skip)]`

Excludes field from generated ref API.

### `#[tirea(nested)]`

Treats field type as nested `State`, returning nested ref accessors.

### `#[tirea(flatten)]`

Flattens nested struct fields into parent object.

### `#[tirea(lattice)]`

Marks field as CRDT/lattice field.

Generated behavior:

- field diff emits `Op::LatticeMerge`
- generated ref includes `merge_<field>(&T)` helper
- `register_lattice(...)` and `lattice_keys()` are emitted for this type

## Validation Rules

Compile-time errors are raised for invalid combinations:

- `flatten` + `rename`
- `lattice` + `nested`
- `lattice` + `flatten`
- `lattice` on `Option<T>`, `Vec<T>`, `Map<K,V>`
- `flatten` on non-struct/non-`State` field

## Generated API Shape

For included fields, macro generates typed methods on `YourTypeRef<'a>` such as:

- readers: `field()`
- setters: `set_field(...)`
- optional helpers: `field_none()`
- vec helpers: `field_push(...)`
- map helpers (`String` key): `field_insert(key, value)`
- numeric helpers: `increment_field(...)`, `decrement_field(...)`
- delete helpers: `delete_field()`
- nested refs: `nested_field()`

Exact method set depends on field type and attributes.

## Generated Trait Implementations

- `impl State for T`
- `type Ref<'a> = TRef<'a>`
- `const PATH: &'static str`
- `from_value` / `to_value`
- optimized `diff_ops` (field-level)
- optional `impl StateSpec` when `action` is configured
