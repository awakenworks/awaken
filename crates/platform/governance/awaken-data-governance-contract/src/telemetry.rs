// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Static owner: the tighten-only composition of the content-capture ceiling and the
// @formal reason: Run-boundary policy that applies it. Dynamic path: every lower layer enters only
// @formal reason: through `TelemetryCeiling::narrow` (a meet), and an unreadable Workspace narrowing
// @formal reason: closes the ceiling to `Off`. Evidence: analytics.observability.projection_only plus
// @formal reason: this module's exhaustive composition tables; a product-requirement classification,
// @formal reason: not a direct proof claim.
//! Telemetry ceiling and its tighten-only composition (ADR-0050 D2/D3).
//!
//! The top layer that exists holds the binding ceiling: the Org in the cloud,
//! the deployment configuration in a self-hosted deployment (the operator is
//! the controller). Workspace, Agent and Session carry only a
//! [`TelemetryNarrowing`], which can be applied solely through
//! [`TelemetryCeiling::narrow`]. Composition takes the stricter capture level,
//! the stricter redaction mode and the shorter retention bound, so each
//! operation is monotone and a lower layer can never widen the ceiling —
//! enforced by types, not validation code.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::ContentCapture;
use serde::{Deserialize, Serialize};

/// How thoroughly recorded content is scrubbed. A total order by strictness
/// (`None < Regex < Dlp`); composing two takes the **stricter** (higher).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionMode {
    /// No scrubbing (the open default).
    #[default]
    None,
    /// Regex PII scrubbing (email/ssn/card).
    Regex,
    /// External DLP backend (future).
    Dlp,
}

impl RedactionMode {
    /// The stricter (more-scrubbing) of two modes.
    #[must_use]
    pub fn stricter(self, other: Self) -> Self {
        self.max(other)
    }
}

/// A telemetry ceiling: the most a layer permits. `retention_days = None` means
/// unbounded (the least strict). Org's is the compliance baseline; lower layers
/// tighten it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TelemetryCeiling {
    /// The most content this layer permits.
    pub content_capture: ContentCapture,
    /// The least scrubbing this layer requires.
    pub redaction: RedactionMode,
    /// The longest retention this layer permits (`None` = unbounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

impl TelemetryCeiling {
    /// Compose this (upper) ceiling with a `lower` layer's narrowing: capture
    /// meets (stricter), redaction takes the stricter mode, retention takes the
    /// shorter bound. Monotone — the result is never looser than either input.
    #[must_use]
    pub fn tighten(self, lower: Self) -> Self {
        Self {
            content_capture: self.content_capture.meet(lower.content_capture),
            redaction: self.redaction.stricter(lower.redaction),
            retention_days: shorter(self.retention_days, lower.retention_days),
        }
    }

    /// The binding ceiling held by the top layer: its capture level, no extra
    /// scrubbing requirement and unbounded retention, so only the axes the top
    /// layer actually configures constrain the result.
    #[must_use]
    pub fn top(content_capture: ContentCapture) -> Self {
        Self {
            content_capture,
            redaction: RedactionMode::None,
            retention_days: None,
        }
    }

    /// Apply one lower layer's narrowing. An axis the layer leaves unset is
    /// the identity of its composition, so it never changes the result.
    #[must_use]
    pub fn narrow(self, narrowing: &TelemetryNarrowing) -> Self {
        self.tighten(Self {
            content_capture: narrowing.content_capture.unwrap_or(ContentCapture::Full),
            redaction: RedactionMode::None,
            retention_days: None,
        })
    }
}

/// What one lower layer (Workspace, Agent or Session) may author: a capture
/// level no wider than the ceiling above it. `None` leaves the ceiling as is.
///
/// The only way to apply it is [`TelemetryCeiling::narrow`], which meets, so
/// a value above the ceiling has no effect; authoring surfaces additionally
/// reject it with [`TelemetryNarrowing::admits_under`] so the stored intent
/// never claims more than the deployment permits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryNarrowing {
    /// The most content this layer permits, or `None` to inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_capture: Option<ContentCapture>,
}

impl TelemetryNarrowing {
    /// The fail-closed narrowing used when a layer's configuration cannot be
    /// read: no content at all, rather than silently inheriting the ceiling.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            content_capture: Some(ContentCapture::Off),
        }
    }

    /// Whether this narrowing stays within `ceiling`. A wider value is
    /// rejected at authoring time; composition clamps it regardless.
    #[must_use]
    pub fn admits_under(&self, ceiling: ContentCapture) -> bool {
        self.content_capture.is_none_or(|level| level <= ceiling)
    }
}

/// A Workspace's telemetry configuration could not be read. Either way the
/// Run boundary closes the ceiling rather than inheriting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TelemetryPolicyError {
    /// The authored configuration exists but its store could not serve it.
    #[error("the Workspace telemetry configuration store is unavailable")]
    StoreUnavailable,
    /// The Control authority could not be reached or refused the caller.
    #[error("the Control telemetry authority is unreachable")]
    AuthorityUnreachable,
}

/// Read port for the Workspace layer of the capture ceiling, consulted at each
/// Run boundary beside the consent port.
///
/// Control owns the authored Workspace configuration. AllInOne injects its
/// repository-backed source; a split Coordinator reaches the same source
/// through the authenticated private Control boundary.
#[async_trait]
pub trait TelemetryNarrowingSource: Send + Sync {
    /// The narrowing `workspace` carries; the default value when it has none.
    /// An error means the configuration is unreadable; callers fail closed.
    async fn workspace_narrowing(
        &self,
        workspace: &str,
    ) -> Result<TelemetryNarrowing, TelemetryPolicyError>;
}

/// The Control-owned read ports the Run boundary consults (ADR-0050 D2):
/// subject consent and the Workspace narrowing of the capture ceiling. One
/// value, so every topology injects both from the same authority. The default
/// is the open standalone pair: no consent clamp and no narrowing.
#[derive(Clone)]
pub struct CapturePolicySources {
    pub consent: Arc<dyn crate::DataSubjectConsentSource>,
    pub narrowing: Arc<dyn TelemetryNarrowingSource>,
}

impl Default for CapturePolicySources {
    fn default() -> Self {
        Self {
            consent: Arc::new(crate::NullResolver),
            narrowing: Arc::new(crate::NullResolver),
        }
    }
}

impl CapturePolicySources {
    /// Bind these ports to the Workspace one Thread's Runs execute in.
    #[must_use]
    pub fn for_workspace(&self, workspace: String) -> WorkspaceCapturePolicy {
        WorkspaceCapturePolicy {
            sources: self.clone(),
            workspace,
        }
    }
}

/// The binding top-layer ceiling with the sources that narrow it: everything a
/// capture decision reads besides the request. The Host holds one; the
/// transparency reads receive the same value, so they decide as its Runs do.
/// The default is the open standalone Host's: the default `Structured`
/// ceiling and the null sources.
#[derive(Clone, Default)]
pub struct CapturePolicy {
    /// The top layer's binding ceiling (the deployment configuration in a
    /// self-hosted deployment).
    pub top: ContentCapture,
    pub sources: CapturePolicySources,
}

impl CapturePolicy {
    #[must_use]
    pub const fn new(top: ContentCapture, sources: CapturePolicySources) -> Self {
        Self { top, sources }
    }

    /// The decision for one request in `workspace`; see [`WorkspaceCapturePolicy::decide`].
    pub async fn decide(
        &self,
        workspace: &str,
        requested: ContentCapture,
        subject: Option<&awaken_runtime_contract::DataSubjectId>,
    ) -> crate::CapturePolicyDecision {
        self.sources
            .for_workspace(workspace.to_owned())
            .decide(self.top, requested, subject)
            .await
    }
}

/// [`CapturePolicySources`] bound to one Workspace: what a Run boundary asks.
#[derive(Clone)]
pub struct WorkspaceCapturePolicy {
    sources: CapturePolicySources,
    workspace: String,
}

impl WorkspaceCapturePolicy {
    /// The Workspace narrowing as its source answers, error included; only
    /// [`crate::decide_capture`] turns an error into a closed ceiling.
    pub async fn narrowing(&self) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
        self.sources
            .narrowing
            .workspace_narrowing(&self.workspace)
            .await
    }

    /// The capture decision for one request in this Workspace: reads the
    /// Workspace narrowing and, when `subject` is attributed, its telemetry
    /// consent from these sources, then applies [`crate::decide_capture`].
    pub async fn decide(
        &self,
        top: ContentCapture,
        requested: ContentCapture,
        subject: Option<&awaken_runtime_contract::DataSubjectId>,
    ) -> crate::CapturePolicyDecision {
        let workspace = self.narrowing().await;
        let consent = match subject {
            Some(subject) => Some(
                self.sources
                    .consent
                    .consent_ceiling(subject, awaken_runtime_contract::Purpose::TelemetryContent)
                    .await,
            ),
            None => None,
        };
        crate::decide_capture(crate::CapturePolicyInputs {
            requested,
            top,
            workspace,
            consent,
        })
    }
}

/// The shorter of two optional day-bounds, treating `None` as unbounded.
fn shorter(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tighten_takes_stricter_capture_and_redaction_and_shorter_retention() {
        let org = TelemetryCeiling {
            content_capture: ContentCapture::Full,
            redaction: RedactionMode::Regex,
            retention_days: Some(90),
        };
        let workspace = TelemetryCeiling {
            content_capture: ContentCapture::Structured,
            redaction: RedactionMode::None,
            retention_days: Some(30),
        };
        let got = org.tighten(workspace);
        // capture: stricter = Structured; redaction: stricter = Regex; retention: shorter = 30.
        assert_eq!(got.content_capture, ContentCapture::Structured);
        assert_eq!(got.redaction, RedactionMode::Regex);
        assert_eq!(got.retention_days, Some(30));
    }

    #[test]
    fn a_lower_layer_cannot_widen() {
        let org = TelemetryCeiling {
            content_capture: ContentCapture::Off,
            redaction: RedactionMode::Dlp,
            retention_days: Some(7),
        };
        // A lower layer asking for MORE than the Org baseline is clamped down.
        let greedy = TelemetryCeiling {
            content_capture: ContentCapture::Full,
            redaction: RedactionMode::None,
            retention_days: Some(365),
        };
        let got = org.tighten(greedy);
        assert_eq!(got.content_capture, ContentCapture::Off);
        assert_eq!(got.redaction, RedactionMode::Dlp);
        assert_eq!(got.retention_days, Some(7));
    }

    #[test]
    fn none_retention_is_unbounded() {
        assert_eq!(shorter(None, Some(30)), Some(30));
        assert_eq!(shorter(Some(30), None), Some(30));
        assert_eq!(shorter(None, None), None);
        assert_eq!(shorter(Some(30), Some(7)), Some(7));
    }

    #[test]
    fn redaction_stricter_is_a_total_order_none_lt_regex_lt_dlp() {
        // A10(a): `stricter` is max over the total order None < Regex < Dlp.
        assert!(RedactionMode::None < RedactionMode::Regex);
        assert!(RedactionMode::Regex < RedactionMode::Dlp);
        assert_eq!(
            RedactionMode::None.stricter(RedactionMode::Regex),
            RedactionMode::Regex
        );
        assert_eq!(
            RedactionMode::Regex.stricter(RedactionMode::Dlp),
            RedactionMode::Dlp
        );
        // Commutative, and the open default never wins over a stricter mode.
        assert_eq!(
            RedactionMode::Dlp.stricter(RedactionMode::None),
            RedactionMode::Dlp
        );
        assert_eq!(
            RedactionMode::None.stricter(RedactionMode::None),
            RedactionMode::None
        );
    }

    #[test]
    fn tighten_composes_the_three_axes_independently() {
        // A10(e): capture/redaction/retention each tighten on their own axis — a layer
        // that only tightens one leaves the others at the upper ceiling.
        let org = TelemetryCeiling {
            content_capture: ContentCapture::Full,
            redaction: RedactionMode::None,
            retention_days: Some(90),
        };
        // Lower layer tightens ONLY redaction.
        let only_redaction = TelemetryCeiling {
            content_capture: ContentCapture::Full,
            redaction: RedactionMode::Dlp,
            retention_days: None,
        };
        let got = org.tighten(only_redaction);
        assert_eq!(got.content_capture, ContentCapture::Full);
        assert_eq!(got.redaction, RedactionMode::Dlp);
        assert_eq!(got.retention_days, Some(90));
    }

    #[test]
    fn tighten_is_associative_over_a_chain() {
        let org = TelemetryCeiling {
            content_capture: ContentCapture::Full,
            redaction: RedactionMode::None,
            retention_days: Some(90),
        };
        let ws = TelemetryCeiling {
            content_capture: ContentCapture::Full,
            redaction: RedactionMode::Regex,
            retention_days: None,
        };
        let agent = TelemetryCeiling {
            content_capture: ContentCapture::Structured,
            redaction: RedactionMode::None,
            retention_days: Some(30),
        };
        let chained = org.tighten(ws).tighten(agent);
        assert_eq!(chained.content_capture, ContentCapture::Structured);
        assert_eq!(chained.redaction, RedactionMode::Regex);
        assert_eq!(chained.retention_days, Some(30));
    }

    const LEVELS: [ContentCapture; 3] = [
        ContentCapture::Off,
        ContentCapture::Structured,
        ContentCapture::Full,
    ];

    fn narrowings() -> [TelemetryNarrowing; 4] {
        [
            TelemetryNarrowing::default(),
            TelemetryNarrowing {
                content_capture: Some(ContentCapture::Off),
            },
            TelemetryNarrowing {
                content_capture: Some(ContentCapture::Structured),
            },
            TelemetryNarrowing {
                content_capture: Some(ContentCapture::Full),
            },
        ]
    }

    #[test]
    fn top_then_workspace_agent_session_narrowing_decision_table() {
        // Causes: C1 top ceiling (3 levels); C2..C4 Workspace/Agent/Session
        // narrowing, each unset or one of 3 levels. Effect: the effective level
        // is the minimum of the top level and every set narrowing. Rules:
        // R1 an unset layer is the identity; R2 no layer exceeds the top
        // (a lower layer cannot widen); R3 the result is order-independent
        // across the lower layers. Exhaustive: 3 x 4^3 = 192 rows.
        for top in LEVELS {
            for workspace in narrowings() {
                for agent in narrowings() {
                    for session in narrowings() {
                        let effective = TelemetryCeiling::top(top)
                            .narrow(&workspace)
                            .narrow(&agent)
                            .narrow(&session);
                        let expected = [workspace, agent, session]
                            .iter()
                            .filter_map(|layer| layer.content_capture)
                            .fold(top, ContentCapture::meet);
                        assert_eq!(effective.content_capture, expected);
                        assert!(effective.content_capture <= top, "R2");
                        let reordered = TelemetryCeiling::top(top)
                            .narrow(&session)
                            .narrow(&workspace)
                            .narrow(&agent);
                        assert_eq!(reordered, effective, "R3");
                        assert_eq!(effective.redaction, RedactionMode::None);
                        assert_eq!(effective.retention_days, None);
                    }
                }
            }
        }
    }

    #[test]
    fn unset_narrowing_is_the_identity_and_unavailable_is_off() {
        for top in LEVELS {
            let ceiling = TelemetryCeiling::top(top);
            assert_eq!(ceiling.narrow(&TelemetryNarrowing::default()), ceiling);
            assert_eq!(
                ceiling
                    .narrow(&TelemetryNarrowing::unavailable())
                    .content_capture,
                ContentCapture::Off
            );
        }
    }

    #[test]
    fn admits_under_rejects_exactly_the_wider_narrowings() {
        // A narrowing is admitted iff it is unset or not above the ceiling.
        for ceiling in LEVELS {
            for narrowing in narrowings() {
                let admitted = narrowing.admits_under(ceiling);
                let expected = narrowing
                    .content_capture
                    .is_none_or(|level| level <= ceiling);
                assert_eq!(admitted, expected);
            }
        }
        let wider = TelemetryNarrowing {
            content_capture: Some(ContentCapture::Full),
        };
        assert!(!wider.admits_under(ContentCapture::Structured));
        assert!(TelemetryNarrowing::default().admits_under(ContentCapture::Off));
    }

    #[test]
    fn narrowing_wire_shape_is_closed() {
        assert_eq!(
            serde_json::to_value(TelemetryNarrowing::default()).unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            serde_json::from_value::<TelemetryNarrowing>(
                serde_json::json!({"content_capture": "structured"})
            )
            .unwrap()
            .content_capture,
            Some(ContentCapture::Structured)
        );
        assert!(
            serde_json::from_value::<TelemetryNarrowing>(serde_json::json!({"retention_days": 1}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn workspace_policy_decision_equals_the_kernel_for_every_cause() {
        // Equivalence row set: every (requested, top, Workspace answer,
        // consent) combination of the kernel's decision table. The policy only
        // reads the two ports; whatever they answer, its decision is exactly
        // `decide_capture` over those answers, and the consent port is asked
        // only for an attributed request.
        struct Fixed(Result<TelemetryNarrowing, TelemetryPolicyError>);
        #[async_trait]
        impl TelemetryNarrowingSource for Fixed {
            async fn workspace_narrowing(
                &self,
                workspace: &str,
            ) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
                assert_eq!(workspace, "ws");
                self.0
            }
        }
        struct Consent(Option<ContentCapture>);
        #[async_trait]
        impl crate::DataSubjectConsentSource for Consent {
            async fn consent_ceiling(
                &self,
                subject: &awaken_runtime_contract::DataSubjectId,
                purpose: awaken_runtime_contract::Purpose,
            ) -> ContentCapture {
                assert_eq!(subject.as_str(), "dsub");
                assert_eq!(purpose, awaken_runtime_contract::Purpose::TelemetryContent);
                self.0.expect("an unattributed request never reads consent")
            }
        }
        let subject = awaken_runtime_contract::DataSubjectId("dsub".into());
        for inputs in crate::decision::tests::every_input() {
            let sources = CapturePolicySources {
                consent: Arc::new(Consent(inputs.consent)),
                narrowing: Arc::new(Fixed(inputs.workspace)),
            };
            let subject = inputs.consent.map(|_| &subject);
            let decided = sources
                .for_workspace("ws".into())
                .decide(inputs.top, inputs.requested, subject)
                .await;
            assert_eq!(decided, crate::decide_capture(inputs), "{inputs:?}");
            let held = CapturePolicy::new(inputs.top, sources)
                .decide("ws", inputs.requested, subject)
                .await;
            assert_eq!(held, decided, "the held policy decides identically");
        }
    }

    #[tokio::test]
    async fn workspace_policy_composes_the_narrowing_and_fails_closed() {
        // Causes: C1 top level; C2 the Workspace narrowing read (each value, or
        // unreadable). Effects: E1 min(top, narrowing); E2 unreadable -> Off,
        // never the inherited top; E3 the open default leaves top unchanged.
        struct Fixed(Result<TelemetryNarrowing, TelemetryPolicyError>);
        #[async_trait]
        impl TelemetryNarrowingSource for Fixed {
            async fn workspace_narrowing(
                &self,
                _workspace: &str,
            ) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
                self.0
            }
        }
        let with = |result| {
            CapturePolicySources {
                narrowing: Arc::new(Fixed(result)),
                ..CapturePolicySources::default()
            }
            .for_workspace("ws".into())
        };
        // The ceiling a Run decides under: unattributed and requesting
        // `Full`, so `effective` is the composed ceiling itself.
        let ceiling = |policy: WorkspaceCapturePolicy, top| async move {
            let decision = policy.decide(top, ContentCapture::Full, None).await;
            assert_eq!(decision.effective, decision.ceiling);
            decision.ceiling
        };
        for top in LEVELS {
            let open = CapturePolicySources::default().for_workspace("ws".into());
            assert_eq!(ceiling(open, top).await, top, "E3");
            for narrowing in narrowings() {
                let expected = TelemetryCeiling::top(top)
                    .narrow(&narrowing)
                    .content_capture;
                assert_eq!(ceiling(with(Ok(narrowing)), top).await, expected, "E1");
            }
            for error in [
                TelemetryPolicyError::StoreUnavailable,
                TelemetryPolicyError::AuthorityUnreachable,
            ] {
                assert_eq!(
                    ceiling(with(Err(error)), top).await,
                    ContentCapture::Off,
                    "E2"
                );
            }
        }
        let subject = awaken_runtime_contract::DataSubjectId("dsub".into());
        assert_eq!(
            with(Ok(TelemetryNarrowing::default()))
                .decide(ContentCapture::Full, ContentCapture::Full, Some(&subject))
                .await
                .consent,
            Some(ContentCapture::Full),
            "the open consent default never clamps"
        );
    }
}
