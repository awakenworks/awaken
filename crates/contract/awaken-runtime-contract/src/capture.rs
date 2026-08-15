//! Content-capture level and the resolved privacy primitive (ADR-0050).
//!
//! Telemetry/eval splits into two data classes: **structure** (span tree,
//! latency, token counts — generally not personal data) and **content**
//! (prompt/completion/tool text — personal data under GDPR). [`ContentCapture`]
//! is the total-ordered lattice deciding how much content a run may record;
//! [`CaptureDecision`] is the single opaque primitive the runtime/sinks receive
//! for one Run — they hold only this, never scope, consent, or subject
//! attributes (D5).

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// How much content a run may record: a total order `Off ⊏ Structured ⊏ Full`
/// whose greatest-lower-bound ([`meet`](ContentCapture::meet)) is the only
/// combinator. Composition down the Org→Workspace→Agent→Session→consent chain
/// is repeated `meet`, so a lower layer can only tighten, never widen (D2).
///
/// Wire values are `off`/`structured`/`full`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentCapture {
    /// No telemetry content at all.
    Off,
    /// Structure only — spans/metrics without prompt/completion/tool content.
    /// The safe operational default.
    #[default]
    Structured,
    /// Full content (still passed through the [`ContentRedactor`]).
    Full,
}

impl ContentCapture {
    /// Greatest lower bound: the stricter (lower) of two levels. Variants are
    /// ordered `Off < Structured < Full`, so `meet` is `min`.
    #[must_use]
    pub fn meet(self, other: Self) -> Self {
        self.min(other)
    }

    /// Whether prompt/completion/tool content may be recorded at this level.
    #[must_use]
    pub fn allows_content(self) -> bool {
        self.projection() == ContentProjection::Redact
    }

    /// Select the only record-safe handling for content at this level.
    ///
    /// Keeping this as a closed, allocation-free decision kernel lets every
    /// sink share the same fail-closed rule: `Full` may enter the configured
    /// redactor, while `Off` and `Structured` omit the content before the
    /// redactor is invoked.
    #[must_use]
    pub(crate) const fn projection(self) -> ContentProjection {
        match self {
            Self::Full => ContentProjection::Redact,
            Self::Off | Self::Structured => ContentProjection::Omit,
        }
    }
}

/// Record-safe projection selected before any content sink or redactor runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentProjection {
    /// Do not expose the content to the persistence projection at all.
    Omit,
    /// Pass the content through the configured redactor before persistence.
    Redact,
}

/// The kind of content passing through a [`ContentRedactor`], so a redactor can
/// treat model I/O and tool payloads differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKind {
    /// Prompt / input messages sent to the model.
    InputMessages,
    /// Completion / output messages returned by the model.
    OutputMessages,
    /// Tool-call arguments.
    ToolArguments,
    /// Tool-call result payload.
    ToolResult,
}

/// Scrubs PII from free-text content **before** it is persisted to a span
/// attribute, eval fixture, or event history (D6). Orthogonal to the capture
/// level: the level decides *which* fields are recorded, the redactor decides
/// *how thoroughly the recorded text is scrubbed*. Never runs on the inference
/// hot path — only on the persisted projection.
pub trait ContentRedactor: Send + Sync {
    /// Return `text` scrubbed of PII (borrow it unchanged when nothing matched).
    fn redact<'a>(&self, kind: ContentKind, text: &'a str) -> Cow<'a, str>;
}

/// Null-object redactor: passes text through unchanged. The default for the
/// single-machine/open build; real scrubbing (`RegexPiiRedactor`) lives in
/// `awaken-observability`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRedactor;

impl ContentRedactor for NoopRedactor {
    fn redact<'a>(&self, _kind: ContentKind, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }
}

/// The resolved, opaque privacy primitive handed to the runtime/sinks for one
/// Run (D5). Resolution (the `meet` of ceiling × request × consent) happens
/// at the config/host boundary; the runtime holds only `{level, redactor}` and
/// an opaque subject id — never scope, consent, or subject attributes.
#[derive(Clone)]
pub struct CaptureDecision {
    /// The effective (already `meet`-resolved) capture level.
    pub level: ContentCapture,
    /// The redactor applied to any recorded content.
    pub redactor: Arc<dyn ContentRedactor>,
}

impl CaptureDecision {
    /// A decision at `level` with the null redactor.
    #[must_use]
    pub fn new(level: ContentCapture) -> Self {
        Self {
            level,
            redactor: Arc::new(NoopRedactor),
        }
    }

    /// A decision at `level` with an explicit redactor.
    #[must_use]
    pub fn with_redactor(level: ContentCapture, redactor: Arc<dyn ContentRedactor>) -> Self {
        Self { level, redactor }
    }

    /// Record-safe content: `None` when the level forbids content, otherwise the
    /// redactor-scrubbed text. This is the single gate every content sink calls.
    #[must_use]
    pub fn content<'a>(&self, kind: ContentKind, text: &'a str) -> Option<Cow<'a, str>> {
        match self.level.projection() {
            ContentProjection::Omit => None,
            ContentProjection::Redact => Some(self.redactor.redact(kind, text)),
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{ContentCapture, ContentProjection};

    fn arbitrary_capture() -> ContentCapture {
        match kani::any::<u8>() % 3 {
            0 => ContentCapture::Off,
            1 => ContentCapture::Structured,
            _ => ContentCapture::Full,
        }
    }

    #[kani::proof]
    fn capture_meet_is_exact_commutative_and_non_widening() {
        let left = arbitrary_capture();
        let right = arbitrary_capture();
        let effective = left.meet(right);

        assert_eq!(effective, left.min(right));
        assert_eq!(effective, right.meet(left));
        assert!(effective <= left);
        assert!(effective <= right);
    }

    #[kani::proof]
    fn capture_projection_admits_content_only_at_full() {
        let level = arbitrary_capture();
        assert_eq!(
            level.projection(),
            if level == ContentCapture::Full {
                ContentProjection::Redact
            } else {
                ContentProjection::Omit
            }
        );
        assert_eq!(level.allows_content(), level == ContentCapture::Full);
    }

    #[kani::proof]
    fn non_full_capture_fails_closed_before_redaction() {
        let level = arbitrary_capture();
        if level != ContentCapture::Full {
            assert_eq!(level.projection(), ContentProjection::Omit);
            assert!(!level.allows_content());
        }
    }
}

impl Default for CaptureDecision {
    fn default() -> Self {
        Self::new(ContentCapture::default())
    }
}

impl std::fmt::Debug for CaptureDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureDecision")
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meet_takes_the_stricter_level() {
        use ContentCapture::{Full, Off, Structured};
        assert_eq!(Full.meet(Structured), Structured);
        assert_eq!(Structured.meet(Off), Off);
        assert_eq!(Full.meet(Full), Full);
        assert_eq!(Off.meet(Full), Off);
        // meet is commutative.
        assert_eq!(Full.meet(Off), Off.meet(Full));
    }

    #[test]
    fn ordering_is_off_lt_structured_lt_full() {
        use ContentCapture::{Full, Off, Structured};
        assert!(Off < Structured && Structured < Full);
    }

    #[test]
    fn default_is_structured() {
        assert_eq!(ContentCapture::default(), ContentCapture::Structured);
    }

    #[test]
    fn only_full_allows_content() {
        assert!(ContentCapture::Full.allows_content());
        assert!(!ContentCapture::Structured.allows_content());
        assert!(!ContentCapture::Off.allows_content());
    }

    #[test]
    fn wire_values_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&ContentCapture::Structured).unwrap(),
            "\"structured\""
        );
        assert_eq!(
            serde_json::from_str::<ContentCapture>("\"full\"").unwrap(),
            ContentCapture::Full
        );
    }

    #[test]
    fn decision_gates_content_by_level() {
        let structured = CaptureDecision::new(ContentCapture::Structured);
        assert!(
            structured
                .content(ContentKind::InputMessages, "hi")
                .is_none()
        );

        let full = CaptureDecision::new(ContentCapture::Full);
        assert_eq!(
            full.content(ContentKind::InputMessages, "hi").unwrap(),
            "hi"
        );

        let off = CaptureDecision::new(ContentCapture::Off);
        assert!(off.content(ContentKind::ToolResult, "x").is_none());
    }

    #[test]
    fn default_decision_is_structured_and_records_no_content() {
        let d = CaptureDecision::default();
        assert_eq!(d.level, ContentCapture::Structured);
        assert!(d.content(ContentKind::OutputMessages, "secret").is_none());
    }

    #[test]
    fn with_redactor_carries_level_and_debug_hides_the_redactor() {
        let d = CaptureDecision::with_redactor(ContentCapture::Full, Arc::new(NoopRedactor));
        assert_eq!(d.level, ContentCapture::Full);
        let dbg = format!("{d:?}");
        assert!(dbg.contains("CaptureDecision") && dbg.contains("Full"));
    }

    #[test]
    fn noop_redactor_passes_every_kind_through_borrowed() {
        for kind in [
            ContentKind::InputMessages,
            ContentKind::OutputMessages,
            ContentKind::ToolArguments,
            ContentKind::ToolResult,
        ] {
            let out = NoopRedactor.redact(kind, "unchanged a@b.com");
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
            assert_eq!(out, "unchanged a@b.com");
        }
    }
}
