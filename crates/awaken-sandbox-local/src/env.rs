//! Environment variable declarations and egress secret substitution.
//!
//! `EnvVar` combines a name, a value (inline literal or broker reference),
//! and a visibility flag.  The provider builds the process environment from
//! these declarations:
//!
//! - `Process` secrets are resolved and injected as real values.
//! - `EgressOnly` secrets are replaced with an opaque placeholder in the
//!   process environment; the returned [`EgressReplacer`] substitutes the
//!   real bytes in outgoing HTTP request bodies / headers.

use bytes::Bytes;

// ── Value and visibility ──────────────────────────────────────────────────────

/// The concrete value of an env var.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    /// A non-secret literal (e.g. `TZ=UTC`, `NODE_ENV=production`).
    Inline(String),
    /// A secret resolved by the broker at environment-build time.  Only the
    /// opaque reference crosses the contract seam — never the bytes.
    Secret { reference: String },
}

/// Where the resolved value is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvVisibility {
    /// The process receives the real value.
    #[default]
    Process,
    /// The process receives a deterministic placeholder; the real bytes are
    /// substituted by [`EgressReplacer`] in outgoing data.  Use for secrets
    /// that must never appear in process memory (e.g. bearer tokens injected
    /// into HTTP headers by a sidecar).
    EgressOnly,
}

/// An environment variable declaration.
#[derive(Debug, Clone)]
pub struct EnvVar {
    pub name: String,
    pub value: EnvValue,
    pub visibility: EnvVisibility,
}

impl EnvVar {
    /// Construct a plain inline env var (visible to the process).
    pub fn inline(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: EnvValue::Inline(value.into()),
            visibility: EnvVisibility::Process,
        }
    }

    /// Construct a secret env var whose real value is visible to the process.
    pub fn secret_process(name: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: EnvValue::Secret {
                reference: reference.into(),
            },
            visibility: EnvVisibility::Process,
        }
    }

    /// Construct a secret env var whose real value is substituted only at
    /// network egress (the process sees a placeholder).
    pub fn secret_egress_only(name: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: EnvValue::Secret {
                reference: reference.into(),
            },
            visibility: EnvVisibility::EgressOnly,
        }
    }
}

// ── Placeholder generation ────────────────────────────────────────────────────

/// Generate a deterministic, collision-resistant placeholder token for a
/// secret `reference`.
///
/// The token is safe to embed in environment variable values and HTTP
/// headers; it contains only alphanumeric characters and underscores.
/// The FNV-1a 64-bit hash gives uniqueness across distinct references
/// without pulling in a heavyweight hash dependency.
pub fn egress_placeholder(reference: &str) -> String {
    let mut h: u64 = 14_695_981_039_346_656_037;
    for b in reference.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(1_099_511_628_211);
    }
    format!("__AWAKEN_EGRESS_{h:016x}__")
}

// ── EgressReplacer ────────────────────────────────────────────────────────────

/// Substitutes egress-only secret placeholders in outgoing bytes.
///
/// Build one via [`EgressReplacerBuilder`], then call
/// [`replace_in_bytes`][Self::replace_in_bytes] on each outgoing HTTP request
/// body or header value before it leaves the sandbox boundary.
pub struct EgressReplacer {
    /// (placeholder_bytes, secret_bytes) pairs.
    pairs: Vec<(Vec<u8>, Bytes)>,
}

impl EgressReplacer {
    pub fn builder() -> EgressReplacerBuilder {
        EgressReplacerBuilder::default()
    }

    /// Returns `true` when there are no substitution pairs.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Substitute all registered placeholder → secret pairs in `data`.
    ///
    /// The replacement is applied left-to-right for each pair; overlapping
    /// matches are not re-scanned.  This is O(n × m) in the input length
    /// and number of pairs — sufficient for header / short-body use cases.
    pub fn replace_in_bytes(&self, data: &[u8]) -> Vec<u8> {
        if self.pairs.is_empty() {
            return data.to_vec();
        }
        let mut result = data.to_vec();
        for (placeholder, secret) in &self.pairs {
            result = replace_all(&result, placeholder, secret);
        }
        result
    }

    /// Convenience wrapper for UTF-8 text (header values, JSON bodies).
    ///
    /// Non-UTF-8 bytes produced by the replacement pass through unchanged.
    pub fn replace_in_str(&self, s: &str) -> String {
        if self.pairs.is_empty() {
            return s.to_owned();
        }
        String::from_utf8_lossy(&self.replace_in_bytes(s.as_bytes())).into_owned()
    }
}

/// Builder for [`EgressReplacer`].
#[derive(Default)]
pub struct EgressReplacerBuilder {
    pairs: Vec<(Vec<u8>, Bytes)>,
}

impl EgressReplacerBuilder {
    /// Register a `(placeholder, secret_bytes)` pair.
    ///
    /// The placeholder is typically the value returned by
    /// [`egress_placeholder`] for the same reference; it must be identical
    /// to the string injected into the process environment.
    pub fn add(mut self, placeholder: impl AsRef<[u8]>, secret: Bytes) -> Self {
        self.pairs.push((placeholder.as_ref().to_vec(), secret));
        self
    }

    pub fn build(self) -> EgressReplacer {
        EgressReplacer { pairs: self.pairs }
    }
}

// ── Internal helper ───────────────────────────────────────────────────────────

fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if haystack[i..i + needle.len()] == *needle {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&haystack[i..]);
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_placeholder_is_deterministic() {
        let p1 = egress_placeholder("provider://anthropic/key");
        let p2 = egress_placeholder("provider://anthropic/key");
        assert_eq!(p1, p2);
    }

    #[test]
    fn egress_placeholder_differs_for_distinct_references() {
        let p1 = egress_placeholder("provider://a/key");
        let p2 = egress_placeholder("provider://b/key");
        assert_ne!(p1, p2);
    }

    #[test]
    fn egress_placeholder_format_is_safe_for_env_vars() {
        let p = egress_placeholder("some-reference");
        assert!(
            p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "placeholder must be safe for env vars: {p}"
        );
    }

    #[test]
    fn replacer_substitutes_placeholder_in_bytes() {
        let placeholder = egress_placeholder("ref://key");
        let secret = Bytes::from_static(b"sk-supersecret");
        let replacer = EgressReplacer::builder()
            .add(placeholder.as_bytes(), secret.clone())
            .build();

        let input = format!("Bearer {placeholder}");
        let output = replacer.replace_in_str(&input);
        assert_eq!(output, "Bearer sk-supersecret");
    }

    #[test]
    fn replacer_is_idempotent_on_no_match() {
        let placeholder = egress_placeholder("ref://key");
        let replacer = EgressReplacer::builder()
            .add(placeholder.as_bytes(), Bytes::from_static(b"secret"))
            .build();

        let input = b"no placeholder here";
        let output = replacer.replace_in_bytes(input);
        assert_eq!(output, input);
    }

    #[test]
    fn replacer_handles_multiple_pairs() {
        let p1 = egress_placeholder("ref://a");
        let p2 = egress_placeholder("ref://b");
        let replacer = EgressReplacer::builder()
            .add(p1.as_bytes(), Bytes::from_static(b"secret-a"))
            .add(p2.as_bytes(), Bytes::from_static(b"secret-b"))
            .build();

        let input = format!("{p1} and {p2}");
        let output = replacer.replace_in_str(&input);
        assert_eq!(output, "secret-a and secret-b");
    }

    #[test]
    fn empty_replacer_is_noop() {
        let replacer = EgressReplacer::builder().build();
        assert!(replacer.is_empty());
        let data = b"unchanged data";
        assert_eq!(replacer.replace_in_bytes(data), data);
    }

    #[test]
    fn replacer_handles_binary_secrets() {
        let placeholder = egress_placeholder("bin://key");
        let secret_bytes = vec![0u8, 1, 2, 255];
        let replacer = EgressReplacer::builder()
            .add(placeholder.as_bytes(), Bytes::from(secret_bytes.clone()))
            .build();

        let mut input = placeholder.as_bytes().to_vec();
        input.extend_from_slice(b"-suffix");
        let output = replacer.replace_in_bytes(&input);
        let mut expected = secret_bytes;
        expected.extend_from_slice(b"-suffix");
        assert_eq!(output, expected);
    }

    #[test]
    fn env_var_constructors() {
        let inline = EnvVar::inline("TZ", "UTC");
        assert!(matches!(inline.value, EnvValue::Inline(v) if v == "UTC"));
        assert_eq!(inline.visibility, EnvVisibility::Process);

        let sp = EnvVar::secret_process("API_KEY", "ref://key");
        assert!(matches!(sp.value, EnvValue::Secret { .. }));
        assert_eq!(sp.visibility, EnvVisibility::Process);

        let eo = EnvVar::secret_egress_only("BEARER_TOKEN", "ref://bearer");
        assert_eq!(eo.visibility, EnvVisibility::EgressOnly);
    }
}
