use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use awaken_contract::ProviderSpec;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_runtime::registry::ModelCapabilityPatch;

/// Per-provider executor cache entry: the spec used to build the cached
/// executor and the executor itself.
pub(super) type ProviderExecutorCache = HashMap<String, (ProviderSpec, Arc<dyn LlmExecutor>)>;

/// Maximum age a provider-discovered capability snapshot may reach before it is
/// no longer served as runtime-trusted data. Discovery runs on every config
/// publish, so a successful provider stays refreshed well inside this window;
/// the TTL exists to bound how long a *stale* snapshot can keep driving the
/// modality guard and knowledge-cutoff context after the provider stops
/// returning usable `/models` data (discovery failures retain the last
/// snapshot, but only until it expires). Twelve hours keeps a provider that is
/// briefly unreachable trusted across short outages while ensuring metadata
/// that the provider has silently changed cannot stay trusted indefinitely.
const CAPABILITY_SNAPSHOT_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Cached provider capability snapshot tagged with the provider signature it was
/// discovered under and the time it was discovered, so stale snapshots can be
/// expired by age.
#[derive(Clone)]
struct CachedCapabilitySnapshot {
    signature: String,
    discovered_at: SystemTime,
    capabilities: HashMap<String, ModelCapabilityPatch>,
}

impl CachedCapabilitySnapshot {
    fn is_expired(&self, now: SystemTime, ttl: Duration) -> bool {
        now.duration_since(self.discovered_at)
            .map(|age| age > ttl)
            // A `discovered_at` in the future (clock moved backwards) is not
            // older than the TTL, so treat it as still fresh.
            .unwrap_or(false)
    }
}

type ProviderCapabilityCache = HashMap<String, CachedCapabilitySnapshot>;

/// A computed-but-uncommitted capability cache. `resolved` is the snapshot map
/// handed to compilation; `cache` is the next cache state, committed via
/// [`ProviderRuntimeCache::commit_capabilities`] only once the publish
/// transaction (versioned publish + runtime swap) has succeeded — so a failed
/// publish never leaves discovered metadata in the trusted cache.
pub(super) struct StagedCapabilityCache {
    cache: ProviderCapabilityCache,
    pub(super) resolved: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
}

#[derive(Default)]
pub(super) struct ProviderRuntimeCache {
    executors: ProviderExecutorCache,
    capabilities: ProviderCapabilityCache,
}

impl ProviderRuntimeCache {
    pub(super) fn executor_snapshot(&self) -> ProviderExecutorCache {
        self.executors.clone()
    }

    pub(super) fn replace_executors(&mut self, next: ProviderExecutorCache) {
        self.executors = next;
    }

    #[cfg(test)]
    pub(super) fn executor_provider(&self, provider_id: &str) -> Option<ProviderSpec> {
        self.executors
            .get(provider_id)
            .map(|(provider, _)| provider.clone())
    }

    /// Compute the next capability cache (retain still-valid snapshots, merge
    /// discovery) **without** committing it. The returned [`StagedCapabilityCache`]
    /// carries the resolved snapshot map for compilation; its cache is applied
    /// only by [`commit_capabilities`](Self::commit_capabilities) after the
    /// publish transaction succeeds, so a failed publish cannot pollute the
    /// trusted cache.
    pub(super) fn stage_capability_snapshots(
        &self,
        providers: &[ProviderSpec],
        discovered: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
        attempted: &HashSet<String>,
        provider_signature: impl Fn(&ProviderSpec) -> String,
        now: SystemTime,
    ) -> StagedCapabilityCache {
        self.stage_capability_snapshots_with_ttl(
            providers,
            discovered,
            attempted,
            provider_signature,
            now,
            CAPABILITY_SNAPSHOT_TTL,
        )
    }

    fn stage_capability_snapshots_with_ttl(
        &self,
        providers: &[ProviderSpec],
        discovered: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
        attempted: &HashSet<String>,
        provider_signature: impl Fn(&ProviderSpec) -> String,
        now: SystemTime,
        ttl: Duration,
    ) -> StagedCapabilityCache {
        let signatures = providers
            .iter()
            .map(|provider| (provider.id.clone(), provider_signature(provider)))
            .collect::<HashMap<_, _>>();
        // Retain a snapshot only while its provider signature is unchanged and
        // it is still within the TTL. An expired snapshot is dropped here so it
        // can neither be re-served nor retained across a later discovery
        // failure. This reads the current cache but does not mutate it.
        let mut staged: ProviderCapabilityCache = self
            .capabilities
            .iter()
            .filter(|(provider_id, snapshot)| {
                signatures
                    .get(*provider_id)
                    .is_some_and(|current| *current == snapshot.signature)
                    && !snapshot.is_expired(now, ttl)
            })
            .map(|(provider_id, snapshot)| (provider_id.clone(), snapshot.clone()))
            .collect();
        let discovered_provider_ids = discovered.keys().cloned().collect::<HashSet<_>>();
        for (provider_id, capabilities) in discovered {
            let Some(signature) = signatures.get(&provider_id) else {
                continue;
            };
            staged.insert(
                provider_id,
                CachedCapabilitySnapshot {
                    signature: signature.clone(),
                    discovered_at: now,
                    capabilities,
                },
            );
        }
        // Warn only when a retained snapshot is being served *because discovery
        // was attempted and failed* — not when discovery was unnecessary this
        // round (no referenced model needed it, or the endpoint was skipped),
        // which would be a false alarm.
        for provider_id in staged.keys() {
            if !discovered_provider_ids.contains(provider_id) && attempted.contains(provider_id) {
                tracing::warn!(
                    provider_id,
                    "using stale provider capability snapshot after discovery failure"
                );
            }
        }
        let resolved = staged
            .iter()
            .map(|(provider_id, snapshot)| (provider_id.clone(), snapshot.capabilities.clone()))
            .collect();
        StagedCapabilityCache {
            cache: staged,
            resolved,
        }
    }

    /// Commit a previously [`stage_capability_snapshots`](Self::stage_capability_snapshots)
    /// result, replacing the trusted capability cache. Called only after the
    /// publish transaction succeeds, alongside [`replace_executors`](Self::replace_executors).
    pub(super) fn commit_capabilities(&mut self, staged: StagedCapabilityCache) {
        self.capabilities = staged.cache;
    }

    /// Test convenience: stage and immediately commit, returning the resolved
    /// snapshot map — the pre-transactional behavior, used to exercise the
    /// retain/merge/expiry logic directly.
    #[cfg(test)]
    fn update_capability_snapshots_with_ttl(
        &mut self,
        providers: &[ProviderSpec],
        discovered: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
        provider_signature: impl Fn(&ProviderSpec) -> String,
        now: SystemTime,
        ttl: Duration,
    ) -> HashMap<String, HashMap<String, ModelCapabilityPatch>> {
        // Model a normal pass where every discovered provider was attempted.
        let attempted: HashSet<String> = discovered.keys().cloned().collect();
        let staged = self.stage_capability_snapshots_with_ttl(
            providers,
            discovered,
            &attempted,
            provider_signature,
            now,
            ttl,
        );
        let resolved = staged.resolved.clone();
        self.commit_capabilities(staged);
        resolved
    }

    #[cfg(test)]
    fn update_capability_snapshots(
        &mut self,
        providers: &[ProviderSpec],
        discovered: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
        provider_signature: impl Fn(&ProviderSpec) -> String,
        now: SystemTime,
    ) -> HashMap<String, HashMap<String, ModelCapabilityPatch>> {
        self.update_capability_snapshots_with_ttl(
            providers,
            discovered,
            provider_signature,
            now,
            CAPABILITY_SNAPSHOT_TTL,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature(provider: &ProviderSpec) -> String {
        provider.base_url.clone().unwrap_or_default()
    }

    fn patch(context_window: u32) -> ModelCapabilityPatch {
        ModelCapabilityPatch {
            context_window: Some(context_window),
            max_output_tokens: None,
            modalities: None,
            knowledge_cutoff: None,
        }
    }

    #[test]
    fn staged_capability_snapshot_is_not_served_until_committed() {
        // Models a publish whose discovery succeeded but whose later
        // compile/validate/publish/runtime-swap failed: the discovered metadata
        // is staged, never committed, and must not leak into the trusted cache
        // where a subsequent discovery failure could re-serve it.
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };
        let mut cache = ProviderRuntimeCache::default();
        let now = SystemTime::UNIX_EPOCH;
        let attempted = HashSet::from(["p".to_string()]);

        let staged = cache.stage_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            &attempted,
            signature,
            now,
        );
        assert!(staged.resolved.contains_key("p"));

        // Failed publish: stage is dropped, never committed. A later discovery
        // failure must see nothing.
        let after_failed_publish = cache.stage_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::new(),
            &attempted,
            signature,
            now + Duration::from_secs(60),
        );
        assert!(
            after_failed_publish.resolved.is_empty(),
            "an uncommitted (failed-publish) snapshot must not be served"
        );

        // Committing a successful stage does retain across a later failure.
        let committed = cache.stage_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            &attempted,
            signature,
            now,
        );
        cache.commit_capabilities(committed);
        let after_commit = cache.stage_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::new(),
            &attempted,
            signature,
            now + Duration::from_secs(60),
        );
        assert_eq!(
            after_commit.resolved["p"]["gpt-4o"].context_window,
            Some(128_000)
        );
    }

    #[test]
    fn capability_snapshot_merge_keeps_cached_snapshot_on_discovery_failure() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };
        let mut cache = ProviderRuntimeCache::default();
        let now = SystemTime::UNIX_EPOCH;
        let first = cache.update_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            signature,
            now,
        );
        // A discovery failure (empty map) within the TTL keeps the snapshot.
        let second = cache.update_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::new(),
            signature,
            now + Duration::from_secs(60),
        );

        assert_eq!(first, second);
    }

    #[test]
    fn capability_snapshot_expires_after_ttl_on_discovery_failure() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };
        let mut cache = ProviderRuntimeCache::default();
        let ttl = Duration::from_secs(3_600);
        let now = SystemTime::UNIX_EPOCH;
        let first = cache.update_capability_snapshots_with_ttl(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            signature,
            now,
            ttl,
        );
        assert!(!first.is_empty());

        // A discovery failure past the TTL must drop the stale snapshot so it is
        // no longer served as runtime-trusted data.
        let expired = cache.update_capability_snapshots_with_ttl(
            std::slice::from_ref(&provider),
            HashMap::new(),
            signature,
            now + ttl + Duration::from_secs(1),
            ttl,
        );

        assert!(expired.is_empty());
    }

    #[test]
    fn capability_snapshot_within_ttl_is_still_served() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };
        let mut cache = ProviderRuntimeCache::default();
        let ttl = Duration::from_secs(3_600);
        let now = SystemTime::UNIX_EPOCH;
        cache.update_capability_snapshots_with_ttl(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            signature,
            now,
            ttl,
        );

        let still_fresh = cache.update_capability_snapshots_with_ttl(
            std::slice::from_ref(&provider),
            HashMap::new(),
            signature,
            now + ttl,
            ttl,
        );

        assert_eq!(still_fresh["p"]["gpt-4o"].context_window, Some(128_000));
    }

    #[test]
    fn capability_snapshot_empty_success_replaces_cached_snapshot() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };
        let mut cache = ProviderRuntimeCache::default();
        let now = SystemTime::UNIX_EPOCH;
        cache.update_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            signature,
            now,
        );

        let refreshed = cache.update_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::from([("p".into(), HashMap::new())]),
            signature,
            now + Duration::from_secs(60),
        );

        assert_eq!(refreshed.get("p"), Some(&HashMap::new()));
    }

    #[test]
    fn capability_snapshot_merge_drops_cached_snapshot_after_provider_change() {
        let provider = ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            base_url: Some("https://example.test/v1".into()),
            ..ProviderSpec::default()
        };
        let changed = ProviderSpec {
            base_url: Some("https://other.example.test/v1".into()),
            ..provider.clone()
        };
        let mut cache = ProviderRuntimeCache::default();
        let now = SystemTime::UNIX_EPOCH;
        cache.update_capability_snapshots(
            std::slice::from_ref(&provider),
            HashMap::from([(
                "p".into(),
                HashMap::from([("gpt-4o".into(), patch(128_000))]),
            )]),
            signature,
            now,
        );
        let merged = cache.update_capability_snapshots(
            std::slice::from_ref(&changed),
            HashMap::new(),
            signature,
            now,
        );

        assert!(merged.is_empty());
    }
}
