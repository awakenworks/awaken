// @formal entrypoint capture.persistence_redaction_selection_is_total_exact_and_non_widening
//! Content-capture selection (ADR-0050 D6/D7): project the typed Deployment
//! ceiling into the runtime's capture decision, choosing the redactor
//! (`awaken_observability::PiiRedactor` or none) and clamping the level for the
//! append-only trace sink.

use std::sync::Arc;

use awaken_observability::PiiRedactor;
use awaken_runtime_contract::{CaptureDecision, ContentCapture, ContentRedactor, NoopRedactor};

/// Project the typed deployment ceiling into the runtime's opaque decision.
pub(crate) fn capture_decision(
    settings: crate::deployment_config::ContentCaptureSettings,
    trace_file_present: bool,
) -> CaptureDecision {
    let redactor: Arc<dyn ContentRedactor> = match select_capture_redactor(settings.redaction) {
        CaptureRedactorSelection::Regex => Arc::new(PiiRedactor::new()),
        CaptureRedactorSelection::None => Arc::new(NoopRedactor),
    };
    let level = clamp_for_trace_file(settings.level, trace_file_present);
    CaptureDecision::with_redactor(level, redactor)
}

/// Clamp the level for the active sink (ADR-0050 D7): the append-only trace file
/// has no subject key and no TTL, so it cannot honor erasure — it must never hold
/// `Full` content. When it is the sink, cap at `Structured`.
const fn clamp_for_trace_file(level: ContentCapture, trace_file_present: bool) -> ContentCapture {
    match (trace_file_present, level) {
        (true, ContentCapture::Full) => ContentCapture::Structured,
        _ => level,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureRedactorSelection {
    None,
    Regex,
}

const fn select_capture_redactor(
    configured: crate::deployment_config::ContentRedaction,
) -> CaptureRedactorSelection {
    match configured {
        crate::deployment_config::ContentRedaction::None => CaptureRedactorSelection::None,
        crate::deployment_config::ContentRedaction::Regex => CaptureRedactorSelection::Regex,
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{CaptureRedactorSelection, clamp_for_trace_file, select_capture_redactor};
    use crate::deployment_config::ContentRedaction;
    use awaken_runtime_contract::ContentCapture;

    fn arbitrary_capture() -> ContentCapture {
        match kani::any::<u8>() % 3 {
            0 => ContentCapture::Off,
            1 => ContentCapture::Structured,
            _ => ContentCapture::Full,
        }
    }

    /// @obligation capture.persistence_redaction_selection_is_total_exact_and_non_widening
    #[kani::proof]
    fn trace_capture_clamp_is_exact_and_never_widens_persisted_content() {
        let configured = arbitrary_capture();
        let trace_file_present: bool = kani::any();
        let effective = clamp_for_trace_file(configured, trace_file_present);
        let expected = if trace_file_present && configured == ContentCapture::Full {
            ContentCapture::Structured
        } else {
            configured
        };

        assert_eq!(effective, expected);
        assert!(effective <= configured);
        if trace_file_present {
            assert_ne!(effective, ContentCapture::Full);
        }
    }

    /// @obligation capture.persistence_redaction_selection_is_total_exact_and_non_widening
    #[kani::proof]
    fn configured_capture_redactor_selection_is_total_and_exact() {
        let configured = if kani::any() {
            ContentRedaction::None
        } else {
            ContentRedaction::Regex
        };
        let expected = match configured {
            ContentRedaction::None => CaptureRedactorSelection::None,
            ContentRedaction::Regex => CaptureRedactorSelection::Regex,
        };
        assert_eq!(select_capture_redactor(configured), expected);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_capture_policy_projects_level_redaction_and_trace_clamp() {
        // Cause/effect graph:
        // configured level selects the ceiling; Regex selects PII redaction;
        // an append-only trace sink clamps Full to Structured.
        //
        // | Rule | level | redaction | trace file | Effect |
        // |---|---|---|---:|---|
        // | C1 | Structured | None | 0 | structured, no content |
        // | C2 | Full | Regex | 0 | full, scrubbed content |
        // | C3 | Off | Regex | 0 | no content |
        // | C4 | Full | either | 1 | structured |
        use crate::deployment_config::{ContentCaptureSettings, ContentRedaction};
        use awaken_runtime_contract::{ContentCapture, ContentKind};
        let structured = super::capture_decision(ContentCaptureSettings::default(), false);
        assert_eq!(structured.level, ContentCapture::Structured, "C1");
        assert!(
            structured
                .content(ContentKind::InputMessages, "a@b.com")
                .is_none(),
            "C1"
        );
        let full = super::capture_decision(
            ContentCaptureSettings {
                level: ContentCapture::Full,
                redaction: ContentRedaction::Regex,
            },
            false,
        );
        assert_eq!(full.level, ContentCapture::Full, "C2");
        assert_eq!(
            full.content(ContentKind::InputMessages, "mail a@b.com")
                .as_deref(),
            Some("mail [redacted-email]"),
            "C2"
        );
        let off = super::capture_decision(
            ContentCaptureSettings {
                level: ContentCapture::Off,
                redaction: ContentRedaction::Regex,
            },
            false,
        );
        assert!(
            off.content(ContentKind::OutputMessages, "x").is_none(),
            "C3"
        );
        assert_eq!(
            super::capture_decision(
                ContentCaptureSettings {
                    level: ContentCapture::Full,
                    redaction: ContentRedaction::None,
                },
                true,
            )
            .level,
            ContentCapture::Structured,
            "C4"
        );
    }
}
