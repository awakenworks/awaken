//! OAuth access-token acquisition (ADR-0043 Phase 3, feature `oauth-command`).
//!
//! An OAuth-backed provider credential's secret is a **short-lived Bearer token**
//! that must be *refreshed*, not stored once. This module owns that refresh. The
//! materialized token is a [`RedactedString`] used at the injection seam exactly
//! like any other resolved secret (D6/D9) — the difference is only in how it is
//! obtained.
//!
//! [`CommandTokenSource`] delegates the refresh to an external helper whose stdout
//! is the access token — e.g. `gcloud auth print-access-token`, which holds the
//! long-lived Google grant and mints a fresh cloud-platform-scoped token on demand.
//! This is the faithful integration for gcloud-managed Google credentials (used to
//! call Gemini on Vertex AI): the refresh token never enters this process; each
//! call yields a fresh, expiring access token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use awaken_agent_contract::RedactedString;

use crate::{CredentialError, CredentialSourceId};

/// Default safety-window TTL for a minted OAuth token: it is reused for this long
/// before a refresh, bounding helper spawns (real access tokens live ~1h) without
/// risking a stale token. Overridable via `AWAKEN_OAUTH_TOKEN_TTL_SECS` so a
/// short-lived-token provider can tighten it.
const DEFAULT_OAUTH_CACHE_TTL_SECS: u64 = 300;

/// The effective cache TTL, from `AWAKEN_OAUTH_TOKEN_TTL_SECS` (seconds) or the
/// default. A non-numeric or empty value falls back to the default.
fn oauth_cache_ttl() -> Duration {
    parse_ttl(std::env::var("AWAKEN_OAUTH_TOKEN_TTL_SECS").ok().as_deref())
}

/// Pure TTL parse: `Some("<secs>")` → that many seconds; anything else → default.
fn parse_ttl(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_OAUTH_CACHE_TTL_SECS);
    Duration::from_secs(secs)
}

/// A source of fresh OAuth access tokens. Each call performs (or delegates) a
/// refresh, so the returned token is current.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn access_token(&self) -> Result<RedactedString, CredentialError>;
}

/// Refresh by running an external command whose trimmed stdout is the access
/// token. The command is the OAuth helper that holds the long-lived grant.
pub struct CommandTokenSource {
    program: String,
    args: Vec<String>,
}

impl CommandTokenSource {
    /// A refresher that runs `program args...` and reads the token from stdout.
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The `gcloud auth print-access-token` refresher — a Google OAuth2 access
    /// token (cloud-platform scope) for the active gcloud account.
    #[must_use]
    pub fn gcloud() -> Self {
        Self::new("gcloud", ["auth", "print-access-token"])
    }
}

#[async_trait::async_trait]
impl TokenSource for CommandTokenSource {
    async fn access_token(&self) -> Result<RedactedString, CredentialError> {
        let output = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .output()
            .await
            .map_err(|e| CredentialError::OAuth(format!("spawn `{}`: {e}", self.program)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CredentialError::OAuth(format!(
                "`{}` exited with {}: {}",
                self.program,
                output.status,
                stderr.trim()
            )));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(CredentialError::OAuth(
                "refresh returned an empty token".into(),
            ));
        }
        Ok(RedactedString::new(token))
    }
}

/// Caches an inner [`TokenSource`]'s token for a safety-window TTL, so repeated
/// materializations reuse one minted token instead of re-spawning the helper. A
/// transparent decorator: on a miss (empty or expired) it refreshes through the
/// inner source and re-stamps the cache.
pub struct CachingTokenSource<T> {
    inner: T,
    ttl: Duration,
    cached: Mutex<Option<(RedactedString, Instant)>>,
}

impl<T: TokenSource> CachingTokenSource<T> {
    /// Wrap `inner`, serving a minted token for up to `ttl` before refreshing.
    pub fn new(inner: T, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cached: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl<T: TokenSource> TokenSource for CachingTokenSource<T> {
    async fn access_token(&self) -> Result<RedactedString, CredentialError> {
        if let Some((token, minted)) = self.cached.lock().unwrap().as_ref() {
            if minted.elapsed() < self.ttl {
                return Ok(token.clone());
            }
        }
        let fresh = self.inner.access_token().await?;
        *self.cached.lock().unwrap() = Some((fresh.clone(), Instant::now()));
        Ok(fresh)
    }
}

/// Process-wide cache of per-source [`CachingTokenSource`]s, so the stateless
/// `materialize` free function still reuses one minted token across runs (keyed by
/// credential-source id). Built lazily on first use.
type OauthSources = HashMap<String, Arc<CachingTokenSource<CommandTokenSource>>>;
static OAUTH_SOURCES: OnceLock<Mutex<OauthSources>> = OnceLock::new();

/// Mint (or reuse a cached) OAuth access token for `source_id` by running
/// `command` (`[program, args…]`). The per-source [`CachingTokenSource`] persists
/// across calls, so a hot loop refreshes at most once per [`OAUTH_CACHE_TTL`].
pub(crate) async fn oauth_access_token(
    source_id: &CredentialSourceId,
    command: &[String],
) -> Result<RedactedString, CredentialError> {
    let (program, args) = command.split_first().ok_or_else(|| {
        CredentialError::OAuth(format!("empty oauth_command for {}", source_id.0))
    })?;
    let cached = {
        let mut sources = OAUTH_SOURCES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        sources
            .entry(source_id.0.clone())
            .or_insert_with(|| {
                Arc::new(CachingTokenSource::new(
                    CommandTokenSource::new(program.clone(), args.to_vec()),
                    oauth_cache_ttl(),
                ))
            })
            .clone()
    };
    cached.access_token().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_stdout_becomes_the_token() {
        // A hermetic stand-in for the OAuth helper: `printf` emits a fixed token.
        let source = CommandTokenSource::new("printf", ["ya29.test-token"]);
        let token = source.access_token().await.unwrap();
        assert_eq!(token.expose_secret(), "ya29.test-token");
    }

    #[tokio::test]
    async fn a_failing_helper_is_an_oauth_error() {
        let source = CommandTokenSource::new("false", Vec::<String>::new());
        assert!(matches!(
            source.access_token().await,
            Err(CredentialError::OAuth(_))
        ));
    }

    #[tokio::test]
    async fn an_empty_token_is_rejected() {
        // A helper that "succeeds" but emits only whitespace refreshed nothing.
        let source = CommandTokenSource::new("printf", ["  \\n\\t  "]);
        assert!(matches!(
            source.access_token().await,
            Err(CredentialError::OAuth(msg)) if msg == "refresh returned an empty token"
        ));
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let source =
            CommandTokenSource::new("awaken-no-such-oauth-helper-7f3a", Vec::<String>::new());
        assert!(matches!(
            source.access_token().await,
            Err(CredentialError::OAuth(msg)) if msg.starts_with("spawn `")
        ));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `TokenSource` that counts refreshes, so a test can prove the cache
    /// serves without re-invoking the inner helper.
    struct CountingSource {
        calls: AtomicUsize,
        token: &'static str,
    }
    #[async_trait::async_trait]
    impl TokenSource for CountingSource {
        async fn access_token(&self) -> Result<RedactedString, CredentialError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(RedactedString::new(self.token))
        }
    }

    #[tokio::test]
    async fn caching_source_refreshes_once_within_ttl() {
        let caching = CachingTokenSource::new(
            CountingSource {
                calls: AtomicUsize::new(0),
                token: "tok",
            },
            Duration::from_secs(300),
        );
        assert_eq!(caching.access_token().await.unwrap().expose_secret(), "tok");
        assert_eq!(caching.access_token().await.unwrap().expose_secret(), "tok");
        // The second call served the cache — the helper ran exactly once.
        assert_eq!(caching.inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn caching_source_refreshes_again_after_ttl_expiry() {
        let caching = CachingTokenSource::new(
            CountingSource {
                calls: AtomicUsize::new(0),
                token: "tok",
            },
            Duration::from_millis(0),
        );
        caching.access_token().await.unwrap();
        caching.access_token().await.unwrap();
        // A zero TTL expires immediately, so each call refreshes.
        assert_eq!(caching.inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn oauth_access_token_runs_the_command_and_returns_its_stdout() {
        let id = CredentialSourceId("cred:ws:oauth-test-1".into());
        let token = oauth_access_token(&id, &["printf".to_string(), "ya29.abc".to_string()])
            .await
            .unwrap();
        assert_eq!(token.expose_secret(), "ya29.abc");
    }

    #[tokio::test]
    async fn an_empty_oauth_command_is_an_error() {
        let id = CredentialSourceId("cred:ws:oauth-empty".into());
        assert!(matches!(
            oauth_access_token(&id, &[]).await,
            Err(CredentialError::OAuth(_))
        ));
    }

    #[test]
    fn ttl_parses_seconds_and_falls_back_to_the_default() {
        assert_eq!(parse_ttl(Some("30")), Duration::from_secs(30));
        assert_eq!(parse_ttl(Some("  90 ")), Duration::from_secs(90));
        // Non-numeric / empty / unset → the conservative default.
        assert_eq!(
            parse_ttl(Some("nope")),
            Duration::from_secs(DEFAULT_OAUTH_CACHE_TTL_SECS)
        );
        assert_eq!(
            parse_ttl(None),
            Duration::from_secs(DEFAULT_OAUTH_CACHE_TTL_SECS)
        );
    }
}
