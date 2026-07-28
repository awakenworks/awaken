//! Text-level PII scrubbing (ADR-0050 D6): a real [`ContentRedactor`] for the
//! managed/compliance build. Emails, US SSNs, and card-length digit runs in
//! free text are replaced **before** the text is persisted to a span attribute,
//! eval fixture, or event history. Complementary to the schema-driven
//! `redact_arguments` (which scrubs known-sensitive JSON fields): this handles
//! unknown-position PII in free text. Over-redaction is privacy-safe, so the
//! patterns are deliberately broad; a `DlpRedactor` is a future addition.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use awaken_runtime_contract::{
    CaptureDecision, ContentCapture, ContentKind, ContentRedactor, NoopRedactor,
};
use regex::Regex;

/// Project the typed deployment ceiling into the runtime's opaque decision.
pub(crate) fn capture_decision(
    settings: crate::deployment_config::ContentCaptureSettings,
    trace_file_present: bool,
) -> CaptureDecision {
    let redactor: Arc<dyn ContentRedactor> = match settings.redaction {
        crate::deployment_config::ContentRedaction::Regex => Arc::new(PiiRedactor::new()),
        crate::deployment_config::ContentRedaction::None => Arc::new(NoopRedactor),
    };
    let level = clamp_for_trace_file(settings.level, trace_file_present);
    CaptureDecision::with_redactor(level, redactor)
}

/// Clamp the level for the active sink (ADR-0050 D7): the append-only trace file
/// has no subject key and no TTL, so it cannot honor erasure — it must never hold
/// `Full` content. When it is the sink, cap at `Structured`.
fn clamp_for_trace_file(level: ContentCapture, trace_file_present: bool) -> ContentCapture {
    if trace_file_present && level == ContentCapture::Full {
        ContentCapture::Structured
    } else {
        level
    }
}

/// Regex-based PII redactor. Compiled patterns are process-shared.
#[derive(Debug, Clone, Copy, Default)]
pub struct PiiRedactor;

impl PiiRedactor {
    /// Construct the redactor (patterns compile lazily on first use).
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

struct Patterns {
    email: Regex,
    ssn: Regex,
    /// 13–19 digit runs with optional space/dash separators (payment cards).
    card: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        email: Regex::new(r"(?i)\b[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}\b").unwrap(),
        ssn: Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
        card: Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").unwrap(),
    })
}

impl ContentRedactor for PiiRedactor {
    fn redact<'a>(&self, _kind: ContentKind, text: &'a str) -> Cow<'a, str> {
        let p = patterns();
        // Fast path: borrow unchanged when nothing matches.
        if !(p.email.is_match(text) || p.ssn.is_match(text) || p.card.is_match(text)) {
            return Cow::Borrowed(text);
        }
        // SSN before card so the 3-2-4 shape isn't swallowed; email is disjoint.
        let s = p.email.replace_all(text, "[redacted-email]").into_owned();
        let s = p.ssn.replace_all(&s, "[redacted-ssn]").into_owned();
        let s = p.card.replace_all(&s, "[redacted-number]").into_owned();
        Cow::Owned(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(text: &str) -> String {
        PiiRedactor::new()
            .redact(ContentKind::OutputMessages, text)
            .into_owned()
    }

    #[test]
    fn scrubs_email() {
        assert_eq!(
            redact("reach me at Alice.B+x@example.co.uk please"),
            "reach me at [redacted-email] please"
        );
    }

    #[test]
    fn scrubs_ssn() {
        assert_eq!(redact("ssn 123-45-6789"), "ssn [redacted-ssn]");
    }

    #[test]
    fn scrubs_card_with_and_without_separators() {
        assert_eq!(
            redact("card 4111111111111111 ok"),
            "card [redacted-number] ok"
        );
        assert_eq!(
            redact("card 4111 1111 1111 1111 ok"),
            "card [redacted-number] ok"
        );
    }

    #[test]
    fn leaves_clean_text_borrowed_and_unchanged() {
        let text = "the quick brown fox has 7 legs";
        let out = PiiRedactor::new().redact(ContentKind::InputMessages, text);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, text);
    }

    #[test]
    fn short_numbers_are_not_treated_as_cards() {
        // 9-digit and shorter runs stay (SSN shape has its own rule).
        assert_eq!(redact("order 12345 shipped"), "order 12345 shipped");
    }

    #[test]
    fn scrubs_multiple_kinds_in_one_pass() {
        assert_eq!(
            redact("a@b.com and 123-45-6789"),
            "[redacted-email] and [redacted-ssn]"
        );
    }

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
