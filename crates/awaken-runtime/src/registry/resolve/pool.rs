//! Pool resolution: turn a [`ModelPoolSpec`] into a [`PoolExecutor`] presenting
//! the single-model contract, plus capability reconciliation so the pool clamps
//! context like a model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use awaken_runtime_contract::contract::executor::LlmExecutor;
use awaken_runtime_contract::registry_spec::{ModelPoolSpec, ModelSpec};

use crate::engine::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::engine::pool_executor::{PoolExecutor, PoolMemberExecutor};
use crate::engine::pool_router::{PoolRouter, RouterMember};
use crate::engine::retry::{LlmRetryPolicy, RetryingExecutor};
use crate::registry::model_capabilities::{
    CapabilitySource, ModelCapabilitySources, resolve_model_capabilities,
};
use crate::registry::traits::RegistrySet;

use super::error::ResolveError;

/// Build a [`PoolExecutor`] for `pool` over the registry's member models.
///
/// Each member resolves to its provider executor (wrapped in a
/// [`RetryingExecutor`] when the agent retry policy is active, so transient
/// blips are absorbed before the pool considers a switch). `home_key` (the
/// agent id) drives deterministic home selection; a process-shared circuit
/// breaker keyed by pool id carries member health across sessions.
///
/// Returns the executor, the stand-in upstream model name written onto
/// requests (the pool overrides it per member), and the reconciled
/// [`ModelSpec`] used for context-window clamping.
pub fn build_pool_executor(
    registries: &RegistrySet,
    pool: &ModelPoolSpec,
    home_key: &str,
    policy: &LlmRetryPolicy,
) -> Result<
    (
        Arc<dyn LlmExecutor>,
        String,
        ModelSpec,
        ModelCapabilitySources,
    ),
    ResolveError,
> {
    let mut router_members = Vec::with_capacity(pool.members.len());
    let mut member_execs = Vec::with_capacity(pool.members.len());
    let mut member_specs = Vec::with_capacity(pool.members.len());
    let mut member_capability_sources = Vec::with_capacity(pool.members.len());
    let mut provider_signatures = Vec::with_capacity(pool.members.len());

    for member in &pool.members {
        let model = registries
            .models
            .get_model(&member.model_id)
            .ok_or_else(|| ResolveError::ModelNotFound(member.model_id.clone()))?;
        let provider_source = registries
            .providers
            .provider_capability_source(&model.provider_id);
        let discovered = registries
            .providers
            .provider_model_capability(&model.provider_id, &model.upstream_model);
        let resolved =
            resolve_model_capabilities(model, provider_source.as_deref(), discovered.as_ref());
        let model = resolved.model;
        let capability_sources = resolved.sources;
        let provider_signature = registries
            .providers
            .provider_signature(&model.provider_id)
            .unwrap_or_else(|| model.provider_id.clone());
        let provider_executor = registries
            .providers
            .get_provider(&model.provider_id)
            .ok_or_else(|| ResolveError::ProviderNotFound(model.provider_id.clone()))?;
        let provider_signature = format!("{provider_signature}:{}", provider_executor.name());
        let executor = if policy.max_retries > 0 {
            Arc::new(RetryingExecutor::new(provider_executor, policy.clone()))
                as Arc<dyn LlmExecutor>
        } else {
            provider_executor
        };
        let executor = crate::engine::ModalityGuardExecutor::wrap_trusted(
            executor,
            &model,
            capability_sources.input_modalities,
        );

        router_members.push(RouterMember {
            model_id: member.model_id.clone(),
            role: member.role,
            weight: member.weight.unwrap_or(1),
        });
        member_execs.push(PoolMemberExecutor {
            model_id: member.model_id.clone(),
            upstream_model: model.upstream_model.clone(),
            executor,
        });
        member_specs.push(model);
        member_capability_sources.push(capability_sources);
        provider_signatures.push(provider_signature);
    }

    let (reconciled, pool_capability_sources) = reconcile_pool_capabilities_with_sources(
        &pool.id,
        &member_specs,
        &member_capability_sources,
    );
    let router = PoolRouter::new(router_members, pool.routing.clone(), pool.switch.clone());
    let breaker = pool_breaker(&pool_breaker_key(pool, &member_specs, &provider_signatures));
    let upstream_stand_in = reconciled.upstream_model.clone();
    let executor: Arc<dyn LlmExecutor> = Arc::new(PoolExecutor::new(
        pool.id.clone(),
        home_key,
        member_execs,
        router,
        breaker,
    ));
    Ok((
        executor,
        upstream_stand_in,
        reconciled,
        pool_capability_sources,
    ))
}

/// Upper bound on distinct breaker keys retained at once. Each key embeds the
/// pool id, a hash of the serialized pool/member specs, and the provider
/// signature, so every config hot-reload that changes any of those mints a new
/// key. Without a bound the cache would accumulate one stranded breaker per
/// historical config revision for the life of the process. 1024 leaves ample
/// room for many concurrent pools across many recent revisions while capping
/// memory; evicting the least-recently-used key only resets that rarely-touched
/// config's circuit state, which self-heals on the next failure/success.
const MAX_POOL_BREAKERS: usize = 1024;

/// Access-ordered, bounded registry of pool circuit breakers. Mirrors the
/// monotonic `last_access` counter scheme used by
/// `PoolExecutorInner::ensure_stream_attempt_capacity`: each touch stamps the
/// entry with the next sequence value, and eviction drops the smallest stamp.
#[derive(Default)]
struct BoundedBreakerCache {
    entries: HashMap<String, (Arc<CircuitBreaker>, u64)>,
    access_seq: u64,
}

impl BoundedBreakerCache {
    fn next_access(&mut self) -> u64 {
        self.access_seq = self.access_seq.wrapping_add(1);
        self.access_seq
    }

    fn get_or_insert(&mut self, key: &str) -> Arc<CircuitBreaker> {
        if self.entries.contains_key(key) {
            let stamp = self.next_access();
            let (breaker, last_access) = self.entries.get_mut(key).expect("checked above");
            *last_access = stamp;
            return breaker.clone();
        }
        if self.entries.len() >= MAX_POOL_BREAKERS
            && let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, last_access))| *last_access)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&victim);
        }
        let stamp = self.next_access();
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default()));
        self.entries
            .insert(key.to_string(), (breaker.clone(), stamp));
        breaker
    }
}

/// Process-shared circuit breaker for a pool, created on first use. Sharing the
/// breaker across resolutions gives member health cross-session memory: while a
/// member is unhealthy every session avoids it, and sessions return once it
/// heals. Breakers reset on process restart.
///
/// The backing registry is bounded ([`MAX_POOL_BREAKERS`], LRU eviction) so
/// config hot-reload — which mints a fresh key per revision — cannot leak
/// breakers without limit.
fn pool_breaker(key: &str) -> Arc<CircuitBreaker> {
    static BREAKERS: OnceLock<Mutex<BoundedBreakerCache>> = OnceLock::new();
    let breakers = BREAKERS.get_or_init(|| Mutex::new(BoundedBreakerCache::default()));
    let mut guard = breakers.lock().expect("pool breaker registry poisoned");
    guard.get_or_insert(key)
}

fn pool_breaker_key(
    pool: &ModelPoolSpec,
    members: &[ModelSpec],
    provider_signatures: &[String],
) -> String {
    let mut input = serde_json::to_string(pool).unwrap_or_else(|_| pool.id.clone());
    for (idx, model) in members.iter().enumerate() {
        input.push('\n');
        input.push_str(&model.id);
        input.push('\t');
        input.push_str(&model.provider_id);
        input.push('\t');
        input.push_str(&model.upstream_model);
        input.push('\t');
        if let Some(signature) = provider_signatures.get(idx) {
            input.push_str(signature);
        }
    }
    format!("{}:{:016x}", pool.id, fnv1a(input.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Synthesize the [`ModelSpec`] the runtime should treat the pool as, so the
/// context-window policy clamps consistently regardless of which member serves
/// a request.
///
/// Only the capability fields with runtime behavior are reconciled:
/// - `context_window` / `max_output_tokens`: the minimum bound only when every
///   member declares that bound. If any member is unknown, the pool bound is
///   unknown too, because routing may select that member.
///
/// Modalities and pricing are left unset: they cannot be soundly attributed to
/// a single member. Knowledge cutoff is exposed only when every member has the
/// same trusted cutoff, so the pool can install one deterministic context
/// plugin without depending on the eventual routed member.
#[cfg(test)]
fn reconcile_pool_capabilities(pool_id: &str, members: &[ModelSpec]) -> ModelSpec {
    reconcile_pool_capabilities_with_sources(pool_id, members, &[]).0
}

fn reconcile_pool_capabilities_with_sources(
    pool_id: &str,
    members: &[ModelSpec],
    sources: &[ModelCapabilitySources],
) -> (ModelSpec, ModelCapabilitySources) {
    let min_declared = |f: fn(&ModelSpec) -> Option<u32>| {
        members
            .iter()
            .map(f)
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().min())
    };

    let mut capability_sources = ModelCapabilitySources::default();
    let trusted_cutoff = common_trusted_knowledge_cutoff(members, sources);
    if trusted_cutoff.is_some() {
        capability_sources.knowledge_cutoff = sources.first().and_then(|source| {
            source
                .knowledge_cutoff
                .filter(|source| source.is_runtime_trusted())
        });
    }

    let mut spec = ModelSpec {
        context_window: min_declared(|m| m.context_window),
        max_output_tokens: min_declared(|m| m.max_output_tokens),
        ..ModelSpec::new(pool_id, pool_id, pool_id)
    };
    spec.knowledge_cutoff = trusted_cutoff;
    (spec, capability_sources)
}

fn common_trusted_knowledge_cutoff(
    members: &[ModelSpec],
    sources: &[ModelCapabilitySources],
) -> Option<String> {
    if members.is_empty() || members.len() != sources.len() {
        return None;
    }
    let first = members.first()?.knowledge_cutoff.as_ref()?;
    let all_same_trusted = members.iter().zip(sources).all(|(member, source)| {
        member.knowledge_cutoff.as_ref() == Some(first)
            && source
                .knowledge_cutoff
                .is_some_and(CapabilitySource::is_runtime_trusted)
    });
    all_same_trusted.then(|| first.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(id: &str, ctx: Option<u32>, out: Option<u32>) -> ModelSpec {
        ModelSpec {
            context_window: ctx,
            max_output_tokens: out,
            ..ModelSpec::new(id, "provider", format!("{id}-upstream"))
        }
    }

    fn trusted_cutoff_source() -> ModelCapabilitySources {
        ModelCapabilitySources {
            knowledge_cutoff: Some(CapabilitySource::ExplicitSpec),
            ..ModelCapabilitySources::default()
        }
    }

    #[test]
    fn reconciled_id_is_pool_id() {
        let spec = reconcile_pool_capabilities("my-pool", &[model_with("a", None, None)]);
        assert_eq!(spec.id, "my-pool");
    }

    #[test]
    fn context_window_is_minimum_known_bound() {
        let members = [
            model_with("a", Some(200_000), Some(8_000)),
            model_with("b", Some(100_000), Some(4_000)),
        ];
        let spec = reconcile_pool_capabilities("pool", &members);
        assert_eq!(spec.context_window, Some(100_000));
        assert_eq!(spec.max_output_tokens, Some(4_000));
    }

    #[test]
    fn unknown_member_bound_makes_pool_bound_unknown() {
        let members = [
            model_with("a", Some(128_000), Some(8_000)),
            model_with("b", None, None),
        ];
        let spec = reconcile_pool_capabilities("pool", &members);
        assert_eq!(spec.context_window, None);
        assert_eq!(spec.max_output_tokens, None);
    }

    #[test]
    fn all_unknown_bounds_yield_none() {
        let members = [model_with("a", None, None), model_with("b", None, None)];
        let spec = reconcile_pool_capabilities("pool", &members);
        assert_eq!(spec.context_window, None);
        assert_eq!(spec.max_output_tokens, None);
    }

    #[test]
    fn capability_metadata_without_runtime_effect_is_unset() {
        let members = [model_with("a", Some(1000), Some(500))];
        let spec = reconcile_pool_capabilities("pool", &members);
        assert!(spec.modalities.input.is_empty() && spec.modalities.output.is_empty());
        assert_eq!(spec.knowledge_cutoff, None);
        assert_eq!(spec.input_token_price_per_million_usd, None);
    }

    #[test]
    fn common_trusted_member_cutoff_is_reconciled_for_pool_runtime() {
        let mut a = model_with("a", Some(1000), Some(500));
        a.knowledge_cutoff = Some("2025-01".into());
        let mut b = model_with("b", Some(1000), Some(500));
        b.knowledge_cutoff = Some("2025-01".into());
        let (spec, sources) = reconcile_pool_capabilities_with_sources(
            "pool",
            &[a, b],
            &[trusted_cutoff_source(), trusted_cutoff_source()],
        );

        assert_eq!(spec.knowledge_cutoff.as_deref(), Some("2025-01"));
        assert_eq!(
            sources.knowledge_cutoff,
            Some(CapabilitySource::ExplicitSpec)
        );
    }

    #[test]
    fn static_or_divergent_member_cutoffs_are_not_reconciled() {
        let mut a = model_with("a", Some(1000), Some(500));
        a.knowledge_cutoff = Some("2025-01".into());
        let mut b = model_with("b", Some(1000), Some(500));
        b.knowledge_cutoff = Some("2025-02".into());

        let (divergent, divergent_sources) = reconcile_pool_capabilities_with_sources(
            "pool",
            &[a.clone(), b],
            &[trusted_cutoff_source(), trusted_cutoff_source()],
        );
        assert_eq!(divergent.knowledge_cutoff, None);
        assert_eq!(divergent_sources.knowledge_cutoff, None);

        let static_source = ModelCapabilitySources {
            knowledge_cutoff: Some(CapabilitySource::StaticHeuristic),
            ..ModelCapabilitySources::default()
        };
        let (static_only, static_sources) =
            reconcile_pool_capabilities_with_sources("pool", &[a], &[static_source]);
        assert_eq!(static_only.knowledge_cutoff, None);
        assert_eq!(static_sources.knowledge_cutoff, None);
    }

    #[test]
    fn breaker_key_changes_when_pool_members_change() {
        let mut pool = ModelPoolSpec::new("pool", ["a", "b"]);
        let members = [
            model_with("a", Some(1000), Some(500)),
            model_with("b", Some(1000), Some(500)),
        ];
        let signatures = vec!["provider-a".into(), "provider-b".into()];
        let first = pool_breaker_key(&pool, &members, &signatures);

        pool.members[1].model_id = "c".into();
        let changed_pool = pool_breaker_key(&pool, &members, &signatures);
        assert_ne!(first, changed_pool);

        let mut changed_member = [
            model_with("a", Some(1000), Some(500)),
            model_with("b", Some(1000), Some(500)),
        ];
        changed_member[1].upstream_model = "other".into();
        assert_ne!(
            first,
            pool_breaker_key(
                &ModelPoolSpec::new("pool", ["a", "b"]),
                &changed_member,
                &signatures
            )
        );
    }

    #[test]
    fn breaker_cache_evicts_least_recently_used_when_over_capacity() {
        let mut cache = BoundedBreakerCache::default();

        // Fill to capacity.
        for i in 0..MAX_POOL_BREAKERS {
            cache.get_or_insert(&format!("key-{i}"));
        }
        assert_eq!(cache.entries.len(), MAX_POOL_BREAKERS);

        // Touch key-0 so it becomes the most-recently-used; key-1 is now the LRU.
        let survivor = cache.get_or_insert("key-0");

        // One more distinct key forces a single eviction of the LRU (key-1).
        cache.get_or_insert("over-cap");

        assert_eq!(
            cache.entries.len(),
            MAX_POOL_BREAKERS,
            "cache must stay bounded under key churn"
        );
        assert!(
            !cache.entries.contains_key("key-1"),
            "the least-recently-used key must be evicted"
        );
        assert!(
            cache.entries.contains_key("key-0"),
            "a recently-used key must survive eviction"
        );
        assert!(cache.entries.contains_key("over-cap"));

        // The retained breaker is the same instance (identity preserved on hit).
        assert!(Arc::ptr_eq(&survivor, &cache.get_or_insert("key-0")));
    }

    #[test]
    fn breaker_cache_returns_same_breaker_for_repeated_key() {
        let mut cache = BoundedBreakerCache::default();
        let first = cache.get_or_insert("stable");
        let second = cache.get_or_insert("stable");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn breaker_key_changes_when_provider_signature_changes() {
        let pool = ModelPoolSpec::new("pool", ["a"]);
        let members = [model_with("a", Some(1000), Some(500))];

        assert_ne!(
            pool_breaker_key(&pool, &members, &["endpoint-a".into()]),
            pool_breaker_key(&pool, &members, &["endpoint-b".into()])
        );
    }
}
