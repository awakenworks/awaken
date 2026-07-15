//! Telemetry ceiling and its composition (ADR-0050 D3).
//!
//! The Org sets the compliance baseline (`content_capture` cap, `redaction`
//! floor, `retention` max); Workspace/Agent/Session may only **tighten** it.
//! [`TelemetryCeiling::tighten`] composes an upper ceiling with a lower layer's
//! narrowing: capture takes the stricter (lower) level, redaction the stricter
//! (higher) mode, retention the shorter bound. Because each operation is
//! monotone, a lower layer can never widen the ceiling — enforced by types, not
//! validation code.

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
}
