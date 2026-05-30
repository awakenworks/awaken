//! Canonical event input & identity vocabulary.
//!
//! runtime-contract owns what a writer constructs and what locates an event:
//! `CanonicalEventDraft`/`AppendOptions` (named by the commit-coordinator write
//! boundary), the scope/kind/visibility value types, and `CanonicalEventId`/
//! `EventCursor`. The store-output types (`CanonicalEvent` record, `EventPage`,
//! `AppendResult`, subscription handles) and the store traits are a
//! server/store concern and live in `awaken-server-contract`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Errors returned by canonical event store implementations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EventStoreError {
    /// The provided input violates the event-store contract.
    #[error("validation error: {0}")]
    Validation(String),
    /// The idempotency identity already exists with different append input.
    #[error("idempotency conflict: {0}")]
    IdempotencyConflict(String),
    /// The caller supplied an expected cursor that does not match storage state.
    #[error("expected cursor conflict: {0}")]
    ExpectedCursorConflict(String),
    /// The requested cursor is outside the retained history.
    #[error("cursor expired: {0}")]
    CursorExpired(String),
    /// Storage history is missing data that should still be retained.
    #[error("integrity error: {0}")]
    Integrity(String),
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(String),
    /// A serialization or deserialization error occurred.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Stable canonical event identifier assigned by an [`EventWriter`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalEventId(String);

impl CanonicalEventId {
    /// Create an event id after validating that it is non-empty.
    pub fn new(value: impl Into<String>) -> Result<Self, EventStoreError> {
        let value = value.into();
        reject_blank("event_id", &value)?;
        Ok(Self(value))
    }

    /// Return the opaque id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque cursor for a single [`EventScope`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventCursor(String);

impl EventCursor {
    /// Create an event cursor after validating that it is non-empty.
    pub fn new(value: impl Into<String>) -> Result<Self, EventStoreError> {
        let value = value.into();
        reject_blank("cursor", &value)?;
        Ok(Self(value))
    }

    /// Return the opaque cursor string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Protocol-neutral canonical event kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalEventKind(String);

impl CanonicalEventKind {
    /// Create an event kind after validating that it is non-empty.
    pub fn new(value: impl Into<String>) -> Result<Self, EventStoreError> {
        let value = value.into();
        reject_blank("event_kind", &value)?;
        Ok(Self(value))
    }

    /// Return the event kind string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Query and ordering scope for canonical events.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "scope_type", rename_all = "snake_case")]
pub enum EventScope {
    /// Events for one thread.
    Thread { thread_id: String },
    /// Events for one run activation.
    Run { run_id: String },
}

impl EventScope {
    /// Create a thread scope.
    #[must_use]
    pub fn thread(thread_id: impl Into<String>) -> Self {
        Self::Thread {
            thread_id: thread_id.into(),
        }
    }

    /// Create a run scope.
    #[must_use]
    pub fn run(run_id: impl Into<String>) -> Self {
        Self::Run {
            run_id: run_id.into(),
        }
    }

    /// Return the stable scope family name.
    #[must_use]
    pub const fn family(&self) -> EventScopeFamily {
        match self {
            Self::Thread { .. } => EventScopeFamily::Thread,
            Self::Run { .. } => EventScopeFamily::Run,
        }
    }
}

/// Standard event-scope family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScopeFamily {
    Thread,
    Run,
}

/// Denormalized ids derived from event scopes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScopeIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Visibility and redaction hint for canonical events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventVisibility {
    /// Safe for protocol replay after protocol-specific shaping.
    #[default]
    Public,
    /// Internal server detail.
    Internal,
    /// Audit-oriented detail.
    Audit,
    /// Sensitive data requiring redaction or payload references.
    Sensitive,
}

/// EventStore append input. Store-assigned fields are intentionally absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalEventDraft {
    pub scopes: Vec<EventScope>,
    pub event_kind: CanonicalEventKind,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub origin: String,
    #[serde(default)]
    pub visibility: EventVisibility,
    pub schema_version: u32,
}

impl CanonicalEventDraft {
    /// Create and validate a canonical event draft.
    pub fn new(
        scopes: Vec<EventScope>,
        event_kind: CanonicalEventKind,
        payload: Value,
        origin: impl Into<String>,
    ) -> Result<Self, EventStoreError> {
        let draft = Self {
            scopes,
            event_kind,
            payload,
            causation_id: None,
            correlation_id: None,
            origin: origin.into(),
            visibility: EventVisibility::default(),
            schema_version: 1,
        };
        draft.validate()?;
        Ok(draft)
    }

    /// Validate scope membership, origin, and schema version.
    pub fn validate(&self) -> Result<(), EventStoreError> {
        validate_scope_set(&self.scopes)?;
        reject_blank("origin", &self.origin)?;
        if self.schema_version == 0 {
            return Err(EventStoreError::Validation(
                "schema_version must be greater than 0".to_string(),
            ));
        }
        Ok(())
    }

    /// Return denormalized ids derived from scopes.
    pub fn scope_ids(&self) -> Result<EventScopeIds, EventStoreError> {
        derive_scope_ids(&self.scopes)
    }

    /// Idempotency equality basis per ADR-0034 D5: scope set, event_kind,
    /// canonical payload, visibility, causation_id, correlation_id. Excludes
    /// `origin` and `schema_version` so retries that differ only in those
    /// fields return the original event instead of IdempotencyConflict.
    pub fn idempotency_digest(&self) -> Result<Vec<u8>, EventStoreError> {
        #[derive(Serialize)]
        struct Basis<'a> {
            scopes: &'a Vec<EventScope>,
            event_kind: &'a CanonicalEventKind,
            payload: &'a Value,
            visibility: &'a EventVisibility,
            causation_id: &'a Option<String>,
            correlation_id: &'a Option<String>,
        }
        serde_json::to_vec(&Basis {
            scopes: &self.scopes,
            event_kind: &self.event_kind,
            payload: &self.payload,
            visibility: &self.visibility,
            causation_id: &self.causation_id,
            correlation_id: &self.correlation_id,
        })
        .map_err(|error| EventStoreError::Serialization(error.to_string()))
    }
}

/// Options supplied when appending a canonical event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub expected_prior_cursors: BTreeMap<EventScope, EventCursor>,
}

impl AppendOptions {
    /// Validate append options before passing them to a store.
    pub fn validate(&self) -> Result<(), EventStoreError> {
        if self.idempotency_key.is_some() {
            reject_blank("writer_id", self.writer_id.as_deref().unwrap_or_default())?;
        }
        if let Some(key) = self.idempotency_key.as_deref() {
            reject_blank("idempotency_key", key)?;
        }
        Ok(())
    }

    /// Return the `(writer_id, idempotency_key)` identity when present.
    pub fn idempotency_identity(&self) -> Result<Option<(String, String)>, EventStoreError> {
        self.validate()?;
        Ok(match (&self.writer_id, &self.idempotency_key) {
            (Some(writer_id), Some(key)) => Some((writer_id.clone(), key.clone())),
            _ => None,
        })
    }
}

fn reject_blank(field: &str, value: &str) -> Result<(), EventStoreError> {
    if value.trim().is_empty() {
        return Err(EventStoreError::Validation(format!("{field} is required")));
    }
    Ok(())
}

fn validate_scope_set(scopes: &[EventScope]) -> Result<(), EventStoreError> {
    if scopes.is_empty() {
        return Err(EventStoreError::Validation(
            "at least one event scope is required".to_string(),
        ));
    }
    let mut exact_scopes = BTreeSet::new();
    let mut families = BTreeSet::new();
    for scope in scopes {
        validate_scope(scope)?;
        if !exact_scopes.insert(scope) {
            return Err(EventStoreError::Validation(format!(
                "duplicate event scope: {scope:?}"
            )));
        }
        if !families.insert(scope.family()) {
            return Err(EventStoreError::Validation(format!(
                "duplicate event scope family: {:?}",
                scope.family()
            )));
        }
    }
    derive_scope_ids(scopes)?;
    Ok(())
}

fn validate_scope(scope: &EventScope) -> Result<(), EventStoreError> {
    match scope {
        EventScope::Thread { thread_id } => reject_blank("thread_id", thread_id),
        EventScope::Run { run_id } => reject_blank("run_id", run_id),
    }
}

fn derive_scope_ids(scopes: &[EventScope]) -> Result<EventScopeIds, EventStoreError> {
    let mut ids = EventScopeIds::default();
    for scope in scopes {
        match scope {
            EventScope::Thread { thread_id } => set_optional_id(
                &mut ids.thread_id,
                thread_id,
                "thread_id contradicts scope membership",
            )?,
            EventScope::Run { run_id } => {
                set_optional_id(
                    &mut ids.run_id,
                    run_id,
                    "run_id contradicts scope membership",
                )?;
            }
        }
    }
    Ok(ids)
}

fn set_optional_id(
    slot: &mut Option<String>,
    value: &str,
    error: &str,
) -> Result<(), EventStoreError> {
    match slot {
        Some(existing) if existing != value => Err(EventStoreError::Validation(error.to_string())),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value.to_string());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind() -> CanonicalEventKind {
        CanonicalEventKind::new("RunStarted").unwrap()
    }

    #[test]
    fn draft_requires_at_least_one_scope() {
        let err = CanonicalEventDraft::new(Vec::new(), kind(), Value::Null, "server").unwrap_err();
        assert!(matches!(err, EventStoreError::Validation(message) if message.contains("scope")));
    }

    #[test]
    fn draft_rejects_duplicate_scope_family() {
        let err = CanonicalEventDraft::new(
            vec![EventScope::thread("t1"), EventScope::thread("t2")],
            kind(),
            Value::Null,
            "server",
        )
        .unwrap_err();
        assert!(matches!(err, EventStoreError::Validation(message) if message.contains("family")));
    }

    #[test]
    fn draft_derives_scope_ids() {
        let draft = CanonicalEventDraft::new(
            vec![EventScope::thread("t1"), EventScope::run("r1")],
            kind(),
            Value::Null,
            "server",
        )
        .unwrap();
        let ids = draft.scope_ids().unwrap();
        assert_eq!(ids.thread_id.as_deref(), Some("t1"));
        assert_eq!(ids.run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn append_options_require_writer_for_idempotency() {
        let options = AppendOptions {
            writer_id: None,
            idempotency_key: Some("key-1".into()),
            expected_prior_cursors: BTreeMap::new(),
        };
        let err = options.validate().unwrap_err();
        assert!(
            matches!(err, EventStoreError::Validation(message) if message.contains("writer_id"))
        );
    }

    #[test]
    fn append_options_return_idempotency_identity() {
        let options = AppendOptions {
            writer_id: Some("writer".into()),
            idempotency_key: Some("key-1".into()),
            expected_prior_cursors: BTreeMap::new(),
        };
        assert_eq!(
            options.idempotency_identity().unwrap(),
            Some(("writer".to_string(), "key-1".to_string()))
        );
    }

    #[test]
    fn idempotency_digest_ignores_origin_and_schema_version() {
        let mut a = CanonicalEventDraft::new(
            vec![EventScope::thread("t1")],
            kind(),
            serde_json::json!({"x": 1}),
            "server",
        )
        .unwrap();
        let mut b = a.clone();
        b.origin = "ai-sdk".to_string();
        b.schema_version = 17;
        assert_eq!(
            a.idempotency_digest().unwrap(),
            b.idempotency_digest().unwrap()
        );

        // Different payload must produce a different digest.
        a.payload = serde_json::json!({"x": 2});
        assert_ne!(
            a.idempotency_digest().unwrap(),
            b.idempotency_digest().unwrap()
        );
    }

    #[test]
    fn idempotency_digest_distinguishes_d5_fields() {
        let base = CanonicalEventDraft::new(
            vec![EventScope::thread("t1")],
            kind(),
            serde_json::json!({}),
            "server",
        )
        .unwrap();
        let base_digest = base.idempotency_digest().unwrap();

        let mut other_scope = base.clone();
        other_scope.scopes = vec![EventScope::thread("t2")];
        assert_ne!(base_digest, other_scope.idempotency_digest().unwrap());

        let mut other_visibility = base.clone();
        other_visibility.visibility = EventVisibility::Internal;
        assert_ne!(base_digest, other_visibility.idempotency_digest().unwrap());

        let mut other_causation = base.clone();
        other_causation.causation_id = Some("evt_prev".into());
        assert_ne!(base_digest, other_causation.idempotency_digest().unwrap());

        let mut other_correlation = base.clone();
        other_correlation.correlation_id = Some("corr-1".into());
        assert_ne!(base_digest, other_correlation.idempotency_digest().unwrap());
    }

    #[test]
    fn opaque_cursor_roundtrips_without_exposing_structure() {
        let cursor = EventCursor::new("evtcur_opaque").unwrap();
        let encoded = serde_json::to_string(&cursor).unwrap();
        assert_eq!(encoded, "\"evtcur_opaque\"");
        let decoded: EventCursor = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.as_str(), "evtcur_opaque");
    }
}
