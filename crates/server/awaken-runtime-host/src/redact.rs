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

/// Parse a content-capture decision from `level`/`redaction` strings (the
/// single-machine env default, ADR-0050 D5/D9). `level` = off|structured|full
/// (default `structured`); `redaction` = none|regex (default none = `Noop`).
fn parse_capture(level: Option<&str>, redaction: Option<&str>) -> CaptureDecision {
    let level = match level {
        Some("off") => ContentCapture::Off,
        Some("full") => ContentCapture::Full,
        _ => ContentCapture::Structured,
    };
    let redactor: Arc<dyn ContentRedactor> = match redaction {
        Some("regex") => Arc::new(PiiRedactor::new()),
        _ => Arc::new(NoopRedactor),
    };
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

/// Resolve the content-capture decision from the environment: the open/
/// single-machine default. Managed builds override this with the resolved
/// ceiling × request × consent `meet`.
pub(crate) fn env_capture_decision() -> CaptureDecision {
    let d = parse_capture(
        std::env::var("AWAKEN_CONTENT_CAPTURE").ok().as_deref(),
        std::env::var("AWAKEN_CONTENT_REDACTION").ok().as_deref(),
    );
    let level = clamp_for_trace_file(d.level, std::env::var("AWAKEN_TRACE_FILE").is_ok());
    CaptureDecision::with_redactor(level, d.redactor)
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
    fn parse_capture_defaults_to_structured_noop() {
        use awaken_runtime_contract::{ContentCapture, ContentKind};
        let d = super::parse_capture(None, None);
        assert_eq!(d.level, ContentCapture::Structured);
        // Structured records no content.
        assert!(d.content(ContentKind::InputMessages, "a@b.com").is_none());
    }

    #[test]
    fn parse_capture_full_with_regex_scrubs() {
        use awaken_runtime_contract::{ContentCapture, ContentKind};
        let d = super::parse_capture(Some("full"), Some("regex"));
        assert_eq!(d.level, ContentCapture::Full);
        assert_eq!(
            d.content(ContentKind::InputMessages, "mail a@b.com")
                .as_deref(),
            Some("mail [redacted-email]")
        );
    }

    #[test]
    fn parse_capture_off_records_nothing_even_full_text() {
        use awaken_runtime_contract::ContentKind;
        let d = super::parse_capture(Some("off"), Some("regex"));
        assert!(d.content(ContentKind::OutputMessages, "x").is_none());
    }

    #[test]
    fn trace_file_downgrades_full_to_structured() {
        use awaken_runtime_contract::ContentCapture;
        // With the append-only trace file active, Full content is not allowed.
        assert_eq!(
            super::clamp_for_trace_file(ContentCapture::Full, true),
            ContentCapture::Structured
        );
        // Without it, Full stands.
        assert_eq!(
            super::clamp_for_trace_file(ContentCapture::Full, false),
            ContentCapture::Full
        );
        // Structured/Off are unaffected either way.
        assert_eq!(
            super::clamp_for_trace_file(ContentCapture::Structured, true),
            ContentCapture::Structured
        );
        assert_eq!(
            super::clamp_for_trace_file(ContentCapture::Off, true),
            ContentCapture::Off
        );
    }
}
