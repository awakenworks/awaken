//! `CredentialBroker` — the single chokepoint for all provider credentials.
//!
//! ## Responsibilities
//! - Hold credential **material** (parsed at registration) keyed by provider id.
//! - Hand out **tokens** keyed by `(provider_id, scope)` via [`token_for`].
//! - **Cache** tokens until near expiry (`SAFETY_WINDOW` before).
//! - **Single-flight**: concurrent token requests for the same key share
//!   one mint operation rather than stampeding the upstream OAuth endpoint.
//!
//! ## Architecture
//! Two RwLocks:
//! - `materials`: `provider_id → (generation, CredentialMaterial)`. Updated on
//!   [`register`](AwakenCredentialBroker::register) and
//!   [`deregister`](AwakenCredentialBroker::deregister); read on every mint.
//!   The generation prevents in-flight mints from writing or returning tokens
//!   after credential rotation/removal.
//! - `cache`: `(provider_id, scope) → (generation, Token)`. Updated on each
//!   mint; read on every `token_for`. Cache hits are accepted only when the
//!   provider is still registered at the same generation.
//!
//! Single-flight is implemented with a per-key
//! `tokio::sync::OnceCell` pattern: the first task to find a stale cache
//! takes a `Mutex` guarded slot, mints, populates the cache, and releases;
//! waiters block on the same Mutex slot and read the freshly cached token
//! when they wake.
//!
//! [`token_for`]: CredentialBroker::token_for

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock as PlRwLock;
use tokio::sync::Mutex as AsyncMutex;

use super::error::CredentialError;
use super::material::CredentialMaterial;
use super::token::{IssuedToken, Token};

/// Refresh tokens this long before their stated expiry. Prevents handing
/// out a token that would expire mid-request. Google OAuth tokens are
/// nominally 3600s; 60s margin keeps us safely inside the window even for
/// short-validity tokens.
const SAFETY_WINDOW: Duration = Duration::from_secs(60);

/// HTTP client used by signers that need a network call (e.g. OAuth token
/// exchange). Connection-pooled; cheap to clone.
type HttpClient = reqwest::Client;

/// Bounded retry policy applied **inside the broker** when minting tokens.
///
/// The broker's job is to hand back a working token; transient credential
/// failures (network blip, 5xx from the OAuth endpoint) should not bubble
/// up into the inference layer's retry loop, where they would (a) get
/// mis-classified by `engine::executor::map_error`'s string fall-through,
/// (b) consume the inference retry budget meant for LLM-side errors, and
/// (c) waste user-visible latency on retries that the inference layer can
/// never resolve (no amount of LLM retry will mint an OAuth token).
///
/// **Permanent errors** (`InvalidMaterial`, `SigningFailed`,
/// `PermanentUpstream`, `NotConfigured`) are returned **immediately** —
/// `is_retryable() == false` short-circuits the loop on attempt 1.
///
/// **Transient errors** (`Network`, `TransientUpstream`) are retried up to
/// `max_attempts` times with exponential backoff bounded by `max`. The
/// sequence for the default policy is approximately
/// 100ms → 200ms → 400ms (~700ms total wall clock for 3 attempts).
///
/// This is a thin alias over [`crate::retry::BackoffPolicy`] — the broker
/// uses the shared exponential-backoff primitive rather than maintaining
/// its own loop. The alias preserves the existing `CredentialRetryPolicy`
/// name in tests and embedder code.
pub type CredentialRetryPolicy = crate::retry::BackoffPolicy;

/// Trait for credential lookups. Owning crates can swap in fakes for tests.
///
/// Both methods are on the trait — `register` so the runtime can install
/// material via a `dyn` reference without downcasting, and `token_for` so
/// the auth-resolver hook handed to genai is shape-stable across
/// implementations. Test doubles override `token_for` with a fixed return;
/// `register` can be a no-op for tests that bake material in elsewhere.
#[async_trait]
pub trait CredentialBroker: Send + Sync {
    /// Register or replace credential material for a provider id.
    /// Default implementation is a no-op (suitable for test doubles).
    fn register(&self, _provider_id: String, _material: CredentialMaterial) {}

    /// Forget a provider entirely. Default impl is no-op.
    fn deregister(&self, _provider_id: &str) {}

    /// Mint a fresh, return a cached, or block on a single-flight refresh
    /// for `(provider_id, scope)`.
    async fn token_for(
        &self,
        provider_id: &str,
        scope: &str,
    ) -> Result<IssuedToken, CredentialError>;
}

/// Cache key. Scope is part of the key because the same SA can mint
/// tokens with different scopes; their access_tokens are not
/// interchangeable.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    provider_id: String,
    scope: String,
}

/// Per-key single-flight slot. Distinct from the cached token so the
/// refresh path doesn't hold the cache lock while awaiting the network.
#[derive(Default)]
struct FlightSlot {
    /// Async mutex serialises mint operations for this key. Holding the
    /// guard across `await` points is fine because it's a tokio Mutex
    /// (not a parking_lot one).
    inflight: AsyncMutex<()>,
}

#[derive(Clone)]
struct MaterialEntry {
    generation: u64,
    material: Option<CredentialMaterial>,
}

struct CachedToken {
    generation: u64,
    token: Token,
}

pub struct AwakenCredentialBroker {
    materials: PlRwLock<HashMap<String, MaterialEntry>>,
    cache: PlRwLock<HashMap<CacheKey, CachedToken>>,
    /// Per-key in-flight mutex map. Entries live for the lifetime of the
    /// broker; the value is a small struct so the memory footprint stays
    /// bounded by the number of distinct (provider_id, scope) pairs in
    /// use, which is small in practice.
    flights: PlRwLock<HashMap<CacheKey, Arc<FlightSlot>>>,
    http: HttpClient,
    retry_policy: CredentialRetryPolicy,
}

impl Default for AwakenCredentialBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl AwakenCredentialBroker {
    /// Create a broker with a fresh internal HTTP client. Use
    /// [`with_http_client`](Self::with_http_client) to share a connection
    /// pool with other parts of the runtime.
    pub fn new() -> Self {
        Self::with_http_client(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client builds with default settings"),
        )
    }

    pub fn with_http_client(http: HttpClient) -> Self {
        Self {
            materials: PlRwLock::new(HashMap::new()),
            cache: PlRwLock::new(HashMap::new()),
            flights: PlRwLock::new(HashMap::new()),
            http,
            retry_policy: CredentialRetryPolicy::default(),
        }
    }

    /// Override the retry policy used when minting tokens. Useful in
    /// tests (set [`CredentialRetryPolicy::disabled`] to assert raw
    /// errors) and for tuning per-deployment.
    #[must_use]
    pub fn with_retry_policy(mut self, policy: CredentialRetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Whether this broker knows about `provider_id`. Useful for
    /// embedder-side assertions.
    pub fn is_registered(&self, provider_id: &str) -> bool {
        self.materials
            .read()
            .get(provider_id)
            .and_then(|entry| entry.material.as_ref())
            .is_some()
    }

    fn flight_slot(&self, key: &CacheKey) -> Arc<FlightSlot> {
        // Fast path: slot exists.
        if let Some(slot) = self.flights.read().get(key) {
            return Arc::clone(slot);
        }
        // Slow path: insert or take whatever the racer inserted.
        let mut flights = self.flights.write();
        Arc::clone(
            flights
                .entry(key.clone())
                .or_insert_with(|| Arc::new(FlightSlot::default())),
        )
    }

    fn material_snapshot(
        &self,
        provider_id: &str,
    ) -> Result<(u64, CredentialMaterial), CredentialError> {
        let materials = self.materials.read();
        let entry = materials
            .get(provider_id)
            .ok_or_else(|| CredentialError::NotConfigured(provider_id.to_owned()))?;
        let material = entry
            .material
            .clone()
            .ok_or_else(|| CredentialError::NotConfigured(provider_id.to_owned()))?;
        Ok((entry.generation, material))
    }

    fn generation_still_current(&self, provider_id: &str, generation: u64) -> bool {
        self.materials
            .read()
            .get(provider_id)
            .is_some_and(|entry| entry.generation == generation && entry.material.is_some())
    }

    /// Mint a token for a material snapshot. **No cache, no single-flight** —
    /// the caller (`token_for`) wraps this with both and validates the snapshot
    /// generation before returning.
    async fn mint_material(
        &self,
        scope: &str,
        material: CredentialMaterial,
    ) -> Result<Token, CredentialError> {
        // Dispatch via the Minter trait — no central match. Adding a new
        // cloud means a new Minter impl and a new CredentialMaterial
        // constructor, not editing the broker.
        material.minter().mint(scope, &self.http).await
    }
}

#[async_trait]
impl CredentialBroker for AwakenCredentialBroker {
    /// Register or replace credential material for a provider id.
    ///
    /// Replacing material **invalidates the cache** for all scopes of that
    /// provider — the next `token_for` will mint anew. This makes the
    /// admin "rotate the SA JSON" flow feel atomic from the runtime's
    /// perspective.
    fn register(&self, provider_id: String, material: CredentialMaterial) {
        {
            let mut materials = self.materials.write();
            let next_generation = materials
                .get(&provider_id)
                .map(|entry| entry.generation.saturating_add(1))
                .unwrap_or(1);
            materials.insert(
                provider_id.clone(),
                MaterialEntry {
                    generation: next_generation,
                    material: Some(material),
                },
            );
        }
        // Drop any cached tokens for this provider — material change means
        // they may have been signed by a key that's about to be revoked.
        let mut cache = self.cache.write();
        cache.retain(|key, _| key.provider_id != provider_id);
    }

    /// Forget a provider entirely. In-flight mints for this provider are
    /// generation-checked before cache write/return, so they are discarded if
    /// this deregistration wins the race.
    fn deregister(&self, provider_id: &str) {
        if let Some(entry) = self.materials.write().get_mut(provider_id) {
            entry.generation = entry.generation.saturating_add(1);
            entry.material = None;
        }
        self.cache
            .write()
            .retain(|key, _| key.provider_id != provider_id);
        // Drop flight slots so deregister-then-re-register does not reuse a
        // stale `Arc<FlightSlot>` (and so a long-lived broker doesn't
        // accumulate orphaned slot entries when providers churn).
        self.flights
            .write()
            .retain(|key, _| key.provider_id != provider_id);
    }

    async fn token_for(
        &self,
        provider_id: &str,
        scope: &str,
    ) -> Result<IssuedToken, CredentialError> {
        let key = CacheKey {
            provider_id: provider_id.to_owned(),
            scope: scope.to_owned(),
        };

        loop {
            let (generation, _) = self.material_snapshot(provider_id)?;

            // 1. Cache fast path. A cached token is valid only while the
            // provider remains registered at the same material generation.
            if let Some(cached) = self.cache.read().get(&key)
                && cached.generation == generation
                && !cached.token.is_near_expiry(SAFETY_WINDOW)
            {
                return Ok(IssuedToken::from_token(&cached.token));
            }

            // 2. Acquire the per-key single-flight slot.
            let slot = self.flight_slot(&key);
            let _guard = slot.inflight.lock().await;

            // 3. Re-read material and cache under the slot — a concurrent
            //    task may have populated it or an admin may have rotated
            //    credentials while we were waiting.
            let (generation, material) = match self.material_snapshot(provider_id) {
                Ok(snapshot) => snapshot,
                Err(err) => return Err(err),
            };
            if let Some(cached) = self.cache.read().get(&key)
                && cached.generation == generation
                && !cached.token.is_near_expiry(SAFETY_WINDOW)
            {
                return Ok(IssuedToken::from_token(&cached.token));
            }

            // 4. We are the elected refresher. Apply the bounded retry
            //    policy. If credentials are rotated/removed while minting,
            //    discard the result and loop against the current generation.
            let fresh = self
                .mint_with_retry(provider_id, scope, material.clone())
                .await?;
            if !self.generation_still_current(provider_id, generation) {
                tracing::debug!(
                    provider_id = %provider_id,
                    scope = %scope,
                    generation,
                    "credential broker: discarded token minted from stale material generation"
                );
                continue;
            }

            let issued = IssuedToken::from_token(&fresh);
            self.cache.write().insert(
                key.clone(),
                CachedToken {
                    generation,
                    token: fresh,
                },
            );
            return Ok(issued);
        }
    }
}

impl AwakenCredentialBroker {
    /// Wrap [`mint`](Self::mint) with the broker's retry policy.
    ///
    /// Defers to the shared [`crate::retry::with_backoff`] primitive — the
    /// broker holds no retry implementation of its own, just the
    /// retry-classification function (`CredentialError::is_retryable`) and
    /// a tracing hook for transient failures.
    async fn mint_with_retry(
        &self,
        provider_id: &str,
        scope: &str,
        material: CredentialMaterial,
    ) -> Result<Token, CredentialError> {
        crate::retry::with_backoff(
            &self.retry_policy,
            CredentialError::is_retryable,
            |attempt, _err, backoff| {
                tracing::debug!(
                    provider_id = %provider_id,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "credential broker: transient mint error, retrying after backoff"
                );
            },
            || {
                let material = material.clone();
                async move { self.mint_material(scope, material).await }
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::minter::Minter;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Test broker that counts mint calls — used to assert single-flight
    /// behaviour without spinning up an HTTP server.
    struct CountingBroker {
        mint_calls: AtomicUsize,
        /// Token to return on each mint.
        token: parking_lot::Mutex<Token>,
        flight: AsyncMutex<()>,
        cache: PlRwLock<Option<Token>>,
    }

    #[async_trait]
    impl CredentialBroker for CountingBroker {
        async fn token_for(
            &self,
            _provider_id: &str,
            _scope: &str,
        ) -> Result<IssuedToken, CredentialError> {
            if let Some(t) = self.cache.read().as_ref()
                && !t.is_near_expiry(SAFETY_WINDOW)
            {
                return Ok(IssuedToken::from_token(t));
            }
            let _g = self.flight.lock().await;
            if let Some(t) = self.cache.read().as_ref()
                && !t.is_near_expiry(SAFETY_WINDOW)
            {
                return Ok(IssuedToken::from_token(t));
            }
            self.mint_calls.fetch_add(1, Ordering::SeqCst);
            // Simulate slow mint so concurrent callers actually pile up.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let token = self.token.lock().clone();
            let issued = IssuedToken::from_token(&token);
            *self.cache.write() = Some(token);
            Ok(issued)
        }
    }

    fn future_token(secs: u64) -> Token {
        Token {
            bearer: awaken_runtime_contract::secret::RedactedString::new("tok"),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(secs),
        }
    }

    fn future_token_with(bearer: &str, secs: u64) -> Token {
        Token {
            bearer: awaken_runtime_contract::secret::RedactedString::new(bearer),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(secs),
        }
    }

    #[derive(Debug)]
    struct BlockingMinter {
        bearer: &'static str,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Minter for BlockingMinter {
        fn kind_label(&self) -> &'static str {
            "test_blocking"
        }

        async fn mint(
            &self,
            _scope: &str,
            _http: &reqwest::Client,
        ) -> Result<Token, CredentialError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(future_token_with(self.bearer, 3600))
        }
    }

    fn blocking_material(
        bearer: &'static str,
        started: Arc<Notify>,
        release: Arc<Notify>,
    ) -> CredentialMaterial {
        CredentialMaterial::from_minter(Arc::new(BlockingMinter {
            bearer,
            started,
            release,
        }))
    }

    #[derive(Debug)]
    struct FailsOnceMinter {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Minter for FailsOnceMinter {
        fn kind_label(&self) -> &'static str {
            "test_fails_once"
        }

        async fn mint(
            &self,
            _scope: &str,
            _http: &reqwest::Client,
        ) -> Result<Token, CredentialError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Err(CredentialError::PermanentUpstream {
                    provider_id: "p".to_string(),
                    status: 403,
                    body: "rejected".to_string(),
                });
            }
            Ok(future_token_with("after-failure", 3600))
        }
    }

    #[test]
    fn token_is_near_expiry_when_inside_safety_window() {
        // SAFETY_WINDOW is 60s — a token expiring in 30s must trigger refresh.
        let near = Token {
            bearer: awaken_runtime_contract::secret::RedactedString::new("x"),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(30),
        };
        assert!(near.is_near_expiry(SAFETY_WINDOW));

        // A token with plenty of headroom must NOT be near expiry.
        let fresh = Token {
            bearer: awaken_runtime_contract::secret::RedactedString::new("x"),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(3600),
        };
        assert!(!fresh.is_near_expiry(SAFETY_WINDOW));

        // Already-expired tokens must report near-expiry (the safety_window
        // check would otherwise fail with a None duration).
        let stale = Token {
            bearer: awaken_runtime_contract::secret::RedactedString::new("x"),
            expires_at: std::time::SystemTime::now() - Duration::from_secs(10),
        };
        assert!(stale.is_near_expiry(SAFETY_WINDOW));
    }

    #[tokio::test]
    async fn cache_hit_avoids_mint() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("k"),
            ),
        );
        let a = broker.token_for("p", "any").await.unwrap();
        let b = broker.token_for("p", "any").await.unwrap();
        assert_eq!(a.bearer(), b.bearer());
    }

    #[tokio::test]
    async fn unregistered_provider_returns_not_configured() {
        let broker = AwakenCredentialBroker::new();
        let err = broker.token_for("missing", "any").await.unwrap_err();
        assert!(matches!(err, CredentialError::NotConfigured(_)));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn deregister_drops_cache() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("k1"),
            ),
        );
        let _ = broker.token_for("p", "any").await.unwrap();
        broker.deregister("p");
        let err = broker.token_for("p", "any").await.unwrap_err();
        assert!(matches!(err, CredentialError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn deregister_drops_flight_slots() {
        // Without dropping flight slots, `deregister` would leak entries in
        // `flights` for every provider that had ever minted a token.
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("k"),
            ),
        );
        // Touch token_for so a flight slot is created for (p, scope).
        let _ = broker.token_for("p", "scope").await.unwrap();
        assert!(
            broker.flights.read().keys().any(|k| k.provider_id == "p"),
            "precondition: flight slot must exist after a mint"
        );
        broker.deregister("p");
        assert!(
            !broker.flights.read().keys().any(|k| k.provider_id == "p"),
            "deregister must drop flight slots for the provider"
        );
    }

    #[tokio::test]
    async fn re_register_invalidates_cache_so_new_material_takes_effect() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("k1"),
            ),
        );
        assert_eq!(broker.token_for("p", "s").await.unwrap().bearer(), "k1");

        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("k2"),
            ),
        );
        assert_eq!(broker.token_for("p", "s").await.unwrap().bearer(), "k2");
    }

    #[tokio::test]
    async fn register_during_in_flight_mint_discards_stale_token() {
        let broker = Arc::new(AwakenCredentialBroker::new());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        broker.register(
            "p".to_string(),
            blocking_material("old", Arc::clone(&started), Arc::clone(&release)),
        );

        let task = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move { broker.token_for("p", "s").await })
        };
        started.notified().await;

        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("new"),
            ),
        );
        release.notify_one();

        let issued = task.await.unwrap().unwrap();
        assert_eq!(issued.bearer(), "new");
        assert_eq!(broker.token_for("p", "s").await.unwrap().bearer(), "new");
    }

    #[tokio::test]
    async fn deregister_during_in_flight_mint_discards_stale_token() {
        let broker = Arc::new(AwakenCredentialBroker::new());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        broker.register(
            "p".to_string(),
            blocking_material("old", Arc::clone(&started), Arc::clone(&release)),
        );

        let task = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move { broker.token_for("p", "s").await })
        };
        started.notified().await;

        broker.deregister("p");
        release.notify_one();

        let err = task.await.unwrap().unwrap_err();
        assert!(matches!(err, CredentialError::NotConfigured(provider) if provider == "p"));
        assert!(matches!(
            broker.token_for("p", "s").await.unwrap_err(),
            CredentialError::NotConfigured(provider) if provider == "p"
        ));
    }

    #[tokio::test]
    async fn different_scopes_have_independent_cache_entries() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("k"),
            ),
        );
        // Both scopes should resolve to the same static bearer (because
        // for static bearer the scope is irrelevant) but should have
        // independent cache entries — i.e. registering new material drops
        // both. We assert that drop semantics indirectly by registering
        // different material and checking both scope reads return new value.
        let _ = broker.token_for("p", "scope-a").await.unwrap();
        let _ = broker.token_for("p", "scope-b").await.unwrap();
        broker.register(
            "p".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("rotated"),
            ),
        );
        assert_eq!(
            broker.token_for("p", "scope-a").await.unwrap().bearer(),
            "rotated"
        );
        assert_eq!(
            broker.token_for("p", "scope-b").await.unwrap().bearer(),
            "rotated"
        );
    }

    #[tokio::test]
    async fn mint_failures_are_not_cached_or_replayed_to_later_callers() {
        let broker = AwakenCredentialBroker::new();
        let calls = Arc::new(AtomicUsize::new(0));
        broker.register(
            "p".to_string(),
            CredentialMaterial::from_minter(Arc::new(FailsOnceMinter {
                calls: Arc::clone(&calls),
            })),
        );

        let err = broker.token_for("p", "scope").await.unwrap_err();
        assert!(matches!(
            err,
            CredentialError::PermanentUpstream { status: 403, .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let issued = broker.token_for("p", "scope").await.unwrap();
        assert_eq!(issued.bearer(), "after-failure");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "broker must mint again after a failed exchange instead of caching the error"
        );

        let cached = broker.token_for("p", "scope").await.unwrap();
        assert_eq!(cached.bearer(), "after-failure");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "successful retry may be cached, but the failed exchange must not be"
        );
    }

    #[tokio::test]
    async fn concurrent_rotation_is_isolated_by_provider_and_scope() {
        let broker = Arc::new(AwakenCredentialBroker::new());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        broker.register(
            "rotating".to_string(),
            blocking_material("old-rotating", Arc::clone(&started), Arc::clone(&release)),
        );
        broker.register(
            "stable".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("stable-token"),
            ),
        );

        let rotating_task = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move { broker.token_for("rotating", "scope-a").await })
        };
        started.notified().await;

        assert_eq!(
            broker
                .token_for("stable", "scope-a")
                .await
                .unwrap()
                .bearer(),
            "stable-token",
            "unrelated providers must remain readable while another provider is mid-mint"
        );

        broker.register(
            "rotating".to_string(),
            CredentialMaterial::static_bearer(
                awaken_runtime_contract::secret::RedactedString::new("new-rotating"),
            ),
        );
        release.notify_one();

        let issued = rotating_task.await.unwrap().unwrap();
        assert_eq!(
            issued.bearer(),
            "new-rotating",
            "in-flight mint from the old generation must be discarded"
        );
        assert_eq!(
            broker
                .token_for("rotating", "scope-b")
                .await
                .unwrap()
                .bearer(),
            "new-rotating",
            "rotation must apply to all scopes for the provider"
        );
        assert_eq!(
            broker
                .token_for("stable", "scope-b")
                .await
                .unwrap()
                .bearer(),
            "stable-token",
            "rotation must not invalidate unrelated providers"
        );
    }

    #[tokio::test]
    async fn single_flight_collapses_concurrent_mint_calls() {
        // The CountingBroker intentionally takes 20ms per mint; if
        // single-flight works, 50 concurrent token_for calls should mint
        // exactly once.
        let broker = Arc::new(CountingBroker {
            mint_calls: AtomicUsize::new(0),
            token: parking_lot::Mutex::new(future_token(3600)),
            flight: AsyncMutex::new(()),
            cache: PlRwLock::new(None),
        });

        let mut handles = Vec::new();
        for _ in 0..50 {
            let b = broker.clone();
            handles.push(tokio::spawn(async move {
                b.token_for("p", "s").await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let mints = broker.mint_calls.load(Ordering::SeqCst);
        assert_eq!(
            mints, 1,
            "expected exactly 1 mint under single-flight, got {mints}"
        );
    }

    // ── CredentialError retry classification ─────────────────────────────
    //
    // The broker's contribution to the shared retry primitive is its
    // is_retryable() classification — the loop math itself is exercised
    // by `crate::retry::tests`. Pin every variant here so a future change
    // to the enum forces a deliberate decision, not a silent flip.

    #[test]
    fn credential_error_retry_classification_is_pinned_per_variant() {
        let pid = "p".to_owned();
        assert!(!CredentialError::NotConfigured(pid.clone()).is_retryable());
        assert!(
            !CredentialError::InvalidMaterial {
                provider_id: pid.clone(),
                reason: "x".into(),
            }
            .is_retryable()
        );
        assert!(
            !CredentialError::SigningFailed {
                provider_id: pid.clone(),
                reason: "x".into(),
            }
            .is_retryable()
        );
        assert!(
            !CredentialError::PermanentUpstream {
                provider_id: pid.clone(),
                status: 403,
                body: String::new(),
            }
            .is_retryable()
        );
        assert!(
            CredentialError::TransientUpstream {
                provider_id: pid.clone(),
                reason: "503".into(),
            }
            .is_retryable()
        );
        assert!(
            CredentialError::Network {
                provider_id: pid,
                reason: "tcp reset".into(),
            }
            .is_retryable()
        );
    }
}
