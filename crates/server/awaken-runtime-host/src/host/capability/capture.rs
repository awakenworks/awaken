// @formal surface obligation=capture.persistence_redaction_selection_is_total_exact_and_non_widening
// @formal reason: Holds the deployment capture ceiling derived by the redaction selector and the consent port;
// @formal reason: per-request consent may only narrow it.
//! CAPTURE capability of [`SharedHost`] (ADR-0050): the Host-owned
//! subject-tagged content sink, the deployment-resolved capture ceiling, and
//! the Control consent and Workspace-narrowing read ports.

use crate::host::*;

/// Content-capture wiring. Consumer: Session context assembly (per-Run
/// capture decision) and the protocol adapters' ceiling projection.
pub(crate) struct ContentCaptureCapability {
    /// The one Host-owned subject-tagged captured-content sink (ADR-0050),
    /// selected with the Host's dependencies. `None` = spans only.
    pub(crate) capture_sink: Option<Arc<dyn awaken_runtime_contract::CaptureSink>>,
    /// Deployment-resolved capture ceiling/redactor. Per-request consent and
    /// subject attribution may only narrow or activate this value.
    pub(crate) capture_decision: awaken_runtime_contract::CaptureDecision,
    /// Read-only Control consent and Workspace-narrowing ports. The default null
    /// sources preserve open standalone behavior; a managed process replaces them.
    pub(crate) policy_sources: awaken_data_governance_contract::CapturePolicySources,
}

impl ContentCaptureCapability {
    /// The deployment's capture ceiling, no sink, and the open null consent source.
    pub(in crate::host) fn new(deployment: &crate::deployment_config::DeploymentConfig) -> Self {
        Self {
            capture_sink: None,
            capture_decision: crate::redact::capture_decision(deployment.content_capture, false),
            policy_sources: awaken_data_governance_contract::CapturePolicySources::default(),
        }
    }
}

impl SharedHost {
    /// Deployment-resolved capture ceiling used by protocol decision projections.
    #[must_use]
    pub fn content_capture_ceiling(&self) -> awaken_runtime_contract::ContentCapture {
        self.capture.capture_decision.level
    }
}
