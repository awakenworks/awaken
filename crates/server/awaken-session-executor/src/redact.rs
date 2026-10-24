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
    settings: awaken_deployment_config::ContentCaptureSettings,
    trace_file_present: bool,
) -> CaptureDecision {
    let redactor: Arc<dyn ContentRedactor> = if uses_pii_redaction(settings.redaction) {
        Arc::new(PiiRedactor::new())
    } else {
        Arc::new(NoopRedactor)
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

/// The supported persistence choice used by the actual redactor constructor.
/// An exhaustive match keeps any future unsupported mode from falling back to
/// no redaction. The upper-layer DLP requirement is not an execution choice.
const fn uses_pii_redaction(configured: awaken_deployment_config::ContentRedaction) -> bool {
    match configured {
        awaken_deployment_config::ContentRedaction::None => false,
        awaken_deployment_config::ContentRedaction::Regex => true,
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{clamp_for_trace_file, uses_pii_redaction};
    use awaken_deployment_config::ContentRedaction;
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
        // Decision rows: None => false (Noop), Regex => true (Pii). These are
        // all supported inputs to the predicate used by the real constructor;
        // the bytes table below independently checks its selected effect.
        let configured = if kani::any() {
            ContentRedaction::None
        } else {
            ContentRedaction::Regex
        };
        let expected = match configured {
            ContentRedaction::None => false,
            ContentRedaction::Regex => true,
        };
        assert_eq!(uses_pii_redaction(configured), expected);
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
        use awaken_deployment_config::{ContentCaptureSettings, ContentRedaction};
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

        // Exhaustive C5 extension of the same cause/effect table: 3 configured
        // levels x 2 supported redactors x 2 sinks = 12 rows. Off/Structured
        // emit no bytes; a Full trace is Structured; only non-trace Full emits
        // bytes, unchanged for None and actually scrubbed by PiiRedactor for
        // Regex. These literal outcomes also catch a swapped constructor even
        // if the pure predicate's Kani proof still passes. DLP is not an input.
        let rows = [
            (
                ContentCapture::Off,
                ContentRedaction::None,
                false,
                ContentCapture::Off,
                None,
            ),
            (
                ContentCapture::Off,
                ContentRedaction::None,
                true,
                ContentCapture::Off,
                None,
            ),
            (
                ContentCapture::Off,
                ContentRedaction::Regex,
                false,
                ContentCapture::Off,
                None,
            ),
            (
                ContentCapture::Off,
                ContentRedaction::Regex,
                true,
                ContentCapture::Off,
                None,
            ),
            (
                ContentCapture::Structured,
                ContentRedaction::None,
                false,
                ContentCapture::Structured,
                None,
            ),
            (
                ContentCapture::Structured,
                ContentRedaction::None,
                true,
                ContentCapture::Structured,
                None,
            ),
            (
                ContentCapture::Structured,
                ContentRedaction::Regex,
                false,
                ContentCapture::Structured,
                None,
            ),
            (
                ContentCapture::Structured,
                ContentRedaction::Regex,
                true,
                ContentCapture::Structured,
                None,
            ),
            (
                ContentCapture::Full,
                ContentRedaction::None,
                false,
                ContentCapture::Full,
                Some("mail a@b.com"),
            ),
            (
                ContentCapture::Full,
                ContentRedaction::Regex,
                false,
                ContentCapture::Full,
                Some("mail [redacted-email]"),
            ),
            (
                ContentCapture::Full,
                ContentRedaction::None,
                true,
                ContentCapture::Structured,
                None,
            ),
            (
                ContentCapture::Full,
                ContentRedaction::Regex,
                true,
                ContentCapture::Structured,
                None,
            ),
        ];
        assert_eq!(rows.len(), 12, "C5 complete cause matrix");
        for (level, redaction, trace, expected_level, expected_bytes) in rows {
            let decision =
                super::capture_decision(ContentCaptureSettings { level, redaction }, trace);
            assert_eq!(
                decision.level, expected_level,
                "C5 {level:?}/{redaction:?}/{trace}"
            );
            assert_eq!(
                decision
                    .content(ContentKind::InputMessages, "mail a@b.com")
                    .as_deref(),
                expected_bytes,
                "C5 {level:?}/{redaction:?}/{trace}",
            );
        }
    }
}
