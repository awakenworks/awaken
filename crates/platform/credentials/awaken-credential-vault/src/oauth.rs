// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: This source implements the repository-controlled credential lifecycle product boundary
// @formal reason: and is classified against its product requirement; this link is not a formal-proof
// @formal reason: claim.
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
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use awaken_time::mono::{self, Instant};

use awaken_agent_contract::RedactedString;
use awaken_sync::LeafLock;

use crate::{CredentialError, CredentialSourceId};

/// Product safety-window TTL for a minted OAuth token. This is execution
/// behavior, not ambient operator configuration: a provider that needs a
/// different lifetime must persist that fact in its credential contract rather
/// than changing every credential through one process variable.
const DEFAULT_OAUTH_CACHE_TTL_SECS: u64 = 300;

/// The effective cache TTL owned by the credential bounded context.
fn oauth_cache_ttl() -> Duration {
    Duration::from_secs(DEFAULT_OAUTH_CACHE_TTL_SECS)
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
    cached: LeafLock<Option<(RedactedString, Instant)>>,
}

impl<T: TokenSource> CachingTokenSource<T> {
    /// Wrap `inner`, serving a minted token for up to `ttl` before refreshing.
    pub fn new(inner: T, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cached: LeafLock::new(None),
        }
    }

    /// The provider refused `presented`. Drop the cached entry only while it
    /// still holds exactly that token: the next call mints through the helper,
    /// and a token minted after the refused one is never discarded by a late
    /// report. Returns whether an entry was retired.
    pub fn retire_refused(&self, presented: &RedactedString) -> bool {
        let mut cached = self.cached.lock();
        let holds_refused = cached
            .as_ref()
            .is_some_and(|(token, _)| token.expose_secret() == presented.expose_secret());
        if holds_refused {
            *cached = None;
        }
        holds_refused
    }
}

#[async_trait::async_trait]
impl<T: TokenSource> TokenSource for CachingTokenSource<T> {
    async fn access_token(&self) -> Result<RedactedString, CredentialError> {
        if let Some((token, minted)) = self.cached.lock().as_ref()
            && minted.elapsed() < self.ttl
        {
            return Ok(token.clone());
        }
        let fresh = self.inner.access_token().await?;
        *self.cached.lock() = Some((fresh.clone(), mono::now()));
        Ok(fresh)
    }
}

/// Process-wide cache of per-source [`CachingTokenSource`]s, so the stateless
/// `materialize` free function still reuses one minted token across runs (keyed by
/// credential-source id). Built lazily on first use.
type OauthSources = HashMap<String, Arc<CachingTokenSource<CommandTokenSource>>>;
static OAUTH_SOURCES: OnceLock<LeafLock<OauthSources>> = OnceLock::new();

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
            .get_or_init(|| LeafLock::new(HashMap::new()))
            .lock();
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

/// The provider refused `presented`, an access token minted for `source_id`.
/// Retire the per-source cache entry if it still serves that token, so the
/// next materialization runs the helper instead of re-presenting a revoked or
/// rotated token for the rest of the TTL. An absent entry has nothing cached.
pub(crate) fn retire_refused_oauth_token(
    source_id: &CredentialSourceId,
    presented: &RedactedString,
) -> bool {
    let Some(sources) = OAUTH_SOURCES.get() else {
        return false;
    };
    let source = sources.lock().get(&source_id.0).cloned();
    source.is_some_and(|source| source.retire_refused(presented))
}

#[cfg(all(test, windows))]
pub(crate) fn test_stdout_command(output: &str) -> Vec<String> {
    vec![
        "cmd.exe".into(),
        "/D".into(),
        "/C".into(),
        format!("echo|set /p={output} & exit /b 0"),
    ]
}

#[cfg(all(test, not(windows)))]
pub(crate) fn test_stdout_command(output: &str) -> Vec<String> {
    vec!["printf".into(), output.into()]
}

#[cfg(all(test, windows))]
fn test_empty_command() -> Vec<String> {
    vec![
        "cmd.exe".into(),
        "/D".into(),
        "/C".into(),
        "exit /b 0".into(),
    ]
}

#[cfg(all(test, not(windows)))]
fn test_empty_command() -> Vec<String> {
    vec!["printf".into(), "  \n\t  ".into()]
}

#[cfg(all(test, windows))]
fn test_failing_command() -> Vec<String> {
    vec![
        "cmd.exe".into(),
        "/D".into(),
        "/S".into(),
        "/C".into(),
        "echo reauth-required 1>&2 & exit /b 3".into(),
    ]
}

#[cfg(all(test, not(windows)))]
fn test_failing_command() -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "echo reauth-required >&2; exit 3".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_source(command: Vec<String>) -> CommandTokenSource {
        let (program, args) = command.split_first().unwrap();
        CommandTokenSource::new(program.clone(), args.to_vec())
    }

    #[tokio::test]
    async fn command_stdout_becomes_the_token() {
        // A hermetic stand-in for the OAuth helper: `printf` emits a fixed token.
        let source = command_source(test_stdout_command("ya29.test-token"));
        let token = source.access_token().await.unwrap();
        assert_eq!(token.expose_secret(), "ya29.test-token");
    }

    #[tokio::test]
    async fn a_failing_helper_is_an_oauth_error() {
        let source = command_source(test_failing_command());
        assert!(matches!(
            source.access_token().await,
            Err(CredentialError::OAuth(_))
        ));
    }

    #[tokio::test]
    async fn a_nonzero_exit_surfaces_the_helper_stderr() {
        // A helper that fails and explains itself on stderr: the diagnostic must ride
        // out on the OAuth error so an operator sees *why* the refresh failed.
        let source = command_source(test_failing_command());
        assert!(matches!(
            source.access_token().await,
            Err(CredentialError::OAuth(msg))
                if msg.contains("reauth-required") && msg.contains("exited with")
        ));
    }

    #[tokio::test]
    async fn an_empty_token_is_rejected() {
        // A helper that "succeeds" but emits only whitespace refreshed nothing.
        let source = command_source(test_empty_command());
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

    /// A `TokenSource` that mints a distinct token per refresh (`tok-1`,
    /// `tok-2`, ...), as a real helper does once its grant is rotated.
    struct MintingSource {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl TokenSource for MintingSource {
        async fn access_token(&self) -> Result<RedactedString, CredentialError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(RedactedString::new(format!("tok-{call}")))
        }
    }

    fn minting_cache() -> CachingTokenSource<MintingSource> {
        CachingTokenSource::new(
            MintingSource {
                calls: AtomicUsize::new(0),
            },
            Duration::from_secs(300),
        )
    }

    /// Refusal decision table for the token cache. Causes: C1 an entry is
    /// cached; C2 the refused token is the cached one. Effects: E1 the entry
    /// is retired and the next call mints a new token; E2 the entry (and its
    /// TTL) is kept and the next call is served from it.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | R1 | Y | Y | E1 |
    /// | R2 | Y | N | E2 (a late refusal of an older token keeps a newer one) |
    /// | R3 | N | - | nothing retired; the next call mints as usual |
    #[tokio::test]
    async fn a_refused_cached_token_is_retired_and_the_next_call_mints_a_new_one() {
        // R1
        let caching = minting_cache();
        let first = caching.access_token().await.unwrap();
        assert_eq!(first.expose_secret(), "tok-1");
        assert!(caching.retire_refused(&first), "R1 retired");
        let second = caching.access_token().await.unwrap();
        assert_eq!(second.expose_secret(), "tok-2", "R1 a new token is minted");
        assert_eq!(caching.inner.calls.load(Ordering::SeqCst), 2);
        // The refused token is presented at most once: it is never served again.
        assert_eq!(
            caching.access_token().await.unwrap().expose_secret(),
            "tok-2"
        );
        assert_eq!(caching.inner.calls.load(Ordering::SeqCst), 2);

        // R2: the refusal of tok-1 arrives after tok-2 replaced it.
        assert!(
            !caching.retire_refused(&first),
            "R2 a stale refusal retires nothing"
        );
        assert_eq!(
            caching.access_token().await.unwrap().expose_secret(),
            "tok-2"
        );
        assert_eq!(
            caching.inner.calls.load(Ordering::SeqCst),
            2,
            "R2 the newer entry is kept"
        );

        // R3
        let empty = minting_cache();
        assert!(!empty.retire_refused(&RedactedString::new("tok-1")), "R3");
        assert_eq!(empty.access_token().await.unwrap().expose_secret(), "tok-1");
    }

    /// The process-wide per-source cache retires only the entry of the source
    /// that served the refused token, and only while it still holds it.
    #[tokio::test]
    async fn oauth_refusal_retires_only_the_serving_source_entry() {
        let id = CredentialSourceId("cred:ws:oauth-refusal-retire".into());
        let other = CredentialSourceId("cred:ws:oauth-refusal-other".into());
        let unknown = CredentialSourceId("cred:ws:oauth-refusal-unknown".into());
        let token = oauth_access_token(&id, &test_stdout_command("tok-refused"))
            .await
            .unwrap();
        let other_token = oauth_access_token(&other, &test_stdout_command("tok-refused"))
            .await
            .unwrap();
        assert!(
            !retire_refused_oauth_token(&unknown, &token),
            "no entry, nothing retired"
        );
        assert!(
            !retire_refused_oauth_token(&id, &RedactedString::new("tok-other")),
            "a token this entry does not hold retires nothing"
        );
        assert!(
            retire_refused_oauth_token(&id, &token),
            "the serving entry is retired"
        );
        assert!(
            !retire_refused_oauth_token(&id, &token),
            "retired at most once"
        );
        assert!(
            retire_refused_oauth_token(&other, &other_token),
            "another source's entry was untouched"
        );
    }

    #[tokio::test]
    async fn oauth_access_token_runs_the_command_and_returns_its_stdout() {
        let id = CredentialSourceId("cred:ws:oauth-test-1".into());
        let token = oauth_access_token(&id, &test_stdout_command("ya29.abc"))
            .await
            .unwrap();
        assert_eq!(token.expose_secret(), "ya29.abc");
    }

    #[tokio::test]
    async fn oauth_access_token_reuses_its_per_source_cache_across_calls() {
        // The process-wide per-source cache means the *first* command minted for an
        // id wins for the TTL: a second call with a different command for the same id
        // still returns the originally minted token (the entry is not rebuilt).
        let id = CredentialSourceId("cred:ws:oauth-cache-reuse".into());
        let first = oauth_access_token(&id, &test_stdout_command("tok-first"))
            .await
            .unwrap();
        assert_eq!(first.expose_secret(), "tok-first");
        let second = oauth_access_token(&id, &test_stdout_command("tok-second"))
            .await
            .unwrap();
        // Served from the cached CachingTokenSource, so still the first token.
        assert_eq!(second.expose_secret(), "tok-first");
    }

    #[tokio::test]
    async fn an_empty_oauth_command_is_an_error() {
        let id = CredentialSourceId("cred:ws:oauth-empty".into());
        assert!(matches!(
            oauth_access_token(&id, &[]).await,
            Err(CredentialError::OAuth(_))
        ));
    }

    /// Cause/effect rationale: OAuth source kind is fixed and no per-source
    /// lifetime exists in the persisted contract, therefore the only legal rule
    /// is R1 = any process environment -> the bounded-context safety TTL.
    #[test]
    fn oauth_cache_ttl_is_not_ambient_configuration() {
        assert_eq!(
            oauth_cache_ttl(),
            Duration::from_secs(DEFAULT_OAUTH_CACHE_TTL_SECS)
        );
    }
}
