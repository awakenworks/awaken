//! Pool resolution: turn a [`ModelPoolSpec`] into a [`PoolExecutor`] presenting
//! the single-model contract, plus capability reconciliation so the pool clamps
//! context like a model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::registry_spec::{ModelPoolSpec, ModelSpec};

use crate::engine::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::engine::pool_executor::{PoolExecutor, PoolMemberExecutor};
use crate::engine::pool_router::{PoolRouter, RouterMember};
use crate::engine::retry::{LlmRetryPolicy, RetryingExecutor};
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
) -> Result<(Arc<dyn LlmExecutor>, String, ModelSpec), ResolveError> {
    let mut router_members = Vec::with_capacity(pool.members.len());
    let mut member_execs = Vec::with_capacity(pool.members.len());
    let mut member_specs = Vec::with_capacity(pool.members.len());

    for member in &pool.members {
        let model = registries
            .models
            .get_model(&member.model_id)
            .ok_or_else(|| ResolveError::ModelNotFound(member.model_id.clone()))?;
        let executor = registries
            .providers
            .get_provider(&model.provider_id)
            .ok_or_else(|| ResolveError::ProviderNotFound(model.provider_id.clone()))?;
        let executor = if policy.max_retries > 0 {
            Arc::new(RetryingExecutor::new(executor, policy.clone())) as Arc<dyn LlmExecutor>
        } else {
            executor
        };

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
    }

    let reconciled = reconcile_pool_capabilities(&pool.id, &member_specs);
    let router = PoolRouter::new(router_members, pool.routing.clone(), pool.switch.clone());
    let breaker = pool_breaker(&pool_breaker_key(pool, &member_specs));
    let upstream_stand_in = reconciled.upstream_model.clone();
    let executor: Arc<dyn LlmExecutor> = Arc::new(PoolExecutor::new(
        pool.id.clone(),
        home_key,
        member_execs,
        router,
        breaker,
    ));
    Ok((executor, upstream_stand_in, reconciled))
}

/// Process-shared circuit breaker for a pool, created on first use. Sharing the
/// breaker across resolutions gives member health cross-session memory: while a
/// member is unhealthy every session avoids it, and sessions return once it
/// heals. Breakers reset on process restart.
fn pool_breaker(key: &str) -> Arc<CircuitBreaker> {
    static BREAKERS: OnceLock<Mutex<HashMap<String, Arc<CircuitBreaker>>>> = OnceLock::new();
    let breakers = BREAKERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = breakers.lock().expect("pool breaker registry poisoned");
    guard
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default())))
        .clone()
}

fn pool_breaker_key(pool: &ModelPoolSpec, members: &[ModelSpec]) -> String {
    let mut input = serde_json::to_string(pool).unwrap_or_else(|_| pool.id.clone());
    for model in members {
        input.push('\n');
        input.push_str(&model.id);
        input.push('\t');
        input.push_str(&model.provider_id);
        input.push('\t');
        input.push_str(&model.upstream_model);
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
/// Modalities, knowledge cutoff, and pricing are left unset: they have no
/// runtime effect today and cannot be soundly attributed to a single member.
pub fn reconcile_pool_capabilities(pool_id: &str, members: &[ModelSpec]) -> ModelSpec {
    let min_declared = |f: fn(&ModelSpec) -> Option<u32>| {
        members
            .iter()
            .map(f)
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().min())
    };
    ModelSpec {
        context_window: min_declared(|m| m.context_window),
        max_output_tokens: min_declared(|m| m.max_output_tokens),
        ..ModelSpec::new(pool_id, pool_id, pool_id)
    }
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
    fn breaker_key_changes_when_pool_members_change() {
        let mut pool = ModelPoolSpec::new("pool", ["a", "b"]);
        let members = [
            model_with("a", Some(1000), Some(500)),
            model_with("b", Some(1000), Some(500)),
        ];
        let first = pool_breaker_key(&pool, &members);

        pool.members[1].model_id = "c".into();
        let changed_pool = pool_breaker_key(&pool, &members);
        assert_ne!(first, changed_pool);

        let mut changed_member = [
            model_with("a", Some(1000), Some(500)),
            model_with("b", Some(1000), Some(500)),
        ];
        changed_member[1].upstream_model = "other".into();
        assert_ne!(
            first,
            pool_breaker_key(&ModelPoolSpec::new("pool", ["a", "b"]), &changed_member)
        );
    }
}
