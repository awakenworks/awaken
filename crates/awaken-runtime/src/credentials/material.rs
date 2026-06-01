//! Typed credential material parsed from `ProviderSpec`.
//!
//! `CredentialMaterial` is the broker's internal canonical form. The
//! conversion `(credentials_kind, api_key)` → `CredentialMaterial` happens
//! at the **server boundary** (config write time + executor build time);
//! once material lives inside the broker every signer can rely on the
//! variant invariant being satisfied.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_runtime_contract::secret::RedactedString;
use serde::Deserialize;
use serde_json::Value;

use super::minter::{self, Minter};

pub(crate) const GOOGLE_OAUTH_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Opaque carrier of a parsed credential.
///
/// Internally holds an [`Arc<dyn Minter>`](Minter) — the broker dispatches
/// minting via the trait rather than a central `match`. Construct via
/// [`Self::static_bearer`] or [`Self::google_service_account`]; library
/// consumers can also produce one indirectly via [`build_material`].
#[derive(Debug, Clone)]
pub struct CredentialMaterial {
    minter: Arc<dyn Minter>,
}

impl CredentialMaterial {
    /// Static OAuth bearer / API key. Always available.
    pub fn static_bearer(bearer: RedactedString) -> Self {
        Self {
            minter: minter::static_bearer_arc(bearer),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_minter(minter: Arc<dyn Minter>) -> Self {
        Self { minter }
    }

    /// Google service-account material. Only available when the
    /// `credentials-google` feature is enabled (or in tests, which always
    /// link the signer for unit-test coverage).
    #[cfg(any(test, feature = "credentials-google"))]
    pub fn google_service_account(
        provider_id: impl Into<String>,
        key: GoogleServiceAccountKey,
    ) -> Self {
        Self {
            minter: minter::google_service_account_arc(provider_id.into(), Arc::new(key)),
        }
    }

    /// Stable telemetry label for the underlying kind.
    pub fn kind_label(&self) -> &'static str {
        self.minter.kind_label()
    }

    pub(crate) fn minter(&self) -> &Arc<dyn Minter> {
        &self.minter
    }
}

/// Parsed view of a Google service account JSON key.
///
/// Held inside an `Arc` so the broker can clone cheaply when minting tokens
/// concurrently for the same provider (single-flight still serialises the
/// actual HTTP call; the parsed key is shared).
#[derive(Debug, Clone, Deserialize)]
pub struct GoogleServiceAccountKey {
    /// Service account email (used as JWT `iss`).
    pub client_email: String,
    /// PEM-encoded RSA private key (used to sign the JWT with RS256).
    #[serde(deserialize_with = "deserialize_redacted")]
    pub private_key: RedactedString,
    /// OAuth token endpoint. Defaults to Google's standard endpoint when
    /// the service account JSON omits it.
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    /// Project id from the SA JSON. Currently unused by the signer (the
    /// project is encoded in `ProviderSpec.base_url`) but parsed so admins
    /// can be warned when the SA's home project differs from the URL's.
    #[serde(default)]
    pub project_id: Option<String>,
}

fn default_token_uri() -> String {
    GOOGLE_OAUTH_TOKEN_URI.to_owned()
}

pub(crate) fn validate_google_token_uri(token_uri: &str) -> Result<(), String> {
    if token_uri == GOOGLE_OAUTH_TOKEN_URI {
        return Ok(());
    }
    Err(format!(
        "service account JSON token_uri must be {GOOGLE_OAUTH_TOKEN_URI}; got '{token_uri}'"
    ))
}

fn deserialize_redacted<'de, D>(d: D) -> Result<RedactedString, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    Ok(RedactedString::new(s))
}

impl GoogleServiceAccountKey {
    /// Parse a service-account JSON string. Validates the minimum fields
    /// the signer needs (`client_email`, `private_key`, with `private_key`
    /// looking like a PEM block).
    pub fn parse(json: &str) -> Result<Self, String> {
        let key: Self = serde_json::from_str(json)
            .map_err(|e| format!("not a valid service account JSON: {e}"))?;
        if key.client_email.trim().is_empty() {
            return Err("service account JSON missing 'client_email'".into());
        }
        if key.private_key.is_empty() {
            return Err("service account JSON missing 'private_key'".into());
        }
        if !key.private_key.expose_secret().contains("BEGIN") {
            return Err("'private_key' does not look like a PEM block".into());
        }
        validate_google_token_uri(&key.token_uri)?;
        Ok(key)
    }
}

/// String form of a credential kind, used in `adapter_options.credentials_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// Default — `api_key` is a pre-signed bearer.
    Bearer,
    /// Google service account JSON in `api_key`.
    GoogleServiceAccountJson,
}

impl CredentialKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::GoogleServiceAccountJson => "service_account_json",
        }
    }

    /// Parse from the `adapter_options.credentials_kind` value. Absence
    /// resolves to [`CredentialKind::Bearer`] (back-compat with 0.4.0
    /// configs that never set the field).
    pub fn from_options(options: &BTreeMap<String, Value>) -> Result<Self, String> {
        let Some(value) = options.get("credentials_kind") else {
            return Ok(Self::Bearer);
        };
        let Some(s) = value.as_str() else {
            return Err(format!(
                "adapter_options.credentials_kind must be a string, got {value}"
            ));
        };
        match s {
            "bearer" => Ok(Self::Bearer),
            "service_account_json" => Ok(Self::GoogleServiceAccountJson),
            other => Err(format!(
                "unknown adapter_options.credentials_kind '{other}' (valid: bearer, service_account_json)"
            )),
        }
    }
}

/// Adapter strings this kind is allowed for. Empty array means "no
/// restriction"; a non-empty array means "only these adapters".
///
/// Returned from validation so the server can produce a precise error
/// message at config write time without depending on an exhaustive match.
fn compatible_adapters(kind: CredentialKind) -> &'static [&'static str] {
    match kind {
        CredentialKind::Bearer => &[],
        // Vertex is the only adapter today that mints OAuth tokens from a
        // Google service account JWT. (Other Google products use their own
        // auth flows even when nominally on GCP.)
        CredentialKind::GoogleServiceAccountJson => &["vertex"],
    }
}

/// Build typed material from raw fields (adapter + kind + api_key).
///
/// Used at config write time for **eager validation** — the server rejects
/// invalid (kind × adapter × api_key shape) combinations with a precise
/// error rather than letting them blow up at first inference.
///
/// Also used at executor build time to decide whether to register material
/// with the broker.
///
/// Returns:
/// - `Ok(Some(material))` — material is configured; register with the broker.
/// - `Ok(None)` — only from [`build_material_allowing_env_fallback`] when
///   `kind == Bearer` and `api_key` is absent/empty. The runtime should skip
///   broker registration and let genai's adapter fall back to its default env
///   var (e.g. `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`). Callers must opt in
///   explicitly so managed provider configs do not silently borrow host
///   credentials.
/// - `Err(_)` — incompatible (kind × adapter), missing required api_key for
///   bearer/non-bearer kinds, the `credentials-google` feature disabled but a
///   service-account configuration was supplied, or unparseable material.
pub fn build_material(
    adapter: &str,
    kind: CredentialKind,
    api_key: Option<&RedactedString>,
) -> Result<Option<CredentialMaterial>, String> {
    build_material_inner(adapter, kind, api_key, false)
}

/// Build credential material while explicitly allowing bearer env fallback.
///
/// Server-managed provider configs should call this only when their parsed
/// adapter options contain `allow_env_credentials = true`.
pub fn build_material_allowing_env_fallback(
    adapter: &str,
    kind: CredentialKind,
    api_key: Option<&RedactedString>,
) -> Result<Option<CredentialMaterial>, String> {
    build_material_inner(adapter, kind, api_key, true)
}

/// Parse the explicit env-credential opt-in from provider adapter options.
pub fn allow_env_credentials_from_options(
    options: &BTreeMap<String, Value>,
) -> Result<bool, String> {
    let Some(value) = options.get("allow_env_credentials") else {
        return Ok(false);
    };
    value.as_bool().ok_or_else(|| {
        "adapter_options.allow_env_credentials must be a boolean when present".to_string()
    })
}

fn build_material_inner(
    adapter: &str,
    kind: CredentialKind,
    api_key: Option<&RedactedString>,
    allow_env_fallback: bool,
) -> Result<Option<CredentialMaterial>, String> {
    // (kind × adapter) compatibility
    let allowed = compatible_adapters(kind);
    if !allowed.is_empty() && !allowed.contains(&adapter) {
        return Err(format!(
            "credentials_kind '{}' requires adapter ∈ [{}]; got '{adapter}'",
            kind.as_str(),
            allowed.join(", ")
        ));
    }

    match kind {
        CredentialKind::Bearer => {
            let Some(key) = api_key.filter(|k| !k.is_empty()) else {
                return if allow_env_fallback {
                    Ok(None)
                } else {
                    Err("credentials_kind 'bearer' requires api_key unless \
                         adapter_options.allow_env_credentials is true"
                        .to_string())
                };
            };
            Ok(Some(CredentialMaterial::static_bearer(key.clone())))
        }
        CredentialKind::GoogleServiceAccountJson => {
            // Reject at the build boundary when the signer feature is off
            // — a runtime stub would otherwise produce an opaque "not
            // configured" error at first inference. Operators get the
            // precise reason at config write time instead.
            #[cfg(not(any(test, feature = "credentials-google")))]
            {
                let _ = api_key;
                return Err("credentials_kind 'service_account_json' requires the \
                            `credentials-google` feature to be enabled at build time"
                    .to_owned());
            }
            #[cfg(any(test, feature = "credentials-google"))]
            {
                let key = api_key.ok_or_else(|| {
                    "credentials_kind 'service_account_json' requires api_key with the JSON content"
                        .to_owned()
                })?;
                let parsed = GoogleServiceAccountKey::parse(key.expose_secret())?;
                // provider_id isn't known at this layer — embed a placeholder;
                // the broker re-stamps with the real id when registering. The
                // signer only uses provider_id for telemetry.
                Ok(Some(CredentialMaterial::google_service_account(
                    String::new(),
                    parsed,
                )))
            }
        }
    }
}

impl From<&CredentialMaterial> for &'static str {
    /// Stable label for telemetry / logging.
    fn from(m: &CredentialMaterial) -> Self {
        m.kind_label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts(kind: Option<&str>) -> BTreeMap<String, Value> {
        let mut o = BTreeMap::new();
        if let Some(k) = kind {
            o.insert("credentials_kind".into(), json!(k));
        }
        o
    }

    #[test]
    fn kind_defaults_to_bearer_when_absent() {
        assert_eq!(
            CredentialKind::from_options(&opts(None)).unwrap(),
            CredentialKind::Bearer
        );
    }

    #[test]
    fn kind_recognises_explicit_bearer() {
        assert_eq!(
            CredentialKind::from_options(&opts(Some("bearer"))).unwrap(),
            CredentialKind::Bearer
        );
    }

    #[test]
    fn kind_recognises_service_account_json() {
        assert_eq!(
            CredentialKind::from_options(&opts(Some("service_account_json"))).unwrap(),
            CredentialKind::GoogleServiceAccountJson
        );
    }

    #[test]
    fn kind_rejects_unknown_string_with_helpful_message() {
        let err = CredentialKind::from_options(&opts(Some("not-a-kind"))).unwrap_err();
        assert!(err.contains("not-a-kind"));
        assert!(err.contains("bearer") && err.contains("service_account_json"));
    }

    #[test]
    fn kind_rejects_non_string_value() {
        let mut o = BTreeMap::new();
        o.insert("credentials_kind".into(), json!(42));
        assert!(
            CredentialKind::from_options(&o)
                .unwrap_err()
                .contains("must be a string")
        );
    }

    // -- build_material -----------------------------------------------------

    #[test]
    fn build_material_bearer_with_key_returns_static_bearer() {
        let key = RedactedString::new("sk-test-123");
        let m = build_material("openai", CredentialKind::Bearer, Some(&key))
            .unwrap()
            .expect("Some material");
        assert_eq!(m.kind_label(), "bearer");
    }

    #[test]
    fn build_material_bearer_without_key_is_rejected_by_default() {
        let err = build_material("openai", CredentialKind::Bearer, None)
            .expect_err("missing bearer api_key must fail closed");
        assert!(err.contains("api_key"));
    }

    #[test]
    fn build_material_bearer_env_fallback_requires_explicit_opt_in() {
        assert!(
            build_material_allowing_env_fallback("openai", CredentialKind::Bearer, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn build_material_bearer_with_empty_key_is_rejected_by_default() {
        let key = RedactedString::new("");
        let err = build_material("openai", CredentialKind::Bearer, Some(&key))
            .expect_err("empty bearer api_key must fail closed");
        assert!(err.contains("api_key"));
    }

    #[test]
    fn build_material_service_account_kind_requires_vertex_adapter() {
        let key = RedactedString::new(r#"{"client_email":"x","private_key":"-----BEGIN"}"#);
        let err = build_material(
            "openai",
            CredentialKind::GoogleServiceAccountJson,
            Some(&key),
        )
        .unwrap_err();
        assert!(
            err.contains("service_account_json")
                && err.contains("vertex")
                && err.contains("openai"),
            "expected message naming kind/adapter mismatch, got: {err}"
        );
    }

    #[test]
    fn build_material_service_account_kind_requires_api_key() {
        let err =
            build_material("vertex", CredentialKind::GoogleServiceAccountJson, None).unwrap_err();
        assert!(
            err.contains("service_account_json") && err.contains("api_key"),
            "expected message naming missing api_key, got: {err}"
        );
    }

    #[test]
    fn build_material_service_account_kind_rejects_garbage_json() {
        let key = RedactedString::new("this is not json");
        let err = build_material(
            "vertex",
            CredentialKind::GoogleServiceAccountJson,
            Some(&key),
        )
        .unwrap_err();
        assert!(
            err.contains("service account JSON"),
            "expected SA JSON parse error, got: {err}"
        );
    }

    #[test]
    fn build_material_service_account_kind_rejects_json_missing_client_email() {
        let key = RedactedString::new(r#"{"private_key":"-----BEGIN PRIVATE KEY-----\n..."}"#);
        let err = build_material(
            "vertex",
            CredentialKind::GoogleServiceAccountJson,
            Some(&key),
        )
        .unwrap_err();
        // serde rejects the missing field at deserialize time; either error
        // shape names the missing field, so we assert on the field name.
        assert!(
            err.contains("client_email"),
            "expected error mentioning client_email, got: {err}"
        );
    }

    #[test]
    fn build_material_service_account_kind_rejects_json_missing_private_key() {
        let key = RedactedString::new(r#"{"client_email":"sa@p.iam.gserviceaccount.com"}"#);
        let err = build_material(
            "vertex",
            CredentialKind::GoogleServiceAccountJson,
            Some(&key),
        )
        .unwrap_err();
        assert!(
            err.contains("private_key"),
            "expected error mentioning private_key, got: {err}"
        );
    }

    #[test]
    fn build_material_service_account_kind_rejects_private_key_not_pem() {
        let key =
            RedactedString::new(r#"{"client_email":"sa@p","private_key":"raw-bytes-not-pem"}"#);
        let err = build_material(
            "vertex",
            CredentialKind::GoogleServiceAccountJson,
            Some(&key),
        )
        .unwrap_err();
        assert!(err.contains("PEM"), "expected error about PEM, got: {err}");
    }

    #[test]
    fn build_material_service_account_kind_with_well_formed_json_returns_material() {
        let sa = RedactedString::new(
            r#"{
                "type":"service_account",
                "client_email":"sa@p.iam.gserviceaccount.com",
                "private_key":"-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----",
                "project_id":"p"
            }"#,
        );
        let m = build_material(
            "vertex",
            CredentialKind::GoogleServiceAccountJson,
            Some(&sa),
        )
        .unwrap()
        .expect("Some material");
        assert_eq!(m.kind_label(), "service_account_json");
    }

    // -- GoogleServiceAccountKey::parse direct tests ------------------------

    #[test]
    fn google_sa_key_parse_uses_default_token_uri_when_absent() {
        let json = r#"{
            "client_email":"sa@p.iam.gserviceaccount.com",
            "private_key":"-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----"
        }"#;
        let key = GoogleServiceAccountKey::parse(json).unwrap();
        assert_eq!(key.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn google_sa_key_parse_accepts_standard_explicit_token_uri() {
        let json = r#"{
            "client_email":"sa@p.iam.gserviceaccount.com",
            "private_key":"-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----",
            "token_uri":"https://oauth2.googleapis.com/token"
        }"#;
        let key = GoogleServiceAccountKey::parse(json).unwrap();
        assert_eq!(key.token_uri, GOOGLE_OAUTH_TOKEN_URI);
    }

    #[test]
    fn google_sa_key_parse_rejects_custom_token_uri() {
        let json = r#"{
            "client_email":"sa@p.iam.gserviceaccount.com",
            "private_key":"-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----",
            "token_uri":"https://custom.example/token"
        }"#;
        let err = GoogleServiceAccountKey::parse(json).unwrap_err();
        assert!(
            err.contains(GOOGLE_OAUTH_TOKEN_URI) && err.contains("custom.example"),
            "expected token_uri allowlist error, got: {err}"
        );
    }

    #[test]
    fn google_sa_key_parse_rejects_ssrf_token_uri_corpus() {
        let corpus = [
            "http://127.0.0.1/token",
            "http://127.0.0.1:8080/token",
            "http://localhost/token",
            "http://[::1]/token",
            "http://10.0.0.1/token",
            "http://172.16.0.1/token",
            "http://192.168.1.1/token",
            "http://169.254.169.254/latest/meta-data",
            "https://oauth2.googleapis.com.evil.example/token",
            "https://oauth2.googleapis.com@evil.example/token",
            "https://oauth2.googleapis.com./token",
            "https://evil.example/redirect?next=https%3A%2F%2Foauth2.googleapis.com%2Ftoken",
            "HTTPS://oauth2.googleapis.com/token",
        ];

        for token_uri in corpus {
            let json = serde_json::json!({
                "client_email": "sa@p.iam.gserviceaccount.com",
                "private_key": "-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----",
                "token_uri": token_uri,
            })
            .to_string();
            let err = GoogleServiceAccountKey::parse(&json)
                .expect_err("non-allowlisted token_uri must be rejected");
            assert!(
                err.contains(GOOGLE_OAUTH_TOKEN_URI) && err.contains(token_uri),
                "expected allowlist error mentioning canonical endpoint and rejected URI, got: {err}"
            );
        }
    }
}
