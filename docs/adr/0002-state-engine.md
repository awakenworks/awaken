# ADR-0002: State Engine Design

- **Status**: Implemented
- **Date**: 2026-03-21

## Context

The state engine underpins all plugin state, runtime bookkeeping, and cross-phase communication. Alternatives considered: JSON-patch document model (uniform serialization but no compile-time safety), `HashMap<String, Box<dyn Any>>` (simpler but manual downcasting), pessimistic `RwLock` guards.

## Decision

**Typed heterogeneous state via `StateKey` + `TypedMap`**: Each key is a Rust type with compile-time name, value, update, and reducer. Values stored in a type-erased `TypedMap`, accessed via generic methods at zero runtime cost.

**`StateStore` is an in-memory state container**, not a persistence layer. It manages snapshots, revision tracking, commit validation, and plugin registry. Persistence is handled externally via `export_persisted()` / `restore_persisted()` — the store itself has no I/O.

**Immutable snapshots via `Arc<StateMap>`**: Reading state always goes through a `Snapshot`. No mutable reference to the live store is exposed. This decouples readers from writers — hooks can hold snapshots without blocking commits. Cost: `StateMap::clone()` per commit (negligible at tens-per-phase frequency).

**Optimistic locking via revision counter**: `StateStore` tracks `revision: u64`. `MutationBatch` can carry `base_revision`; mismatches are rejected. Exists to catch logic errors (stale snapshots), not to serialize concurrent access.

**Plugin-owned key lifecycle**: Keys registered via `PluginRegistrar` during install; tracked per-plugin. Uninstall clears owned keys (unless `retain_on_uninstall`). Unknown keys rejected at commit.

## Consequences

- Compile-time safety: wrong key type = compilation error
- `apply(value, update)` reducer enables arbitrarily complex state transitions
- Revision detection is whole-store granularity; per-key versioning can be added later without breaking API
