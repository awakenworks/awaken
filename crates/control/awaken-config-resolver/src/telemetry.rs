// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Static owner: the authored Workspace narrowing of the content-capture ceiling and its
// @formal reason: repository admission. Dynamic path: a write is admitted only at the exact successor
// @formal reason: revision and only when it does not exceed the deployment ceiling; readback rejects a
// @formal reason: row relabelled to another Workspace. Evidence: analytics.observability.projection_only
// @formal reason: plus this module's decision tables and both store adapters' CAS tests; a
// @formal reason: product-requirement classification, not a direct proof claim.
//! The Workspace layer of the content-capture ceiling (ADR-0050 D3, amended
//! 2026-10-09): the authored, revisioned [`WorkspaceTelemetryConfiguration`]
//! and its repository port. Control serves it to the Run boundary through the
//! governance `TelemetryNarrowingSource` port, where the execution host
//! composes it with the deployment ceiling.

use awaken_runtime_contract::ContentCapture;
use serde::{Deserialize, Serialize};

/// One Workspace's authored telemetry narrowing. `revision` is the
/// compare-and-set generation: 0 means never written, every write advances it
/// by exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTelemetryConfiguration {
    pub workspace_id: String,
    pub revision: u64,
    /// The most content this Workspace permits; `None` inherits the ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_capture: Option<ContentCapture>,
}

impl WorkspaceTelemetryConfiguration {
    /// The unwritten configuration of `workspace_id`: no narrowing.
    #[must_use]
    pub fn new(workspace_id: String) -> Self {
        Self {
            workspace_id,
            revision: 0,
            content_capture: None,
        }
    }

    /// Structural validity of a stored or proposed configuration.
    pub fn validate(&self) -> Result<(), TelemetryConfigError> {
        if self.workspace_id.trim().is_empty() {
            return Err(TelemetryConfigError::Invalid(
                "telemetry configuration requires a Workspace".into(),
            ));
        }
        Ok(())
    }

    /// Replace the narrowing for the write that follows `expected`. The new
    /// level must stay within `ceiling`, the binding top-layer level: a lower
    /// layer may only tighten, so a wider value is rejected here.
    pub fn narrowed(
        &self,
        expected: u64,
        content_capture: Option<ContentCapture>,
        ceiling: ContentCapture,
    ) -> Result<Self, TelemetryConfigError> {
        if self.revision != expected {
            return Err(TelemetryConfigError::Conflict);
        }
        if content_capture.is_some_and(|level| level > ceiling) {
            return Err(TelemetryConfigError::Invalid(format!(
                "content_capture may only tighten the deployment ceiling `{}`",
                capture_name(ceiling)
            )));
        }
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| TelemetryConfigError::Invalid("revision exhausted".into()))?;
        Ok(Self {
            workspace_id: self.workspace_id.clone(),
            revision,
            content_capture,
        })
    }
}

fn capture_name(level: ContentCapture) -> &'static str {
    match level {
        ContentCapture::Off => "off",
        ContentCapture::Structured => "structured",
        ContentCapture::Full => "full",
    }
}

/// A Workspace telemetry configuration operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TelemetryConfigError {
    #[error("telemetry configuration revision conflict")]
    Conflict,
    #[error("invalid telemetry configuration: {0}")]
    Invalid(String),
    #[error("telemetry configuration storage failure: {0}")]
    Storage(String),
}

/// Durable store of Workspace telemetry configurations. SQLite and PostgreSQL
/// adapters implement it in the admin store.
#[async_trait::async_trait]
pub trait WorkspaceTelemetryRepository: Send + Sync {
    /// The configuration `workspace` authored, if any.
    async fn get_telemetry_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<WorkspaceTelemetryConfiguration>, TelemetryConfigError>;
    /// Write `next` iff the stored revision is `expected` (0 = absent).
    async fn put_telemetry_configuration(
        &self,
        expected: u64,
        next: &WorkspaceTelemetryConfiguration,
    ) -> Result<(), TelemetryConfigError>;
}

/// The admission every adapter applies before its compare-and-set: a valid
/// configuration whose revision is the exact successor of `expected`.
pub fn validate_telemetry_configuration_write(
    expected: u64,
    next: &WorkspaceTelemetryConfiguration,
) -> Result<(), TelemetryConfigError> {
    next.validate()?;
    if expected.checked_add(1) != Some(next.revision) {
        return Err(TelemetryConfigError::Invalid(
            "telemetry configuration revision must advance by one".into(),
        ));
    }
    Ok(())
}

/// Validate a row read back from storage against the key it was read by.
pub fn validate_stored_telemetry_configuration(
    workspace: &str,
    revision: u64,
    stored: &WorkspaceTelemetryConfiguration,
) -> Result<(), TelemetryConfigError> {
    stored.validate()?;
    if stored.workspace_id != workspace || stored.revision != revision || revision == 0 {
        return Err(TelemetryConfigError::Storage(
            "telemetry stored scope/revision mismatch".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVELS: [ContentCapture; 3] = [
        ContentCapture::Off,
        ContentCapture::Structured,
        ContentCapture::Full,
    ];

    #[test]
    fn narrowed_admits_only_tightening_at_the_expected_revision() {
        // Causes: C1 expected revision matches; C2 requested level (unset or
        // each level) within the ceiling (each level). Effects: E1 next
        // revision carrying the level; E2 Conflict; E3 Invalid (wider than the
        // ceiling). Rules: C1&C2->E1, !C1->E2, C1&!C2->E3. Exhaustive table.
        let current = WorkspaceTelemetryConfiguration::new("ws".into());
        for ceiling in LEVELS {
            for requested in [
                None,
                Some(ContentCapture::Off),
                Some(ContentCapture::Structured),
                Some(ContentCapture::Full),
            ] {
                let result = current.narrowed(0, requested, ceiling);
                if requested.is_some_and(|level| level > ceiling) {
                    assert!(
                        matches!(result, Err(TelemetryConfigError::Invalid(_))),
                        "E3"
                    );
                } else {
                    let next = result.expect("E1");
                    assert_eq!((next.revision, next.content_capture), (1, requested));
                }
                assert_eq!(
                    current.narrowed(1, requested, ceiling),
                    Err(TelemetryConfigError::Conflict),
                    "E2"
                );
            }
        }
        let mut exhausted = current.clone();
        exhausted.revision = u64::MAX;
        assert!(matches!(
            exhausted.narrowed(u64::MAX, None, ContentCapture::Full),
            Err(TelemetryConfigError::Invalid(_))
        ));
    }

    #[test]
    fn write_and_readback_admission_tables() {
        let mut next = WorkspaceTelemetryConfiguration::new("ws".into());
        next.revision = 1;
        assert_eq!(validate_telemetry_configuration_write(0, &next), Ok(()));
        assert!(validate_telemetry_configuration_write(1, &next).is_err());
        assert!(validate_telemetry_configuration_write(u64::MAX, &next).is_err());
        let blank = WorkspaceTelemetryConfiguration {
            workspace_id: " ".into(),
            revision: 1,
            content_capture: None,
        };
        assert!(validate_telemetry_configuration_write(0, &blank).is_err());

        assert_eq!(
            validate_stored_telemetry_configuration("ws", 1, &next),
            Ok(())
        );
        assert!(validate_stored_telemetry_configuration("other", 1, &next).is_err());
        assert!(validate_stored_telemetry_configuration("ws", 2, &next).is_err());
        next.revision = 0;
        assert!(validate_stored_telemetry_configuration("ws", 0, &next).is_err());
    }

    #[test]
    fn configuration_wire_shape_is_closed() {
        let config = WorkspaceTelemetryConfiguration::new("ws".into())
            .narrowed(0, Some(ContentCapture::Off), ContentCapture::Full)
            .unwrap();
        let wire = serde_json::to_value(&config).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"workspace_id": "ws", "revision": 1, "content_capture": "off"})
        );
        assert_eq!(
            serde_json::from_value::<WorkspaceTelemetryConfiguration>(wire).unwrap(),
            config
        );
        let inherit = WorkspaceTelemetryConfiguration::new("ws".into());
        assert_eq!(
            serde_json::to_value(&inherit).unwrap(),
            serde_json::json!({"workspace_id": "ws", "revision": 0})
        );
        assert!(
            serde_json::from_value::<WorkspaceTelemetryConfiguration>(serde_json::json!({
                "workspace_id": "ws", "revision": 1, "retention_days": 7
            }))
            .is_err()
        );
    }
}
