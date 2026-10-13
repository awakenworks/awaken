//! Low-level Session identity, source ordering and runtime failure vocabulary.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Serialize};

pub mod error;
pub use error::{RunError, RunErrorClassification, RunErrorKind};

mod monotone;
pub use monotone::{MonotoneAdmission, SourceOrder, monotone_admission};

mod run_command;
pub use run_command::{SessionRunCommandFingerprint, SessionRunReplacement};

mod placement;
pub use placement::SessionRuntimePlacement;
mod recovery;
pub use recovery::SessionRecoveryQuarantine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContractErrorDetail(String);

impl SessionContractErrorDetail {
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl std::fmt::Display for SessionContractErrorDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for SessionContractErrorDetail {
    fn from(detail: String) -> Self {
        Self(detail)
    }
}

impl From<&str> for SessionContractErrorDetail {
    fn from(detail: &str) -> Self {
        Self(detail.to_string())
    }
}

impl From<SessionContractErrorDetail> for String {
    fn from(detail: SessionContractErrorDetail) -> Self {
        detail.0
    }
}

/// Opaque identity of one Session root, which is also the id of its root
/// Thread.
///
/// The value is minted by the Session application and is otherwise an
/// uninterpreted string: this type adds no format rule, so every id already
/// stored in a repository row, WorkQueue item, receipt, or wire payload remains
/// valid. `#[serde(transparent)]` keeps its JSON and column form a bare string.
/// Ordering is lexical for keyset pagination, never admission order.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&String> for SessionId {
    fn from(value: &String) -> Self {
        Self(value.clone())
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for SessionId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&SessionId> for SessionId {
    fn eq(&self, other: &&SessionId) -> bool {
        self == *other
    }
}

impl PartialEq<SessionId> for &SessionId {
    fn eq(&self, other: &SessionId) -> bool {
        *self == other
    }
}

impl PartialEq<SessionId> for str {
    fn eq(&self, other: &SessionId) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<SessionId> for &str {
    fn eq(&self, other: &SessionId) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<SessionId> for String {
    fn eq(&self, other: &SessionId) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<&SessionId> for String {
    fn eq(&self, other: &&SessionId) -> bool {
        self == other.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::SessionId;

    #[test]
    fn wire_form_is_the_bare_string_in_both_directions() {
        // C1 an id is serialized; C2 a historical bare string is decoded.
        // E1 the JSON is exactly the legacy string; E2 decoding admits every
        // stored spelling, including ids outside the minted `sesn_` shape and
        // the empty string, because the newtype adds no validation rule.
        for raw in ["sesn_01HZX", "legacy-thread-id", "with/slash and space", ""] {
            let id = crate::SessionId::from(raw);
            assert_eq!(
                serde_json::to_value(&id).unwrap(),
                serde_json::json!(raw),
                "C1/E1"
            );
            let decoded: SessionId = serde_json::from_value(serde_json::json!(raw)).unwrap();
            assert_eq!(decoded, id, "C2/E2");
            assert_eq!(decoded.as_str(), raw);
        }
        assert!(serde_json::from_value::<SessionId>(serde_json::json!(7)).is_err());
    }

    #[test]
    fn a_containing_record_keeps_its_stored_bytes() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Stored {
            session_id: SessionId,
            session_ids: Vec<SessionId>,
            parent: Option<SessionId>,
        }
        let stored = r#"{"session_id":"sesn_a","session_ids":["sesn_b","c"],"parent":null}"#;
        let decoded: Stored = serde_json::from_str(stored).unwrap();
        assert_eq!(decoded.session_id, "sesn_a");
        assert_eq!(serde_json::to_string(&decoded).unwrap(), stored);
    }

    #[test]
    fn keyed_collections_are_queried_by_borrowed_string() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(crate::SessionId::from("b"), 2);
        map.insert(crate::SessionId::from("a"), 1);
        assert_eq!(map.get("a"), Some(&1));
        assert_eq!(
            map.keys().map(SessionId::as_str).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}

/// Monotonic root revision for every mutation of one Session aggregate.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);
