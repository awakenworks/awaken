//! Console authentication middleware: offline JWT/JWKS verification.
//!
//! [`IamClient`] caches JWKS public keys fetched from the configured
//! `jwks_url` and exposes [`IamClient::verify_access_token`] for offline
//! JWT validation. [`AuthzTransport`] is the axum middleware layer that
//! extracts the Bearer token, verifies it, checks tenant and
//! operator/tenant-admin role claims, and injects [`ConsoleIdentity`] into
//! request extensions. Any failure fails closed (HTTP 401).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, TokenData, Validation};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::routes::ApiError;

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

/// Roles admitted by the console authentication middleware.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleRole {
    Operator,
    TenantAdmin,
}

impl ConsoleRole {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "operator" => Some(Self::Operator),
            "tenant-admin" => Some(Self::TenantAdmin),
            _ => None,
        }
    }
}

impl std::fmt::Display for ConsoleRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Operator => f.write_str("operator"),
            Self::TenantAdmin => f.write_str("tenant-admin"),
        }
    }
}

// ---------------------------------------------------------------------------
// Identity (injected into request extensions)
// ---------------------------------------------------------------------------

/// Verified console identity extracted from a validated access token.
///
/// Injected into Axum request extensions by [`AuthzTransport`]; handlers
/// can extract it with `Extension<ConsoleIdentity>`.
#[derive(Debug, Clone)]
pub struct ConsoleIdentity {
    /// JWT `sub` claim.
    pub subject: String,
    /// Tenant identifier from the `tenant_id` claim.
    pub tenant_id: String,
    /// The highest-privileged admitted role found in the token.
    pub role: ConsoleRole,
}

// ---------------------------------------------------------------------------
// JWT claims
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AccessTokenClaims {
    sub: String,
    #[serde(default)]
    tenant_id: String,
    /// Single-role field (string).
    #[serde(default)]
    role: Option<String>,
    /// Multi-role field (list of strings).
    #[serde(default)]
    roles: Vec<String>,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// JWKS configuration for offline access-token verification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JwksConfig {
    /// URL from which to fetch the public JWKS, e.g.
    /// `https://iam.example.com/.well-known/jwks.json`.
    pub jwks_url: String,
    /// How long to cache the key set before re-fetching (seconds).
    #[serde(default = "default_jwks_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// If non-empty, the `aud` claim in the token must match one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audiences: Vec<String>,
}

fn default_jwks_cache_ttl_secs() -> u64 {
    3600
}

impl Default for JwksConfig {
    fn default() -> Self {
        Self {
            jwks_url: String::new(),
            cache_ttl_secs: default_jwks_cache_ttl_secs(),
            audiences: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IamError {
    #[error("JWKS fetch failed: {0}")]
    JwksFetch(String),
    #[error("no matching key found in JWKS for token")]
    KeyNotFound,
    #[error("token verification failed: {0}")]
    Verification(#[from] jsonwebtoken::errors::Error),
    #[error("missing tenant_id claim")]
    MissingTenant,
    #[error("no admitted role (operator or tenant-admin) found in token")]
    RoleDenied,
}

// ---------------------------------------------------------------------------
// IamClient
// ---------------------------------------------------------------------------

struct JwksCache {
    key_set: JwkSet,
    fetched_at: Instant,
}

/// IAM client: caches JWKS keys and provides [`Self::verify_access_token`].
///
/// Construct via [`IamClient::new`]; share as `Arc<IamClient>` across
/// request handlers. The key cache is refreshed after `cache_ttl_secs`.
pub struct IamClient {
    config: JwksConfig,
    http: reqwest::Client,
    cache: RwLock<Option<JwksCache>>,
}

impl IamClient {
    /// Create a new client from config. The first `verify_access_token` call
    /// will trigger a JWKS fetch.
    pub fn new(config: JwksConfig, http: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            config,
            http,
            cache: RwLock::new(None),
        })
    }

    /// Fetch a fresh [`JwkSet`] from the configured JWKS URL.
    async fn fetch_jwks(&self) -> Result<JwkSet, IamError> {
        let resp = self
            .http
            .get(&self.config.jwks_url)
            .send()
            .await
            .map_err(|e| IamError::JwksFetch(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(IamError::JwksFetch(format!(
                "JWKS endpoint returned {}",
                resp.status()
            )));
        }
        resp.json::<JwkSet>()
            .await
            .map_err(|e| IamError::JwksFetch(e.to_string()))
    }

    /// Return a cached [`JwkSet`], refreshing if the cache is stale or empty.
    async fn current_jwks(&self) -> Result<JwkSet, IamError> {
        let ttl = Duration::from_secs(self.config.cache_ttl_secs);

        // Fast read-lock path: cache is fresh.
        {
            let guard = self.cache.read();
            if let Some(ref c) = *guard
                && c.fetched_at.elapsed() < ttl
            {
                return Ok(c.key_set.clone());
            }
        }

        // Slow path: fetch and update.
        let fresh = self.fetch_jwks().await?;
        *self.cache.write() = Some(JwksCache {
            key_set: fresh.clone(),
            fetched_at: Instant::now(),
        });
        Ok(fresh)
    }

    /// Offline-verify `token` against the cached JWKS.
    ///
    /// Extracts the `kid` header (if present) to locate the matching public
    /// key, then verifies the signature and standard claims. On success
    /// returns a [`ConsoleIdentity`] with `tenant_id` and the first admitted
    /// role found; fails closed on any error.
    pub async fn verify_access_token(&self, token: &str) -> Result<ConsoleIdentity, IamError> {
        let header = jsonwebtoken::decode_header(token)?;

        let jwks = self.current_jwks().await?;

        // Build validation config.
        let mut validation = Validation::new(header.alg);
        if !self.config.audiences.is_empty() {
            validation.set_audience(&self.config.audiences);
        } else {
            validation.validate_aud = false;
        }

        // Find the right JWK — prefer the one matching the `kid` header.
        let decoding_key = find_decoding_key(&jwks, header.kid.as_deref())?;

        let data: TokenData<AccessTokenClaims> =
            jsonwebtoken::decode(token, &decoding_key, &validation)?;

        let claims = data.claims;

        if claims.tenant_id.is_empty() {
            return Err(IamError::MissingTenant);
        }

        let role = extract_admitted_role(&claims).ok_or(IamError::RoleDenied)?;

        Ok(ConsoleIdentity {
            subject: claims.sub,
            tenant_id: claims.tenant_id,
            role,
        })
    }
}

/// Locate a [`DecodingKey`] in `jwks`, preferring the key whose `kid`
/// matches `want_kid` when supplied.
fn find_decoding_key(jwks: &JwkSet, want_kid: Option<&str>) -> Result<DecodingKey, IamError> {
    let candidates = if let Some(kid) = want_kid {
        let matched: Vec<_> = jwks
            .keys
            .iter()
            .filter(|k| k.common.key_id.as_deref() == Some(kid))
            .collect();
        if matched.is_empty() {
            jwks.keys.iter().collect()
        } else {
            matched
        }
    } else {
        jwks.keys.iter().collect()
    };

    for jwk in &candidates {
        if let Ok(key) = DecodingKey::from_jwk(jwk) {
            return Ok(key);
        }
    }
    Err(IamError::KeyNotFound)
}

/// Return the first `ConsoleRole` present in the token's `role` / `roles`
/// claims, or `None` if no admitted role is found.
fn extract_admitted_role(claims: &AccessTokenClaims) -> Option<ConsoleRole> {
    // Check single `role` field first.
    if let Some(r) = &claims.role
        && let Some(role) = ConsoleRole::from_str(r)
    {
        return Some(role);
    }
    // Then check the `roles` list.
    for r in &claims.roles {
        if let Some(role) = ConsoleRole::from_str(r) {
            return Some(role);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// AuthzTransport — axum middleware layer
// ---------------------------------------------------------------------------

/// Axum middleware state for JWT-based console authentication.
///
/// Wraps [`IamClient`] and is installed via
/// `axum::middleware::from_fn_with_state`. Successful verification injects
/// [`ConsoleIdentity`] into the request extensions for downstream handlers.
#[derive(Clone)]
pub struct AuthzTransport {
    pub iam: Arc<IamClient>,
}

impl AuthzTransport {
    pub fn new(iam: Arc<IamClient>) -> Self {
        Self { iam }
    }
}

/// Axum middleware function for the [`AuthzTransport`] layer.
///
/// Extracts the Bearer token from the `Authorization` header, calls
/// [`IamClient::verify_access_token`], and on success injects
/// [`ConsoleIdentity`] into the request extensions. Fails closed: any
/// missing token, verification error, or role denial returns HTTP 401.
pub async fn require_console_auth(
    State(transport): State<AuthzTransport>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = extract_bearer_token(&headers)
        .ok_or_else(|| ApiError::Unauthorized("authorization required".into()))?;

    let identity = transport
        .iam
        .verify_access_token(token)
        .await
        .map_err(|e| ApiError::Unauthorized(e.to_string()))?;

    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
}

/// Extract a Bearer token from an `Authorization` header.
///
/// Returns `None` if the header is absent, malformed, or uses a non-Bearer
/// scheme. Delegates prefix stripping to [`crate::auth::strip_bearer_prefix`].
pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = crate::auth::strip_bearer_prefix(value)?.trim();
    if token.is_empty() { None } else { Some(token) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn console_role_roundtrip() {
        assert_eq!(
            ConsoleRole::from_str("operator"),
            Some(ConsoleRole::Operator)
        );
        assert_eq!(
            ConsoleRole::from_str("tenant-admin"),
            Some(ConsoleRole::TenantAdmin)
        );
        assert_eq!(ConsoleRole::from_str("viewer"), None);
        assert_eq!(ConsoleRole::Operator.to_string(), "operator");
        assert_eq!(ConsoleRole::TenantAdmin.to_string(), "tenant-admin");
    }

    #[test]
    fn extract_admitted_role_prefers_single_role_field() {
        let claims = AccessTokenClaims {
            sub: "u1".into(),
            tenant_id: "t1".into(),
            role: Some("operator".into()),
            roles: vec!["viewer".into()],
        };
        assert_eq!(extract_admitted_role(&claims), Some(ConsoleRole::Operator));
    }

    #[test]
    fn extract_admitted_role_falls_back_to_roles_list() {
        let claims = AccessTokenClaims {
            sub: "u1".into(),
            tenant_id: "t1".into(),
            role: None,
            roles: vec!["viewer".into(), "tenant-admin".into()],
        };
        assert_eq!(
            extract_admitted_role(&claims),
            Some(ConsoleRole::TenantAdmin)
        );
    }

    #[test]
    fn extract_admitted_role_returns_none_for_denied_roles() {
        let claims = AccessTokenClaims {
            sub: "u1".into(),
            tenant_id: "t1".into(),
            role: Some("viewer".into()),
            roles: vec!["member".into()],
        };
        assert_eq!(extract_admitted_role(&claims), None);
    }

    #[test]
    fn extract_bearer_token_accepts_canonical_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer eyJhbGciOiJSUzI1NiJ9.test.sig"),
        );
        assert_eq!(
            extract_bearer_token(&headers),
            Some("eyJhbGciOiJSUzI1NiJ9.test.sig"),
        );
    }

    #[test]
    fn extract_bearer_token_returns_none_when_absent() {
        assert_eq!(extract_bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn extract_bearer_token_returns_none_for_basic_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn jwks_config_default_cache_ttl() {
        let cfg = JwksConfig {
            jwks_url: "https://example.com/.well-known/jwks.json".into(),
            ..Default::default()
        };
        assert_eq!(cfg.cache_ttl_secs, 3600);
        assert!(cfg.audiences.is_empty());
    }
}
