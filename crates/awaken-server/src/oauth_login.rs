//! In-process IAM OAuth login flow: StartLogin → IdP callback → EstablishSession.
//!
//! Routes (no admin auth required — they ARE the auth):
//! - `GET /v1/auth/login?return_to=...`  – StartLogin: redirect to IdP
//! - `GET /v1/auth/callback?code=...&state=...` – Callback + EstablishSession
//! - `GET /v1/auth/me`  – returns current session subject (accepts Bearer or cookie)
//! - `POST /v1/auth/logout` – clears session cookie

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{AdminModuleState, OAuthConfig};
use crate::routes::ApiError;

// PKCE / state nonce TTL: 10 minutes.
const NONCE_TTL: Duration = Duration::from_secs(600);
// Session TTL: 24 hours.
const SESSION_TTL: Duration = Duration::from_secs(86_400);

pub const SESSION_COOKIE_NAME: &str = "awaken-session";

// ---------------------------------------------------------------------------
// In-process stores
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PendingLogin {
    return_to: String,
    created_at: Instant,
}

#[derive(Clone, Serialize)]
pub struct SessionData {
    pub subject: String,
    #[serde(skip)]
    created_at: Instant,
}

/// Shared in-memory store for OAuth nonces and active sessions.
#[derive(Default)]
pub struct InProcessSessionStore {
    pending: Mutex<HashMap<String, PendingLogin>>,
    sessions: Mutex<HashMap<String, SessionData>>,
}

impl InProcessSessionStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn insert_pending(&self, state: String, return_to: String) {
        self.pending.lock().insert(
            state,
            PendingLogin {
                return_to,
                created_at: Instant::now(),
            },
        );
    }

    fn consume_pending(&self, state: &str) -> Option<String> {
        let mut lock = self.pending.lock();
        let entry = lock.remove(state)?;
        if entry.created_at.elapsed() > NONCE_TTL {
            return None;
        }
        Some(entry.return_to)
    }

    /// Create a new session and return the session id.
    pub fn establish_session(&self, subject: String) -> String {
        let session_id = Uuid::new_v4().to_string();
        self.sessions.lock().insert(
            session_id.clone(),
            SessionData {
                subject,
                created_at: Instant::now(),
            },
        );
        session_id
    }

    pub fn lookup_session(&self, session_id: &str) -> Option<SessionData> {
        let lock = self.sessions.lock();
        let data = lock.get(session_id)?.clone();
        if data.created_at.elapsed() > SESSION_TTL {
            return None;
        }
        Some(data)
    }

    pub fn remove_session(&self, session_id: &str) {
        self.sessions.lock().remove(session_id);
    }
}

// ---------------------------------------------------------------------------
// Route state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OAuthRoutesState {
    pub admin: AdminModuleState,
    pub oauth: Arc<OAuthConfig>,
    pub sessions: Arc<InProcessSessionStore>,
}

pub fn oauth_routes() -> Router<OAuthRoutesState> {
    Router::new()
        .route("/v1/auth/capabilities", get(auth_capabilities))
        .route("/v1/auth/login", get(start_login))
        .route("/v1/auth/callback", get(callback))
        .route("/v1/auth/me", get(session_me))
        .route("/v1/auth/logout", post(logout))
}

// ---------------------------------------------------------------------------
// Query parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LoginParams {
    #[serde(default)]
    return_to: String,
}

#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /v1/auth/capabilities — public, no auth required.
/// Returns whether the OAuth login flow is active and the login entry-point.
async fn auth_capabilities(_state: State<OAuthRoutesState>) -> Response {
    Json(serde_json::json!({
        "oauth_enabled": true,
        "login_url": "/v1/auth/login",
    }))
    .into_response()
}

/// StartLogin: generate a random state nonce, store it, redirect to IdP.
#[tracing::instrument(skip(state))]
async fn start_login(
    State(state): State<OAuthRoutesState>,
    Query(params): Query<LoginParams>,
) -> Response {
    let oauth = &state.oauth;

    let nonce = Uuid::new_v4().to_string();
    let return_to = if params.return_to.is_empty() {
        "/".to_string()
    } else {
        params.return_to
    };
    state.sessions.insert_pending(nonce.clone(), return_to);

    let mut auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}",
        oauth.authorization_url,
        urlencoding_encode(&oauth.client_id),
        urlencoding_encode(&oauth.redirect_uri),
        urlencoding_encode(&nonce),
    );
    if !oauth.scopes.is_empty() {
        let scope = oauth.scopes.join(" ");
        auth_url.push_str("&scope=");
        auth_url.push_str(&urlencoding_encode(&scope));
    }

    redirect_response(&auth_url, StatusCode::FOUND)
}

/// Callback: validate state, exchange code for IdP token, EstablishSession.
#[tracing::instrument(skip(state))]
async fn callback(
    State(state): State<OAuthRoutesState>,
    Query(params): Query<CallbackParams>,
) -> Response {
    if let Some(error) = &params.error {
        let desc = params.error_description.as_deref().unwrap_or("");
        tracing::warn!(error, desc, "IdP returned an error on callback");
        return error_redirect(&format!("IdP error: {error} — {desc}"));
    }

    let code = match &params.code {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return error_redirect("missing authorization code"),
    };
    let nonce = match &params.state {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return error_redirect("missing state parameter"),
    };

    let return_to = match state.sessions.consume_pending(&nonce) {
        Some(r) => r,
        None => return error_redirect("invalid or expired state"),
    };

    let subject = match exchange_code(&state.oauth, &code).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "OAuth token exchange failed");
            return error_redirect(&format!("token exchange failed: {err}"));
        }
    };

    let session_id = state.sessions.establish_session(subject);
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_COOKIE_NAME,
        session_id,
        SESSION_TTL.as_secs(),
    );

    // The console may run on a different origin in dev, so we pass the
    // session id as a URL fragment so it can bootstrap itself as a bearer
    // token too.  The fragment is never sent to the server, so it is safe.
    let dest = format!("{return_to}#access_token={session_id}");

    let mut response = redirect_response(&dest, StatusCode::FOUND);
    if let Ok(val) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, val);
    }
    response
}

/// GET /v1/auth/me — returns the current session subject.
async fn session_me(State(state): State<OAuthRoutesState>, headers: HeaderMap) -> Response {
    let session_id = resolve_session_id(&headers);
    match session_id.and_then(|id| state.sessions.lookup_session(&id)) {
        Some(data) => Json(data).into_response(),
        None => ApiError::Unauthorized("no active session".into()).into_response(),
    }
}

/// POST /v1/auth/logout — clears the session cookie.
async fn logout(State(state): State<OAuthRoutesState>, headers: HeaderMap) -> Response {
    if let Some(session_id) = resolve_session_id(&headers) {
        state.sessions.remove_session(&session_id);
    }
    let expire = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        SESSION_COOKIE_NAME
    );
    let mut response = (StatusCode::NO_CONTENT).into_response();
    if let Ok(val) = HeaderValue::from_str(&expire) {
        response.headers_mut().insert(header::SET_COOKIE, val);
    }
    response
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a session id from the request — checks Bearer token first, then
/// the `awaken-session` cookie.
pub fn resolve_session_id(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get(header::AUTHORIZATION)
        && let Ok(val) = auth.to_str()
        && let Some(token) = crate::auth::strip_bearer_prefix(val)
    {
        let t = token.trim().to_owned();
        if !t.is_empty() {
            return Some(t);
        }
    }
    // Fall back to session cookie.
    if let Some(cookie_hdr) = headers.get(header::COOKIE)
        && let Ok(cookie_str) = cookie_hdr.to_str()
    {
        for part in cookie_str.split(';') {
            let part = part.trim();
            if let Some(val) = part.strip_prefix(SESSION_COOKIE_NAME)
                && let Some(val) = val.strip_prefix('=')
            {
                let v = val.trim().to_owned();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Exchange an authorization code for an IdP access token.
/// Returns the `sub` / login identity extracted from the token response.
async fn exchange_code(cfg: &OAuthConfig, code: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let secret = cfg.client_secret.expose_secret().to_owned();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", cfg.redirect_uri.as_str()),
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", secret.as_str()),
    ];
    let resp = client
        .post(&cfg.token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token endpoint returned {status}: {body}"));
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    // Try to extract a subject identity from the token response.
    // Providers vary: GitHub uses "login", OIDC uses "sub", etc.
    let subject = body
        .get("login")
        .or_else(|| body.get("sub"))
        .or_else(|| body.get("email"))
        .or_else(|| body.get("user_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_owned();

    Ok(subject)
}

fn redirect_response(url: &str, status: StatusCode) -> Response {
    let mut response = (status).into_response();
    if let Ok(val) = HeaderValue::from_str(url) {
        response.headers_mut().insert(header::LOCATION, val);
    }
    response
}

fn error_redirect(reason: &str) -> Response {
    let encoded = urlencoding_encode(reason);
    redirect_response(&format!("/#oauth_error={encoded}"), StatusCode::FOUND)
}

fn urlencoding_encode(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c]
            } else {
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
                bytes
                    .iter()
                    .flat_map(|b| format!("%{b:02X}").chars().collect::<Vec<_>>())
                    .collect()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_store_roundtrip() {
        let store = InProcessSessionStore::new();
        let id = store.establish_session("alice".to_string());
        let data = store.lookup_session(&id).expect("session should exist");
        assert_eq!(data.subject, "alice");
        store.remove_session(&id);
        assert!(store.lookup_session(&id).is_none());
    }

    #[test]
    fn pending_login_consumed_once() {
        let store = InProcessSessionStore::new();
        store.insert_pending("state-abc".to_string(), "/dashboard".to_string());
        let r = store.consume_pending("state-abc");
        assert_eq!(r.as_deref(), Some("/dashboard"));
        // Second consume returns None.
        assert!(store.consume_pending("state-abc").is_none());
    }

    #[test]
    fn pending_login_unknown_state_returns_none() {
        let store = InProcessSessionStore::new();
        assert!(store.consume_pending("nonexistent").is_none());
    }

    #[test]
    fn resolve_session_id_prefers_bearer_over_cookie() {
        use axum::http::{HeaderMap, HeaderValue, header};
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bearer-token"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("awaken-session=cookie-token"),
        );
        assert_eq!(
            resolve_session_id(&headers).as_deref(),
            Some("bearer-token")
        );
    }

    #[test]
    fn resolve_session_id_falls_back_to_cookie() {
        use axum::http::{HeaderMap, HeaderValue, header};
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("awaken-session=my-session-id"),
        );
        assert_eq!(
            resolve_session_id(&headers).as_deref(),
            Some("my-session-id")
        );
    }

    #[test]
    fn resolve_session_id_returns_none_when_absent() {
        let headers = HeaderMap::new();
        assert!(resolve_session_id(&headers).is_none());
    }

    #[test]
    fn urlencoding_encode_basic() {
        assert_eq!(urlencoding_encode("hello world"), "hello%20world");
        assert_eq!(urlencoding_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(urlencoding_encode("a=b&c=d"), "a%3Db%26c%3Dd");
    }
}
