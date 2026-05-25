use std::collections::HashMap;
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

    pub(super) fn update_capability_snapshots(
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

    fn update_capability_snapshots_with_ttl(
        &mut self,
        providers: &[ProviderSpec],
        discovered: HashMap<String, HashMap<String, ModelCapabilityPatch>>,
        provider_signature: impl Fn(&ProviderSpec) -> String,
        now: SystemTime,
        ttl: Duration,
    ) -> HashMap<String, HashMap<String, ModelCapabilityPatch>> {
        let signatures = providers
            .iter()
            .map(|provider| (provider.id.clone(), provider_signature(provider)))
            .collect::<HashMap<_, _>>();
        // Retain a snapshot only while its provider signature is unchanged and
        // it is still within the TTL. An expired snapshot is dropped here so it
        // can neither be re-served nor retained across a later discovery
        // failure.
        self.capabilities.retain(|provider_id, snapshot| {
            signatures
                .get(provider_id)
                .is_some_and(|current| *current == snapshot.signature)
                && !snapshot.is_expired(now, ttl)
        });
        for (provider_id, capabilities) in discovered {
            let Some(signature) = signatures.get(&provider_id) else {
                continue;
            };
            self.capabilities.insert(
                provider_id,
                CachedCapabilitySnapshot {
                    signature: signature.clone(),
                    discovered_at: now,
                    capabilities,
                },
            );
        }
        self.capabilities
            .iter()
            .map(|(provider_id, snapshot)| (provider_id.clone(), snapshot.capabilities.clone()))
            .collect()
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
