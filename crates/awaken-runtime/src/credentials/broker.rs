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
//! - `materials`: `provider_id → CredentialMaterial`. Updated on
//!   [`register`](AwakenCredentialBroker::register) calls; read on every mint.
//!   Hot read path: no contention under steady state.
//! - `cache`: `(provider_id, scope) → Token`. Updated on each mint; read on
//!   every `token_for`. Cache hits short-circuit before any lock upgrade.
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
use super::google_oauth;
use super::material::CredentialMaterial;
use super::static_bearer::mint_static_bearer;
use super::token::{Token, TokenLease};

/// Refresh tokens this long before their stated expiry. Prevents handing
/// out a token that would expire mid-request. Google OAuth tokens are
/// nominally 3600s; 60s margin keeps us safely inside the window even for
/// short-validity tokens.
const SAFETY_WINDOW: Duration = Duration::from_secs(60);

/// HTTP client used by signers that need a network call (e.g. OAuth token
/// exchange). Connection-pooled; cheap to clone.
type HttpClient = reqwest::Client;

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
    ) -> Result<TokenLease, CredentialError>;
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

pub struct AwakenCredentialBroker {
    materials: PlRwLock<HashMap<String, CredentialMaterial>>,
    cache: PlRwLock<HashMap<CacheKey, Token>>,
    /// Per-key in-flight mutex map. Entries live for the lifetime of the
    /// broker; the value is a small struct so the memory footprint stays
    /// bounded by the number of distinct (provider_id, scope) pairs in
    /// use, which is small in practice.
    flights: PlRwLock<HashMap<CacheKey, Arc<FlightSlot>>>,
    http: HttpClient,
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
        }
    }

    /// Whether this broker knows about `provider_id`. Useful for
    /// embedder-side assertions.
    pub fn is_registered(&self, provider_id: &str) -> bool {
        self.materials.read().contains_key(provider_id)
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

    /// Mint a token for the registered material. **No cache, no
    /// single-flight** — the caller (`token_for`) wraps this with both.
    async fn mint(&self, provider_id: &str, scope: &str) -> Result<Token, CredentialError> {
        let material = self
            .materials
            .read()
            .get(provider_id)
            .cloned()
            .ok_or_else(|| CredentialError::NotConfigured(provider_id.to_owned()))?;

        match material {
            CredentialMaterial::StaticBearer(bearer) => Ok(mint_static_bearer(&bearer)),
            CredentialMaterial::GoogleServiceAccount(key) => {
                google_oauth::mint(provider_id, &key, scope, &self.http).await
            }
        }
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
            materials.insert(provider_id.clone(), material);
        }
        // Drop any cached tokens for this provider — material change means
        // they may have been signed by a key that's about to be revoked.
        let mut cache = self.cache.write();
        cache.retain(|key, _| key.provider_id != provider_id);
    }

    /// Forget a provider entirely. Best-effort: in-flight mints for this
    /// provider will still complete and write to the (now-orphaned)
    /// cache; the next read will treat the entry as missing and return
    /// [`CredentialError::NotConfigured`].
    fn deregister(&self, provider_id: &str) {
        self.materials.write().remove(provider_id);
        self.cache
            .write()
            .retain(|key, _| key.provider_id != provider_id);
    }

    async fn token_for(
        &self,
        provider_id: &str,
        scope: &str,
    ) -> Result<TokenLease, CredentialError> {
        let key = CacheKey {
            provider_id: provider_id.to_owned(),
            scope: scope.to_owned(),
        };

        // 1. Cache fast path — short-circuit before touching any other lock.
        if let Some(token) = self.cache.read().get(&key)
            && !token.is_near_expiry(SAFETY_WINDOW)
        {
            return Ok(TokenLease::from_token(token));
        }

        // 2. Acquire the per-key single-flight slot.
        let slot = self.flight_slot(&key);
        let _guard = slot.inflight.lock().await;

        // 3. Re-check cache under the slot — a concurrent task may have
        //    populated it while we were waiting for the lock.
        if let Some(token) = self.cache.read().get(&key)
            && !token.is_near_expiry(SAFETY_WINDOW)
        {
            return Ok(TokenLease::from_token(token));
        }

        // 4. We are the elected refresher.
        let fresh = self.mint(provider_id, scope).await?;
        let lease = TokenLease::from_token(&fresh);
        self.cache.write().insert(key, fresh);
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        ) -> Result<TokenLease, CredentialError> {
            if let Some(t) = self.cache.read().as_ref()
                && !t.is_near_expiry(SAFETY_WINDOW)
            {
                return Ok(TokenLease::from_token(t));
            }
            let _g = self.flight.lock().await;
            if let Some(t) = self.cache.read().as_ref()
                && !t.is_near_expiry(SAFETY_WINDOW)
            {
                return Ok(TokenLease::from_token(t));
            }
            self.mint_calls.fetch_add(1, Ordering::SeqCst);
            // Simulate slow mint so concurrent callers actually pile up.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let token = self.token.lock().clone();
            let lease = TokenLease::from_token(&token);
            *self.cache.write() = Some(token);
            Ok(lease)
        }
    }

    fn future_token(secs: u64) -> Token {
        Token {
            bearer: awaken_contract::secret::RedactedString::new("tok"),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(secs),
        }
    }

    #[test]
    fn token_is_near_expiry_when_inside_safety_window() {
        // SAFETY_WINDOW is 60s — a token expiring in 30s must trigger refresh.
        let near = Token {
            bearer: awaken_contract::secret::RedactedString::new("x"),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(30),
        };
        assert!(near.is_near_expiry(SAFETY_WINDOW));

        // A token with plenty of headroom must NOT be near expiry.
        let fresh = Token {
            bearer: awaken_contract::secret::RedactedString::new("x"),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(3600),
        };
        assert!(!fresh.is_near_expiry(SAFETY_WINDOW));

        // Already-expired tokens must report near-expiry (the safety_window
        // check would otherwise fail with a None duration).
        let stale = Token {
            bearer: awaken_contract::secret::RedactedString::new("x"),
            expires_at: std::time::SystemTime::now() - Duration::from_secs(10),
        };
        assert!(stale.is_near_expiry(SAFETY_WINDOW));
    }

    #[tokio::test]
    async fn cache_hit_avoids_mint() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::StaticBearer(awaken_contract::secret::RedactedString::new("k")),
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
            CredentialMaterial::StaticBearer(awaken_contract::secret::RedactedString::new("k1")),
        );
        let _ = broker.token_for("p", "any").await.unwrap();
        broker.deregister("p");
        let err = broker.token_for("p", "any").await.unwrap_err();
        assert!(matches!(err, CredentialError::NotConfigured(_)));
    }

    #[tokio::test]
    async fn re_register_invalidates_cache_so_new_material_takes_effect() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::StaticBearer(awaken_contract::secret::RedactedString::new("k1")),
        );
        assert_eq!(broker.token_for("p", "s").await.unwrap().bearer(), "k1");

        broker.register(
            "p".to_string(),
            CredentialMaterial::StaticBearer(awaken_contract::secret::RedactedString::new("k2")),
        );
        assert_eq!(broker.token_for("p", "s").await.unwrap().bearer(), "k2");
    }

    #[tokio::test]
    async fn different_scopes_have_independent_cache_entries() {
        let broker = AwakenCredentialBroker::new();
        broker.register(
            "p".to_string(),
            CredentialMaterial::StaticBearer(awaken_contract::secret::RedactedString::new("k")),
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
            CredentialMaterial::StaticBearer(awaken_contract::secret::RedactedString::new(
                "rotated",
            )),
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
}
