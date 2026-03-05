//! Patch application logic.
//!
//! This module contains the pure `apply_patch` function that applies a patch
//! to a JSON document and returns a new document.

use crate::{
    error::{value_type_name, TireaError, TireaResult},
    lattice::LatticeRegistry,
    Number, Op, Patch, Path, Seg,
};
use serde_json::{Map, Value};

/// Apply a patch to a JSON document (pure function).
///
/// This function is deterministic: given the same document and patch,
/// it will always produce the same result.
///
/// # Arguments
///
/// * `doc` - The original document (not modified)
/// * `patch` - The patch to apply
///
/// # Returns
///
/// A new document with all operations applied, or an error if any operation fails.
///
/// # Examples
///
/// ```
/// use tirea_state::{apply_patch, Patch, Op, path};
/// use serde_json::json;
///
/// let doc = json!({"count": 0});
/// let patch = Patch::new()
///     .with_op(Op::set(path!("count"), json!(10)))
///     .with_op(Op::set(path!("name"), json!("test")));
///
/// let new_doc = apply_patch(&doc, &patch).unwrap();
/// assert_eq!(new_doc["count"], 10);
/// assert_eq!(new_doc["name"], "test");
///
/// // Original is unchanged (pure function)
/// assert_eq!(doc["count"], 0);
/// ```
pub fn apply_patch(doc: &Value, patch: &Patch) -> TireaResult<Value> {
    let mut result = doc.clone();
    apply_patch_in_place(&mut result, patch)?;
    Ok(result)
}

/// Apply a patch directly to an existing document.
pub(crate) fn apply_patch_in_place(doc: &mut Value, patch: &Patch) -> TireaResult<()> {
    for op in patch.ops() {
        apply_op(doc, op)?;
    }
    Ok(())
}

/// Apply multiple patches to a JSON document in sequence (pure function).
///
/// This is a convenience function that applies patches one by one.
/// If any patch fails, the error is returned and no further patches are applied.
///
/// # Arguments
///
/// * `doc` - The original document (not modified)
/// * `patches` - Iterator of patches to apply in order
///
/// # Returns
///
/// A new document with all patches applied, or an error if any patch fails.
///
/// # Examples
///
/// ```
/// use tirea_state::{apply_patches, Patch, Op, path};
/// use serde_json::json;
///
/// let doc = json!({"count": 0});
/// let patches = vec![
///     Patch::new().with_op(Op::set(path!("count"), json!(1))),
///     Patch::new().with_op(Op::set(path!("count"), json!(2))),
/// ];
///
/// let new_doc = apply_patches(&doc, patches.iter()).unwrap();
/// assert_eq!(new_doc["count"], 2);
/// ```
pub fn apply_patches<'a>(
    doc: &Value,
    patches: impl IntoIterator<Item = &'a Patch>,
) -> TireaResult<Value> {
    let mut result = doc.clone();
    for patch in patches {
        apply_patch_in_place(&mut result, patch)?;
    }
    Ok(result)
}

/// Apply multiple patches to a JSON document using a `LatticeRegistry` for proper merges.
///
/// `LatticeMerge` ops whose path is registered will perform a real lattice merge;
/// unregistered paths fall back to `Op::Set` semantics.
pub fn apply_patches_with_registry<'a>(
    doc: &Value,
    patches: impl IntoIterator<Item = &'a Patch>,
    registry: &LatticeRegistry,
) -> TireaResult<Value> {
    let mut result = doc.clone();
    for patch in patches {
        apply_patch_in_place_with_registry(&mut result, patch, registry)?;
    }
    Ok(result)
}

/// Apply a patch to a JSON document using a `LatticeRegistry` for proper merges.
///
/// `LatticeMerge` ops whose path is registered will perform a real lattice merge;
/// unregistered paths fall back to `Op::Set` semantics.
pub fn apply_patch_with_registry(
    doc: &Value,
    patch: &Patch,
    registry: &LatticeRegistry,
) -> TireaResult<Value> {
    let mut result = doc.clone();
    apply_patch_in_place_with_registry(&mut result, patch, registry)?;
    Ok(result)
}

/// Apply a patch in-place using a `LatticeRegistry`.
pub(crate) fn apply_patch_in_place_with_registry(
    doc: &mut Value,
    patch: &Patch,
    registry: &LatticeRegistry,
) -> TireaResult<()> {
    for op in patch.ops() {
        apply_op_with_registry(doc, op, registry)?;
    }
    Ok(())
}

/// Apply a single operation using a registry (for `LatticeMerge` ops).
fn apply_op_with_registry(doc: &mut Value, op: &Op, registry: &LatticeRegistry) -> TireaResult<()> {
    match op {
        Op::LatticeMerge { path, value } => {
            if let Some(merger) = registry.get(path) {
                let current = get_at_path(doc, path);
                let merged = merger.merge(current, value)?;
                apply_set(doc, path, merged)
            } else {
                // Unregistered path → fallback to Set
                apply_set(doc, path, value.clone())
            }
        }
        _ => apply_op(doc, op),
    }
}

/// Apply a single operation to a document (mutating).
pub(crate) fn apply_op(doc: &mut Value, op: &Op) -> TireaResult<()> {
    match op {
        Op::Set { path, value } => apply_set(doc, path, value.clone()),
        Op::Delete { path } => apply_delete(doc, path),
        Op::Append { path, value } => apply_append(doc, path, value.clone()),
        Op::MergeObject { path, value } => apply_merge_object(doc, path, value),
        Op::Increment { path, amount } => apply_increment(doc, path, amount),
        Op::Decrement { path, amount } => apply_decrement(doc, path, amount),
        Op::Insert { path, index, value } => apply_insert(doc, path, *index, value.clone()),
        Op::Remove { path, value } => apply_remove(doc, path, value),
        // Without a registry, LatticeMerge falls back to Set semantics
        Op::LatticeMerge { path, value } => apply_set(doc, path, value.clone()),
    }
}

/// Apply a Set operation.
fn apply_set(doc: &mut Value, path: &Path, value: Value) -> TireaResult<()> {
    if path.is_empty() {
        *doc = value;
        return Ok(());
    }

    set_at_path(doc, path.segments(), value, path)
}

/// Recursively set a value at a path, creating intermediate objects as needed.
fn set_at_path(
    current: &mut Value,
    segments: &[Seg],
    value: Value,
    full_path: &Path,
) -> TireaResult<()> {
    match segments {
        [] => {
            *current = value;
            Ok(())
        }
        [Seg::Key(key), rest @ ..] => {
            // Create intermediate object if needed
            if !current.is_object() {
                *current = Value::Object(Map::new());
            }

            let obj = current.as_object_mut().unwrap();

            if rest.is_empty() {
                obj.insert(key.clone(), value);
            } else {
                let entry = obj.entry(key.clone()).or_insert(Value::Null);
                set_at_path(entry, rest, value, full_path)?;
            }
            Ok(())
        }
        [Seg::Index(idx), rest @ ..] => {
            // Check type first to avoid borrow issues
            if !current.is_array() {
                return Err(TireaError::type_mismatch(
                    full_path.clone(),
                    "array",
                    value_type_name(current),
                ));
            }

            let arr = current.as_array_mut().unwrap();

            if *idx >= arr.len() {
                return Err(TireaError::index_out_of_bounds(
                    full_path.clone(),
                    *idx,
                    arr.len(),
                ));
            }

            if rest.is_empty() {
                arr[*idx] = value;
            } else {
                set_at_path(&mut arr[*idx], rest, value, full_path)?;
            }
            Ok(())
        }
    }
}

/// Apply a Delete operation (no-op if path doesn't exist).
fn apply_delete(doc: &mut Value, path: &Path) -> TireaResult<()> {
    if path.is_empty() {
        *doc = Value::Null;
        return Ok(());
    }

    // Delete is a no-op if path doesn't exist
    let _ = delete_at_path(doc, path.segments());
    Ok(())
}

/// Try to delete a value at a path. Returns true if deleted, false if not found.
fn delete_at_path(current: &mut Value, segments: &[Seg]) -> bool {
    match segments {
        [] => false,
        [Seg::Key(key)] => {
            if let Some(obj) = current.as_object_mut() {
                obj.remove(key).is_some()
            } else {
                false
            }
        }
        [Seg::Index(idx)] => {
            if let Some(arr) = current.as_array_mut() {
                if *idx < arr.len() {
                    arr.remove(*idx);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
        [Seg::Key(key), rest @ ..] => {
            if let Some(obj) = current.as_object_mut() {
                if let Some(child) = obj.get_mut(key) {
                    delete_at_path(child, rest)
                } else {
                    false
                }
            } else {
                false
            }
        }
        [Seg::Index(idx), rest @ ..] => {
            if let Some(arr) = current.as_array_mut() {
                if let Some(child) = arr.get_mut(*idx) {
                    delete_at_path(child, rest)
                } else {
                    false
                }
            } else {
                false
            }
        }
    }
}

/// Apply an Append operation.
fn apply_append(doc: &mut Value, path: &Path, value: Value) -> TireaResult<()> {
    let target = get_or_create_at_path(doc, path, 0, || Value::Array(vec![]))?;

    match target {
        Value::Array(arr) => {
            arr.push(value);
            Ok(())
        }
        _ => Err(TireaError::append_requires_array(path.clone())),
    }
}

/// Apply a MergeObject operation.
fn apply_merge_object(doc: &mut Value, path: &Path, value: &Value) -> TireaResult<()> {
    let merge_value = value
        .as_object()
        .ok_or_else(|| TireaError::merge_requires_object(path.clone()))?;

    let target = get_or_create_at_path(doc, path, 0, || Value::Object(Map::new()))?;

    match target {
        Value::Object(obj) => {
            for (k, v) in merge_value {
                obj.insert(k.clone(), v.clone());
            }
            Ok(())
        }
        _ => Err(TireaError::merge_requires_object(path.clone())),
    }
}

/// Apply an Increment operation.
fn apply_increment(doc: &mut Value, path: &Path, amount: &Number) -> TireaResult<()> {
    let target = get_at_path_mut(doc, path.segments())
        .ok_or_else(|| TireaError::path_not_found(path.clone()))?;

    match target {
        Value::Number(n) => {
            let result = if let Some(i) = n.as_i64() {
                match amount {
                    Number::Int(a) => {
                        let value = i.checked_add(*a).ok_or_else(|| {
                            TireaError::invalid_operation(format!(
                                "increment overflow at {path}: {i} + {a}"
                            ))
                        })?;
                        Value::Number(value.into())
                    }
                    Number::Float(a) => {
                        Value::Number(finite_number_from_f64(path, i as f64 + a, "increment")?)
                    }
                }
            } else if let Some(f) = n.as_f64() {
                Value::Number(finite_number_from_f64(
                    path,
                    f + amount.as_f64(),
                    "increment",
                )?)
            } else {
                return Err(TireaError::numeric_on_non_number(path.clone()));
            };
            *target = result;
            Ok(())
        }
        _ => Err(TireaError::numeric_on_non_number(path.clone())),
    }
}

/// Apply a Decrement operation.
fn apply_decrement(doc: &mut Value, path: &Path, amount: &Number) -> TireaResult<()> {
    let target = get_at_path_mut(doc, path.segments())
        .ok_or_else(|| TireaError::path_not_found(path.clone()))?;

    match target {
        Value::Number(n) => {
            let result = if let Some(i) = n.as_i64() {
                match amount {
                    Number::Int(a) => {
                        let value = i.checked_sub(*a).ok_or_else(|| {
                            TireaError::invalid_operation(format!(
                                "decrement overflow at {path}: {i} - {a}"
                            ))
                        })?;
                        Value::Number(value.into())
                    }
                    Number::Float(a) => {
                        Value::Number(finite_number_from_f64(path, i as f64 - a, "decrement")?)
                    }
                }
            } else if let Some(f) = n.as_f64() {
                Value::Number(finite_number_from_f64(
                    path,
                    f - amount.as_f64(),
                    "decrement",
                )?)
            } else {
                return Err(TireaError::numeric_on_non_number(path.clone()));
            };
            *target = result;
            Ok(())
        }
        _ => Err(TireaError::numeric_on_non_number(path.clone())),
    }
}

/// Apply an Insert operation.
fn apply_insert(doc: &mut Value, path: &Path, index: usize, value: Value) -> TireaResult<()> {
    let target = get_at_path_mut(doc, path.segments())
        .ok_or_else(|| TireaError::path_not_found(path.clone()))?;

    match target {
        Value::Array(arr) => {
            if index > arr.len() {
                return Err(TireaError::index_out_of_bounds(
                    path.clone(),
                    index,
                    arr.len(),
                ));
            }
            arr.insert(index, value);
            Ok(())
        }
        _ => Err(TireaError::type_mismatch(
            path.clone(),
            "array",
            value_type_name(target),
        )),
    }
}

fn finite_number_from_f64(path: &Path, value: f64, op: &str) -> TireaResult<serde_json::Number> {
    if !value.is_finite() {
        return Err(TireaError::invalid_operation(format!(
            "{op} produced non-finite value at {path}"
        )));
    }

    serde_json::Number::from_f64(value).ok_or_else(|| {
        TireaError::invalid_operation(format!("{op} produced non-representable value at {path}"))
    })
}

/// Apply a Remove operation (removes first occurrence).
fn apply_remove(doc: &mut Value, path: &Path, value: &Value) -> TireaResult<()> {
    let target = get_at_path_mut(doc, path.segments())
        .ok_or_else(|| TireaError::path_not_found(path.clone()))?;

    match target {
        Value::Array(arr) => {
            if let Some(pos) = arr.iter().position(|v| v == value) {
                arr.remove(pos);
            }
            Ok(())
        }
        _ => Err(TireaError::type_mismatch(
            path.clone(),
            "array",
            value_type_name(target),
        )),
    }
}

/// Get a mutable reference to a value at a path.
fn get_at_path_mut<'a>(current: &'a mut Value, segments: &[Seg]) -> Option<&'a mut Value> {
    match segments {
        [] => Some(current),
        [Seg::Key(key), rest @ ..] => {
            let obj = current.as_object_mut()?;
            let child = obj.get_mut(key)?;
            get_at_path_mut(child, rest)
        }
        [Seg::Index(idx), rest @ ..] => {
            let arr = current.as_array_mut()?;
            let child = arr.get_mut(*idx)?;
            get_at_path_mut(child, rest)
        }
    }
}

/// Get or create a value at a path.
fn get_or_create_at_path<'a, F>(
    current: &'a mut Value,
    full_path: &Path,
    consumed: usize,
    default: F,
) -> TireaResult<&'a mut Value>
where
    F: Fn() -> Value,
{
    let segments = &full_path.segments()[consumed..];
    match segments {
        [] => {
            if current.is_null() {
                *current = default();
            }
            Ok(current)
        }
        [Seg::Key(key), ..] => {
            if !current.is_object() {
                *current = Value::Object(Map::new());
            }

            let obj = current.as_object_mut().unwrap();
            let entry = obj.entry(key.clone()).or_insert(Value::Null);
            get_or_create_at_path(entry, full_path, consumed + 1, default)
        }
        [Seg::Index(idx), ..] => {
            // Build the path up to and including this segment for error reporting
            let error_path = Path::from_segments(full_path.segments()[..=consumed].to_vec());

            // Check type first to avoid borrow issues
            if !current.is_array() {
                return Err(TireaError::type_mismatch(
                    error_path,
                    "array",
                    value_type_name(current),
                ));
            }

            let arr = current.as_array_mut().unwrap();

            if *idx >= arr.len() {
                return Err(TireaError::index_out_of_bounds(error_path, *idx, arr.len()));
            }

            get_or_create_at_path(&mut arr[*idx], full_path, consumed + 1, default)
        }
    }
}

/// Get a reference to a value at a path (for reading).
pub fn get_at_path<'a>(doc: &'a Value, path: &Path) -> Option<&'a Value> {
    let mut current = doc;
    for seg in path.segments() {
        match seg {
            Seg::Key(key) => {
                current = current.get(key)?;
            }
            Seg::Index(idx) => {
                current = current.get(idx)?;
            }
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path;
    use serde_json::json;

    #[test]
    fn test_apply_set() {
        let doc = json!({});
        let patch = Patch::new().with_op(Op::set(path!("name"), json!("Alice")));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["name"], "Alice");
    }

    #[test]
    fn test_apply_set_creates_intermediate_objects() {
        let doc = json!({});
        let patch = Patch::new().with_op(Op::set(path!("a", "b", "c"), json!(42)));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["a"]["b"]["c"], 42);
    }

    #[test]
    fn test_apply_set_array_oob() {
        let doc = json!({"arr": [1, 2, 3]});
        let patch = Patch::new().with_op(Op::set(path!("arr", 10), json!(42)));
        let result = apply_patch(&doc, &patch);
        assert!(matches!(result, Err(TireaError::IndexOutOfBounds { .. })));
    }

    #[test]
    fn test_apply_delete_noop() {
        let doc = json!({"x": 1});
        let patch = Patch::new().with_op(Op::delete(path!("nonexistent")));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result, json!({"x": 1}));
    }

    #[test]
    fn test_apply_delete_existing() {
        let doc = json!({"x": 1, "y": 2});
        let patch = Patch::new().with_op(Op::delete(path!("x")));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result, json!({"y": 2}));
    }

    #[test]
    fn test_apply_append() {
        let doc = json!({"items": [1, 2]});
        let patch = Patch::new().with_op(Op::append(path!("items"), json!(3)));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["items"], json!([1, 2, 3]));
    }

    #[test]
    fn test_apply_append_creates_array() {
        let doc = json!({});
        let patch = Patch::new().with_op(Op::append(path!("items"), json!(1)));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["items"], json!([1]));
    }

    #[test]
    fn test_apply_merge_object() {
        let doc = json!({"user": {"name": "Alice"}});
        let patch = Patch::new().with_op(Op::merge_object(
            path!("user"),
            json!({"age": 30, "email": "alice@example.com"}),
        ));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["user"]["name"], "Alice");
        assert_eq!(result["user"]["age"], 30);
        assert_eq!(result["user"]["email"], "alice@example.com");
    }

    #[test]
    fn test_apply_increment() {
        let doc = json!({"count": 5});
        let patch = Patch::new().with_op(Op::increment(path!("count"), 3i64));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["count"], 8);
    }

    #[test]
    fn test_apply_decrement() {
        let doc = json!({"count": 10});
        let patch = Patch::new().with_op(Op::decrement(path!("count"), 3i64));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["count"], 7);
    }

    #[test]
    fn test_apply_increment_nan_amount_returns_error() {
        let doc = json!({"count": 5});
        let patch = Patch::new().with_op(Op::increment(path!("count"), f64::NAN));
        let result = apply_patch(&doc, &patch);
        assert!(
            matches!(result, Err(TireaError::InvalidOperation { .. })),
            "expected InvalidOperation, got: {result:?}"
        );
    }

    #[test]
    fn test_apply_decrement_infinite_amount_returns_error() {
        let doc = json!({"count": 5});
        let patch = Patch::new().with_op(Op::decrement(path!("count"), f64::INFINITY));
        let result = apply_patch(&doc, &patch);
        assert!(
            matches!(result, Err(TireaError::InvalidOperation { .. })),
            "expected InvalidOperation, got: {result:?}"
        );
    }

    #[test]
    fn test_apply_insert() {
        let doc = json!({"arr": [1, 2, 3]});
        let patch = Patch::new().with_op(Op::insert(path!("arr"), 1, json!(99)));
        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["arr"], json!([1, 99, 2, 3]));
    }

    #[test]
    fn test_apply_remove() {
        let doc = json!({"arr": [1, 2, 3, 2]});
        let patch = Patch::new().with_op(Op::remove(path!("arr"), json!(2)));
        let result = apply_patch(&doc, &patch).unwrap();
        // Removes first occurrence only
        assert_eq!(result["arr"], json!([1, 3, 2]));
    }

    #[test]
    fn test_apply_is_pure() {
        let doc = json!({"x": 1});
        let patch = Patch::new().with_op(Op::set(path!("x"), json!(2)));

        let _ = apply_patch(&doc, &patch).unwrap();

        // Original unchanged
        assert_eq!(doc["x"], 1);
    }

    #[test]
    fn test_get_at_path() {
        let doc = json!({"a": {"b": {"c": 42}}});
        let value = get_at_path(&doc, &path!("a", "b", "c"));
        assert_eq!(value, Some(&json!(42)));

        let missing = get_at_path(&doc, &path!("a", "x"));
        assert_eq!(missing, None);
    }
}
