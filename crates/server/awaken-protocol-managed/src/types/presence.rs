//! Serde support for PATCH fields whose absence, explicit `null`, and value have
//! distinct protocol semantics.

use serde::Deserialize;

pub(crate) fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Deserialize a field that may be omitted but whose explicit JSON `null` is
/// not part of the wire contract.
pub(crate) fn optional_non_null<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}
