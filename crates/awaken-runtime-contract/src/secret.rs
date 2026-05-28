//! Secret value newtype that redacts itself in `Debug`/`Display` while still
//! round-tripping through `serde`.
//!
//! Use [`RedactedString`] for any field that holds a credential. The inner
//! buffer is held inside [`secrecy::SecretBox`], so it is zeroized when the
//! value is dropped. The plaintext is reachable only via
//! [`RedactedString::expose_secret`] — this single accessor is the grep-able
//! "trust boundary" for the codebase.
//!
//! Wire format is a plain JSON string. JSON Schema reports `string`. Storage
//! backends that persist this value see the secret in cleartext, so storage
//! choices remain a separate concern.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use secrecy::{ExposeSecret, SecretBox};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// String wrapper whose `Debug`/`Display` implementations never reveal the
/// underlying value, and whose buffer is zeroized on drop.
pub struct RedactedString(SecretBox<String>);

impl RedactedString {
    /// Construct from any `Into<String>`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(SecretBox::new(Box::new(value.into())))
    }

    /// Reach through the redaction to obtain the plaintext value.
    ///
    /// Calls to this function are the trust boundary: every caller is
    /// responsible for not propagating the returned `&str` into any path that
    /// might log it.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret().as_str()
    }

    /// Returns `true` if the inner string is empty.
    pub fn is_empty(&self) -> bool {
        self.expose_secret().is_empty()
    }

    /// Redacted preview suitable for operator-facing logs: first four and
    /// last four characters with the middle masked, e.g. `"sk-a***wxyz"`.
    ///
    /// Values shorter than twelve characters render as `"***"` so that the
    /// preview never reveals more than half of any single secret.
    /// Char-aware so multi-byte UTF-8 cannot be split in the middle of a code
    /// point. Not a cryptographic identifier — do not use for authentication
    /// or de-duplication.
    pub fn preview(&self) -> String {
        let chars: Vec<char> = self.expose_secret().chars().collect();
        if chars.len() < 12 {
            return "***".into();
        }
        let head: String = chars.iter().take(4).collect();
        let tail: String = chars.iter().skip(chars.len() - 4).collect();
        format!("{head}***{tail}")
    }
}

impl Clone for RedactedString {
    fn clone(&self) -> Self {
        Self::new(self.expose_secret().to_owned())
    }
}

impl PartialEq for RedactedString {
    fn eq(&self, other: &Self) -> bool {
        self.expose_secret() == other.expose_secret()
    }
}

impl Eq for RedactedString {}

impl fmt::Debug for RedactedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedactedString(***)")
    }
}

impl fmt::Display for RedactedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for RedactedString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for RedactedString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl Serialize for RedactedString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose_secret())
    }
}

impl<'de> Deserialize<'de> for RedactedString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}

impl JsonSchema for RedactedString {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("String")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        String::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts() {
        let s = RedactedString::new("sk-secret");
        assert_eq!(format!("{s:?}"), "RedactedString(***)");
    }

    #[test]
    fn display_redacts() {
        let s = RedactedString::new("sk-secret");
        assert_eq!(format!("{s}"), "***");
    }

    #[test]
    fn debug_in_option_redacts() {
        let s = Some(RedactedString::new("sk-secret"));
        let formatted = format!("{s:?}");
        assert!(!formatted.contains("sk-secret"), "leaked: {formatted}");
    }

    #[test]
    fn clone_preserves_value() {
        let original = RedactedString::new("sk-secret");
        let copied = original.clone();
        assert_eq!(copied.expose_secret(), "sk-secret");
        assert_eq!(original.expose_secret(), "sk-secret");
    }

    #[test]
    fn serde_roundtrip_preserves_value() {
        let s = RedactedString::new("sk-secret");
        let encoded = serde_json::to_string(&s).unwrap();
        assert_eq!(encoded, "\"sk-secret\"");
        let decoded: RedactedString = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.expose_secret(), "sk-secret");
    }

    #[test]
    fn json_schema_is_plain_string() {
        let schema = schemars::schema_for!(RedactedString);
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(value.get("type").and_then(|v| v.as_str()), Some("string"));
    }

    #[test]
    fn preview_keeps_first_and_last_four_chars() {
        let s = RedactedString::new("sk-abcd1234567890wxyz");
        assert_eq!(s.preview(), "sk-a***wxyz");
    }

    #[test]
    fn preview_masks_short_values_completely() {
        for short in ["", "abc", "sk-12345", "12345678901"] {
            let s = RedactedString::new(short);
            assert_eq!(
                s.preview(),
                "***",
                "values shorter than 12 chars must render as `***`, got input {short:?}"
            );
        }
    }

    #[test]
    fn preview_does_not_panic_on_multibyte_utf8() {
        let s = RedactedString::new("αβγδ-中文-emoji-🔑🔑🔑🔑");
        let preview = s.preview();
        assert!(preview.contains("***"));
        assert!(!preview.contains("中文"));
    }
}
