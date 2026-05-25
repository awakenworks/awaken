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
    let breaker = pool_breaker(&pool.id);
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
fn pool_breaker(pool_id: &str) -> Arc<CircuitBreaker> {
    static BREAKERS: OnceLock<Mutex<HashMap<String, Arc<CircuitBreaker>>>> = OnceLock::new();
    let breakers = BREAKERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = breakers.lock().expect("pool breaker registry poisoned");
    guard
        .entry(pool_id.to_string())
        .or_insert_with(|| Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default())))
        .clone()
}

/// Synthesize the [`ModelSpec`] the runtime should treat the pool as, so the
/// context-window policy clamps consistently regardless of which member serves
/// a request.
///
/// Only the capability fields with runtime behavior are reconciled:
/// - `context_window` / `max_output_tokens`: the **minimum** known bound across
///   members (a `None` member is unknown and ignored), so the clamp never
///   exceeds the smallest member's window.
///
/// Modalities, knowledge cutoff, and pricing are left unset: they have no
/// runtime effect today and cannot be soundly attributed to a single member.
pub fn reconcile_pool_capabilities(pool_id: &str, members: &[ModelSpec]) -> ModelSpec {
    let min_known = |f: fn(&ModelSpec) -> Option<u32>| members.iter().filter_map(f).min();
    ModelSpec {
        context_window: min_known(|m| m.context_window),
        max_output_tokens: min_known(|m| m.max_output_tokens),
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
    fn unknown_member_bound_is_ignored() {
        let members = [
            model_with("a", Some(128_000), Some(8_000)),
            model_with("b", None, None),
        ];
        let spec = reconcile_pool_capabilities("pool", &members);
        assert_eq!(spec.context_window, Some(128_000));
        assert_eq!(spec.max_output_tokens, Some(8_000));
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
}
