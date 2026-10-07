// @formal surface obligation=capture.persistence_redaction_selection_is_total_exact_and_non_widening
// @formal reason: Holds the deployment capture ceiling derived by the redaction selector and the consent port;
// @formal reason: per-request consent may only narrow it.
//! CAPTURE capability of [`SharedHost`] (ADR-0050): the Host-owned
//! subject-tagged content sink, the deployment-resolved capture ceiling, and
//! the Control consent read port.

use crate::host::*;

/// Content-capture wiring. Consumer: Session context assembly (per-Run
/// capture decision) and the protocol adapters' ceiling projection.
pub(crate) struct ContentCaptureCapability {
    /// The one Host-owned subject-tagged captured-content sink (ADR-0050).
    /// Process startup may install it after the shared Host is created; sessions
    /// snapshot the current sink when they are created. `None` = spans only.
    pub(crate) capture_sink:
        std::sync::RwLock<Option<Arc<dyn awaken_runtime_contract::CaptureSink>>>,
    /// Deployment-resolved capture ceiling/redactor. Per-request consent and
    /// subject attribution may only narrow or activate this value.
    pub(crate) capture_decision: awaken_runtime_contract::CaptureDecision,
    /// Read-only Control consent service. The default null source preserves open
    /// standalone behavior; a managed process replaces it explicitly.
    pub(crate) data_subject_consent:
        Arc<dyn awaken_data_governance_contract::DataSubjectConsentSource>,
}

impl ContentCaptureCapability {
    /// The deployment's capture ceiling, no sink, and the open null consent source.
    pub(in crate::host) fn new(deployment: &crate::deployment_config::DeploymentConfig) -> Self {
        Self {
            capture_sink: std::sync::RwLock::new(None),
            capture_decision: crate::redact::capture_decision(deployment.content_capture, false),
            data_subject_consent: Arc::new(awaken_data_governance_contract::NullResolver),
        }
    }
}

impl SharedHost {
    /// Wire the subject-tagged captured-content sink before sharing the Host.
    #[must_use]
    pub fn with_capture_sink(self, sink: Arc<dyn awaken_runtime_contract::CaptureSink>) -> Self {
        *self
            .capture
            .capture_sink
            .write()
            .expect("capture sink lock poisoned") = Some(sink);
        self
    }

    /// Install the Control-owned consent read port used once per attributed Run.
    #[must_use]
    pub fn with_data_subject_consent_source(
        mut self,
        source: Arc<dyn awaken_data_governance_contract::DataSubjectConsentSource>,
    ) -> Self {
        self.capture.data_subject_consent = source;
        self
    }

    /// Deployment-resolved capture ceiling used by protocol decision projections.
    #[must_use]
    pub fn content_capture_ceiling(&self) -> awaken_runtime_contract::ContentCapture {
        self.capture.capture_decision.level
    }
}
