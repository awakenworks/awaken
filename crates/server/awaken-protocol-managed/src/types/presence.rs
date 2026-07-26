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
