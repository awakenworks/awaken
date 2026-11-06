// @formal entrypoint capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed
// @formal entrypoint capture.persistence_redaction_selection_is_total_exact_and_non_widening
//! The one content-capture decision (ADR-0050 D2/D3/D8).
//!
//! Every place that answers "how much content does this Run record?" calls
//! [`decide_capture`]: the Run boundary before an attempt executes, the
//! `capture-decision` read on a User Profile and the `content_capture`
//! projection on a Session. Each caller only reads the causes from the same
//! [`crate::CapturePolicySources`]; none of them composes levels itself, so the
//! transparency reads cannot report a wider level than a Run actually uses.

use awaken_runtime_contract::ContentCapture;
use serde::Serialize;

use crate::{TelemetryCeiling, TelemetryNarrowing, TelemetryPolicyError};

/// Bound a deployment ceiling by the sink selected by this process (ADR-0050
/// D7). The append-only file has neither subject erasure nor a TTL, so it may
/// record Structured spans but never Full content. Control and execution use
/// this same projection before applying Workspace policy and subject consent.
#[must_use]
pub const fn clamp_for_trace_file(
    level: ContentCapture,
    trace_file_present: bool,
) -> ContentCapture {
    match (trace_file_present, level) {
        (true, ContentCapture::Full) => ContentCapture::Structured,
        _ => level,
    }
}

/// Why the effective level is what it is: the `reason` of ADR-0050 D8.
///
/// Wire values are `ok`, `no_consent` and `clamped_by_ceiling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureReason {
    /// The requested level is granted unchanged.
    Ok,
    /// The subject's consent is the binding limit, strictly below the ceiling.
    NoConsent,
    /// The ceiling (the top layer tightened by the Workspace, or closed
    /// because the Workspace narrowing could not be read) is the binding limit.
    ClampedByCeiling,
}

/// The causes of one capture decision, each already read from its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturePolicyInputs {
    /// The level the caller asks for; `Full` when nothing narrower is asked.
    pub requested: ContentCapture,
    /// The binding ceiling of the top layer (the deployment configuration in a
    /// self-hosted deployment).
    pub top: ContentCapture,
    /// The Workspace narrowing as its source answered. An error closes the
    /// ceiling to `Off` rather than inheriting `top`.
    pub workspace: Result<TelemetryNarrowing, TelemetryPolicyError>,
    /// The attributed subject's consent ceiling; `None` when the request is
    /// not attributed to a subject, so consent does not apply.
    pub consent: Option<ContentCapture>,
}

/// One capture decision with its causes: the read-only projection
/// `{ requested, ceiling, consent, effective, reason }` of ADR-0050 D8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CapturePolicyDecision {
    /// The level the caller asked for.
    pub requested: ContentCapture,
    /// The top-layer ceiling tightened by the Workspace narrowing.
    pub ceiling: ContentCapture,
    /// The subject's consent ceiling; `null` when no subject is attributed.
    pub consent: Option<ContentCapture>,
    /// The level a Run records: the meet of every cause.
    pub effective: ContentCapture,
    /// Which cause bound `effective`.
    pub reason: CaptureReason,
}

/// Decide the effective capture level from its causes.
///
/// `effective = requested ⊓ ceiling ⊓ consent`, where
/// `ceiling = top ⊓ workspace` and an unreadable Workspace narrowing makes the
/// ceiling `Off`. The reason is `ok` when nothing narrowed the request,
/// `no_consent` when consent alone is the binding limit below the ceiling, and
/// `clamped_by_ceiling` otherwise.
#[must_use]
pub fn decide_capture(inputs: CapturePolicyInputs) -> CapturePolicyDecision {
    let narrowing = inputs
        .workspace
        .unwrap_or(TelemetryNarrowing::unavailable());
    let ceiling = TelemetryCeiling::top(inputs.top)
        .narrow(&narrowing)
        .content_capture;
    let bounded = inputs.requested.meet(ceiling);
    let effective = match inputs.consent {
        Some(consent) => bounded.meet(consent),
        None => bounded,
    };
    let reason = if effective == inputs.requested {
        CaptureReason::Ok
    } else if inputs.consent == Some(effective) && effective < ceiling {
        CaptureReason::NoConsent
    } else {
        CaptureReason::ClampedByCeiling
    };
    CapturePolicyDecision {
        requested: inputs.requested,
        ceiling,
        consent: inputs.consent,
        effective,
        reason,
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const fn listed_capture(level: ContentCapture) -> usize {
        match level {
            ContentCapture::Off => 0,
            ContentCapture::Structured => 1,
            ContentCapture::Full => 2,
        }
    }
    const EVERY_CAPTURE: [ContentCapture; 3] = [
        ContentCapture::Off,
        ContentCapture::Structured,
        ContentCapture::Full,
    ];

    const fn listed_policy_error(error: TelemetryPolicyError) -> usize {
        match error {
            TelemetryPolicyError::StoreUnavailable => 0,
            TelemetryPolicyError::AuthorityUnreachable => 1,
        }
    }
    const EVERY_POLICY_ERROR: [TelemetryPolicyError; 2] = [
        TelemetryPolicyError::StoreUnavailable,
        TelemetryPolicyError::AuthorityUnreachable,
    ];

    /// Every `CapturePolicyInputs`: requested(3) x top(3) x Workspace answer
    /// (unset, each level, both unreadable kinds = 6) x consent (absent or a
    /// level = 4) = 216.
    fn every_input() -> Vec<CapturePolicyInputs> {
        let workspaces = [
            Ok(TelemetryNarrowing {
                content_capture: None,
            }),
            Ok(TelemetryNarrowing {
                content_capture: Some(EVERY_CAPTURE[0]),
            }),
            Ok(TelemetryNarrowing {
                content_capture: Some(EVERY_CAPTURE[1]),
            }),
            Ok(TelemetryNarrowing {
                content_capture: Some(EVERY_CAPTURE[2]),
            }),
            Err(EVERY_POLICY_ERROR[0]),
            Err(EVERY_POLICY_ERROR[1]),
        ];
        let consents = [
            None,
            Some(EVERY_CAPTURE[0]),
            Some(EVERY_CAPTURE[1]),
            Some(EVERY_CAPTURE[2]),
        ];
        let mut inputs = Vec::new();
        for requested in EVERY_CAPTURE {
            for top in EVERY_CAPTURE {
                for workspace in workspaces {
                    for consent in consents {
                        inputs.push(CapturePolicyInputs {
                            requested,
                            top,
                            workspace,
                            consent,
                        });
                    }
                }
            }
        }
        assert_eq!(inputs.len(), 216);
        inputs
    }

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_CAPTURE.into_iter().enumerate() {
            assert_eq!(listed_capture(value), index);
        }
        for (index, value) in EVERY_POLICY_ERROR.into_iter().enumerate() {
            assert_eq!(listed_policy_error(value), index);
        }
    }

    /// @exhaustive CapturePolicyInputs(216) = 216
    /// @obligation capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed
    #[test]
    fn capture_decision_is_the_exact_meet_and_never_exceeds_any_layer() {
        for inputs in every_input() {
            let decision = decide_capture(inputs);

            assert_eq!(decision.requested, inputs.requested);
            assert_eq!(decision.consent, inputs.consent);
            assert!(decision.ceiling <= inputs.top);
            assert!(decision.effective <= inputs.requested);
            assert!(decision.effective <= decision.ceiling);
            if let Ok(TelemetryNarrowing {
                content_capture: Some(level),
            }) = inputs.workspace
            {
                assert!(decision.ceiling <= level);
            }
            if let Some(consent) = inputs.consent {
                assert!(decision.effective <= consent);
            }
            let exact = inputs.requested.min(decision.ceiling);
            let exact = match inputs.consent {
                Some(consent) => exact.min(consent),
                None => exact,
            };
            assert_eq!(decision.effective, exact);
        }
    }

    /// @exhaustive CapturePolicyInputs(216) x TelemetryPolicyError(2) = 432
    /// @obligation capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed
    #[test]
    fn unreadable_workspace_narrowing_closes_capture() {
        for mut inputs in every_input() {
            for error in EVERY_POLICY_ERROR {
                inputs.workspace = Err(error);
                let decision = decide_capture(inputs);
                assert_eq!(decision.ceiling, ContentCapture::Off);
                assert_eq!(decision.effective, ContentCapture::Off);
                assert!(!decision.effective.allows_content());
            }
        }
    }

    /// @exhaustive CapturePolicyInputs(216) = 216
    /// @obligation capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed
    #[test]
    fn capture_reason_names_the_exact_binding_cause() {
        for inputs in every_input() {
            let decision = decide_capture(inputs);
            let expected = if decision.effective == inputs.requested {
                CaptureReason::Ok
            } else if inputs.consent == Some(decision.effective)
                && decision.effective < decision.ceiling
            {
                CaptureReason::NoConsent
            } else {
                CaptureReason::ClampedByCeiling
            };
            assert_eq!(decision.reason, expected);
            // Content is admitted only when every cause admits it.
            if decision.effective.allows_content() {
                assert_eq!(inputs.requested, ContentCapture::Full);
                assert_eq!(decision.ceiling, ContentCapture::Full);
                assert!(
                    inputs
                        .consent
                        .is_none_or(|consent| consent == ContentCapture::Full)
                );
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const LEVELS: [ContentCapture; 3] = [
        ContentCapture::Off,
        ContentCapture::Structured,
        ContentCapture::Full,
    ];

    /// Every Workspace answer: unset, each level, and both unreadable kinds.
    fn workspace_answers() -> [Result<TelemetryNarrowing, TelemetryPolicyError>; 6] {
        let set = |level| {
            Ok(TelemetryNarrowing {
                content_capture: Some(level),
            })
        };
        [
            Ok(TelemetryNarrowing::default()),
            set(ContentCapture::Off),
            set(ContentCapture::Structured),
            set(ContentCapture::Full),
            Err(TelemetryPolicyError::StoreUnavailable),
            Err(TelemetryPolicyError::AuthorityUnreachable),
        ]
    }

    fn consents() -> [Option<ContentCapture>; 4] {
        [
            None,
            Some(ContentCapture::Off),
            Some(ContentCapture::Structured),
            Some(ContentCapture::Full),
        ]
    }

    /// Every combination of causes, in a fixed order, for callers' equivalence
    /// tables: 3 requested x 3 top x 6 Workspace answers x 4 consents = 216.
    pub(crate) fn every_input() -> Vec<CapturePolicyInputs> {
        let mut inputs = Vec::new();
        for requested in LEVELS {
            for top in LEVELS {
                for workspace in workspace_answers() {
                    for consent in consents() {
                        inputs.push(CapturePolicyInputs {
                            requested,
                            top,
                            workspace,
                            consent,
                        });
                    }
                }
            }
        }
        inputs
    }

    #[test]
    fn capture_decision_table_over_every_cause() {
        // Causes: C1 requested level; C2 top-layer ceiling; C3 the Workspace
        // narrowing read (unset, a level, or unreadable); C4 the subject's
        // consent (absent when unattributed, else a level).
        // Effects: E1 ceiling = C2 meet C3, or Off when C3 is unreadable;
        // E2 effective = C1 meet E1 meet C4 (C4 absent is the identity);
        // E3 reason ok iff E2 = C1; no_consent iff C4 = E2 < E1; otherwise
        // clamped_by_ceiling. Constraints: E2 never exceeds any present cause
        // (non-widening) and an unreadable C3 never yields content (fail closed).
        // Exhaustive: 216 rows.
        let rows = every_input();
        assert_eq!(rows.len(), 216);
        for inputs in rows {
            let decision = decide_capture(inputs);
            let ceiling = match inputs.workspace {
                Ok(narrowing) => narrowing
                    .content_capture
                    .map_or(inputs.top, |level| inputs.top.min(level)),
                Err(_) => ContentCapture::Off,
            };
            assert_eq!(decision.ceiling, ceiling, "E1 {inputs:?}");
            let effective = inputs
                .consent
                .map_or(inputs.requested.min(ceiling), |consent| {
                    inputs.requested.min(ceiling).min(consent)
                });
            assert_eq!(decision.effective, effective, "E2 {inputs:?}");
            let reason = if effective == inputs.requested {
                CaptureReason::Ok
            } else if inputs.consent == Some(effective) && effective < ceiling {
                CaptureReason::NoConsent
            } else {
                CaptureReason::ClampedByCeiling
            };
            assert_eq!(decision.reason, reason, "E3 {inputs:?}");
            assert_eq!(decision.requested, inputs.requested);
            assert_eq!(decision.consent, inputs.consent);
            assert!(decision.effective <= inputs.top, "non-widening {inputs:?}");
            if inputs.workspace.is_err() {
                assert_eq!(decision.effective, ContentCapture::Off, "fail closed");
            }
        }
    }

    #[test]
    fn reasons_for_the_named_rows() {
        let decide = |requested, top, workspace: Option<ContentCapture>, consent| {
            decide_capture(CapturePolicyInputs {
                requested,
                top,
                workspace: Ok(TelemetryNarrowing {
                    content_capture: workspace,
                }),
                consent,
            })
        };
        use ContentCapture::{Full, Off, Structured};
        // The capture-decision e2e rows D1-D3, unchanged by the Workspace layer.
        assert_eq!(
            decide(Full, Full, None, Some(Structured)).reason,
            CaptureReason::NoConsent
        );
        assert_eq!(
            decide(Full, Full, None, Some(Full)).reason,
            CaptureReason::Ok
        );
        assert_eq!(
            decide(Structured, Full, None, Some(Full)).reason,
            CaptureReason::Ok
        );
        // A Workspace narrowing is a ceiling, never a consent limit.
        let narrowed = decide(Full, Full, Some(Structured), Some(Full));
        assert_eq!(narrowed.ceiling, Structured);
        assert_eq!(narrowed.effective, Structured);
        assert_eq!(narrowed.reason, CaptureReason::ClampedByCeiling);
        // Consent equal to the ceiling is attributed to the ceiling.
        assert_eq!(
            decide(Full, Structured, None, Some(Structured)).reason,
            CaptureReason::ClampedByCeiling
        );
        // An unattributed request is bounded by the ceiling alone.
        let open = decide(Full, Full, Some(Off), None);
        assert_eq!(open.effective, Off);
        assert_eq!(open.reason, CaptureReason::ClampedByCeiling);
    }

    #[test]
    fn decision_wire_shape() {
        let decision = decide_capture(CapturePolicyInputs {
            requested: ContentCapture::Full,
            top: ContentCapture::Full,
            workspace: Ok(TelemetryNarrowing::default()),
            consent: Some(ContentCapture::Structured),
        });
        assert_eq!(
            serde_json::to_value(decision).unwrap(),
            serde_json::json!({
                "requested": "full",
                "ceiling": "full",
                "consent": "structured",
                "effective": "structured",
                "reason": "no_consent",
            })
        );
        let unattributed = CapturePolicyDecision {
            consent: None,
            ..decision
        };
        assert_eq!(
            serde_json::to_value(unattributed).unwrap()["consent"],
            serde_json::Value::Null
        );
        assert_eq!(
            serde_json::to_value(CaptureReason::ClampedByCeiling).unwrap(),
            serde_json::json!("clamped_by_ceiling")
        );
    }
}
