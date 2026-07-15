//! [`RedactedString`] — the one already-resolved secret value type that crosses
//! into the runtime (ADR-0043, D6/D9). The host resolves credentials and hands
//! the runtime/adapters this opaque value; it is redacted in `Debug`/`Display`,
//! deliberately **not** `Serialize`/`Deserialize` (so it can never land in a log,
//! wire payload, snapshot, or idempotency record), and zeroized on drop.
//!
//! `expose_secret()` is the single trust boundary: every caller of it is
//! responsible for not propagating the returned `&str` into any path that logs.

use zeroize::Zeroize;

/// An already-resolved secret value. See module docs.
#[derive(Clone)]
pub struct RedactedString(String);

impl RedactedString {
    /// Wrap a raw secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Reach through the redaction to the plaintext. Call only at the injection
    /// seam; do not propagate the `&str` into anything that logs it.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether the inner value is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Operator-safe preview: first four and last four characters with the middle
    /// masked (`"sk-a***wxyz"`); values shorter than twelve chars render as `"***"`
    /// so no preview reveals more than half a secret. Char-aware (never splits a
    /// UTF-8 code point). Not an identifier — do not use for auth or dedup.
    #[must_use]
    pub fn preview(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() < 12 {
            return "***".to_string();
        }
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}***{tail}")
    }
}

impl Drop for RedactedString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for RedactedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedactedString(***)")
    }
}

impl std::fmt::Display for RedactedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for RedactedString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_redacted() {
        let s = RedactedString::new("super-secret-token-value");
        assert_eq!(format!("{s}"), "***");
        assert_eq!(format!("{s:?}"), "RedactedString(***)");
        assert!(!format!("{s:?}").contains("super-secret"));
    }

    #[test]
    fn expose_returns_plaintext() {
        let s = RedactedString::new("abc");
        assert_eq!(s.expose_secret(), "abc");
    }

    #[test]
    fn preview_masks_middle_and_short_values() {
        assert_eq!(RedactedString::new("short").preview(), "***");
        assert_eq!(
            RedactedString::new("sk-abcd1234wxyz").preview(),
            "sk-a***wxyz"
        );
    }

    #[test]
    fn preview_boundary_at_twelve_chars() {
        // 11 chars -> fully masked; 12 -> head4***tail4 (the < 12 threshold).
        assert_eq!(RedactedString::new("0123456789a").preview(), "***"); // 11
        assert_eq!(RedactedString::new("0123456789ab").preview(), "0123***89ab"); // 12
    }

    #[test]
    fn preview_is_char_aware_and_never_splits_a_code_point() {
        // Twelve multibyte chars: char-aware slicing must take whole code points
        // (byte slicing here would panic). Head/tail are the first/last four chars.
        let s = RedactedString::new("αβγδεζηθικλμ"); // 12 Greek letters
        assert_eq!(s.preview(), "αβγδ***ικλμ");
        // A short multibyte value still fully masks without panicking.
        assert_eq!(RedactedString::new("日本語").preview(), "***");
    }

    #[test]
    fn is_empty_reflects_inner_value() {
        assert!(RedactedString::new("").is_empty());
        assert!(!RedactedString::new("x").is_empty());
    }

    #[test]
    fn from_string_wraps_and_stays_redacted() {
        let s: RedactedString = String::from("secret-value").into();
        assert_eq!(s.expose_secret(), "secret-value");
        assert_eq!(format!("{s}"), "***");
    }
}
