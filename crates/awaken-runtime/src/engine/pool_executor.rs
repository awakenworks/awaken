//! [`PoolExecutor`] — a model pool that presents the single-model
//! [`LlmExecutor`] contract.
//!
//! Resolution builds a `PoolExecutor` over the pool's member models, each
//! paired with its own resolved provider executor. Each inference request can
//! carry routing keys (the agent loop supplies thread and run ids), so the
//! [`PoolRouter`] pins the configured sticky scope to one member
//! (prompt-cache affinity); a shared
//! [`CircuitBreaker`] keyed by member model id carries failure memory across
//! sessions, so while a member is unhealthy every session avoids it and
//! returns to it once it heals.
//!
//! Switching is deliberately conservative to protect the upstream prompt
//! cache:
//! - **Quota** (`RateLimited`/`Overloaded`) and **permanent** member errors
//!   (`Unauthorized`/`ModelNotFound`) switch to another member within the same
//!   call, per [`PoolSwitchPolicy`].
//! - **Transient** failures are returned to the caller; the member's own retry
//!   policy absorbs blips, and repeated failures open the member's breaker so a
//!   later call re-homes off it (this is the "long-term failure" path).
//! - **Request-level** errors (`ContextOverflow`, `InvalidRequest`,
//!   `ContentFiltered`) and `Cancelled` never switch — they would fail
//!   identically on every member.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor,
};
use awaken_contract::contract::inference::StreamResult;
use awaken_contract::registry_spec::HomeStrategy;

use super::circuit_breaker::CircuitBreaker;
use super::pool_router::PoolRouter;

const MAX_POOL_SESSION_STATES: usize = 4096;

/// A resolved pool member paired with its concrete provider executor.
pub struct PoolMemberExecutor {
    /// Member `ModelSpec.id`; the circuit-breaker key and router member id.
    pub model_id: String,
    /// Member `ModelSpec.upstream_model`, written onto the request when this
    /// member serves it.
    pub upstream_model: String,
    /// The resolved provider executor (possibly already a `RetryingExecutor`).
    pub executor: Arc<dyn LlmExecutor>,
}

/// A model pool exposed as a single [`LlmExecutor`].
pub struct PoolExecutor {
    pool_id: String,
    fallback_home_key: String,
    members: Vec<PoolMemberExecutor>,
    router: PoolRouter,
    breaker: Arc<CircuitBreaker>,
    sessions: parking_lot::RwLock<HashMap<String, PoolSessionState>>,
    home_sequence: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
struct PoolSessionState {
    active: Option<usize>,
    switch_count: u32,
}

impl PoolExecutor {
    /// Build a pool executor. `members` and `router.members()` must align by
    /// index (same order). `home_key` is the fallback key used when a request
    /// has no routing key; `breaker` is shared across sessions of the pool.
    pub fn new(
        pool_id: impl Into<String>,
        home_key: impl Into<String>,
        members: Vec<PoolMemberExecutor>,
        router: PoolRouter,
        breaker: Arc<CircuitBreaker>,
    ) -> Self {
        debug_assert_eq!(
            members.len(),
            router.members().len(),
            "member executors must align with router members"
        );
        Self {
            pool_id: pool_id.into(),
            fallback_home_key: home_key.into(),
            members,
            router,
            breaker,
            sessions: parking_lot::RwLock::new(HashMap::new()),
            home_sequence: AtomicU64::new(0),
        }
    }

    /// Health of each member by index, honoring `on_circuit_open`: when the
    /// policy ignores circuit state, every member reads as healthy so the
    /// breaker never drives a switch.
    fn health_mask(&self) -> Vec<bool> {
        if self.router.switch_policy().on_circuit_open {
            self.members
                .iter()
                .map(|m| self.breaker.is_available(&m.model_id))
                .collect()
        } else {
            vec![true; self.members.len()]
        }
    }

    fn switches_remain(&self, switch_count: u32) -> bool {
        match self.router.switch_policy().max_switches_per_session {
            Some(max) => switch_count < max,
            None => true,
        }
    }

    fn session_key(&self, request: &InferenceRequest) -> String {
        let key = request
            .routing_key
            .as_ref()
            .and_then(|key| key.for_scope(self.router.sticky_scope()));
        key.unwrap_or_else(|| self.fallback_home_key.clone())
    }

    fn ensure_session_capacity(sessions: &mut HashMap<String, PoolSessionState>, current: &str) {
        if sessions.len() < MAX_POOL_SESSION_STATES || sessions.contains_key(current) {
            return;
        }
        if let Some(victim) = sessions.keys().find(|key| key.as_str() != current).cloned() {
            sessions.remove(&victim);
        }
    }

    fn home_sequence_for_new_session(&self) -> Option<u64> {
        matches!(self.router.home_strategy(), HomeStrategy::RoundRobin)
            .then(|| self.home_sequence.fetch_add(1, Ordering::Relaxed))
    }

    /// Resolve the active member for this session: home on first use, then a
    /// session-level failover if the active member's breaker has since opened.
    fn select_active(&self, session_key: &str) -> usize {
        let health = self.health_mask();
        let mut sessions = self.sessions.write();
        Self::ensure_session_capacity(&mut sessions, session_key);
        let state = sessions.entry(session_key.to_string()).or_default();
        match state.active {
            None => {
                let home = self.router.select_home_with_sequence(
                    session_key,
                    &health,
                    self.home_sequence_for_new_session(),
                );
                state.active = Some(home);
                home
            }
            Some(current) => {
                let unhealthy = health.get(current).copied() == Some(false);
                if unhealthy
                    && self.router.switch_policy().on_circuit_open
                    && self.switches_remain(state.switch_count)
                    && let Some(next) = self.router.select_failover(session_key, current, &health)
                {
                    self.record_switch(current, next, "circuit open");
                    state.switch_count += 1;
                    state.active = Some(next);
                    return next;
                }
                current
            }
        }
    }

    /// Decide the next member after `current` failed with `err`, if a switch is
    /// warranted, the budget allows, and an untried healthy alternative exists.
    /// Updates active + switch count. `tried` excludes members already attempted
    /// in this call so a single call cannot loop on the same members.
    fn next_on_error(
        &self,
        session_key: &str,
        current: usize,
        err: &InferenceExecutionError,
        tried: &[bool],
    ) -> Option<usize> {
        let current_switch_count = self
            .sessions
            .read()
            .get(session_key)
            .map(|state| state.switch_count)
            .unwrap_or_default();
        if !self.router.should_switch_on_error(err) || !self.switches_remain(current_switch_count) {
            return None;
        }
        let mut mask = self.health_mask();
        for (i, attempted) in tried.iter().enumerate() {
            if *attempted {
                mask[i] = false;
            }
        }
        let next = self.router.select_failover(session_key, current, &mask)?;
        self.record_switch(current, next, "error-driven");
        let mut sessions = self.sessions.write();
        Self::ensure_session_capacity(&mut sessions, session_key);
        let state = sessions.entry(session_key.to_string()).or_default();
        state.switch_count += 1;
        state.active = Some(next);
        Some(next)
    }

    fn reset_switch_budget(&self, session_key: &str) {
        if let Some(state) = self.sessions.write().get_mut(session_key) {
            state.switch_count = 0;
        }
    }

    fn record_stream_member_failure(
        &self,
        request: &InferenceRequest,
        err: &InferenceExecutionError,
    ) {
        let session_key = self.session_key(request);
        let current = self.select_active(&session_key);
        self.record_failure(current, err);
        let mut tried = vec![false; self.members.len()];
        tried[current] = true;
        let _ = self.next_on_error(&session_key, current, err, &tried);
    }

    fn record_switch(&self, from: usize, to: usize, reason: &str) {
        tracing::info!(
            pool = %self.pool_id,
            from = %self.members[from].model_id,
            to = %self.members[to].model_id,
            reason,
            "model pool switched member"
        );
    }

    fn record_failure(&self, idx: usize, err: &InferenceExecutionError) {
        if err.counts_toward_circuit_breaker() {
            self.breaker.record_failure(&self.members[idx].model_id);
        }
    }

    fn request_for(&self, idx: usize, base: &InferenceRequest) -> InferenceRequest {
        let mut req = base.clone();
        req.upstream_model = self.members[idx].upstream_model.clone();
        req
    }
}

#[async_trait]
impl LlmExecutor for PoolExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let session_key = self.session_key(&request);
        let mut idx = self.select_active(&session_key);
        let mut tried = vec![false; self.members.len()];
        loop {
            tried[idx] = true;
            let req = self.request_for(idx, &request);
            match self.members[idx].executor.execute(req).await {
                Ok(result) => {
                    self.breaker.record_success(&self.members[idx].model_id);
                    self.reset_switch_budget(&session_key);
                    return Ok(result);
                }
                Err(err) => {
                    self.record_failure(idx, &err);
                    match self.next_on_error(&session_key, idx, &err, &tried) {
                        Some(next) => idx = next,
                        None => return Err(err),
                    }
                }
            }
        }
    }

    fn execute_stream(
        &self,
        request: InferenceRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let session_key = self.session_key(&request);
            let mut idx = self.select_active(&session_key);
            let mut tried = vec![false; self.members.len()];
            loop {
                tried[idx] = true;
                let req = self.request_for(idx, &request);
                match self.members[idx].executor.execute_stream(req).await {
                    Ok(stream) => {
                        return Ok(stream);
                    }
                    Err(err) => {
                        self.record_failure(idx, &err);
                        match self.next_on_error(&session_key, idx, &err, &tried) {
                            Some(next) => idx = next,
                            None => return Err(err),
                        }
                    }
                }
            }
        })
    }

    fn name(&self) -> &str {
        &self.pool_id
    }

    fn supports_upstream_model_override(&self) -> bool {
        false
    }

    fn record_stream_success(&self, request: &InferenceRequest) {
        let session_key = self.session_key(request);
        let idx = self.select_active(&session_key);
        self.breaker.record_success(&self.members[idx].model_id);
        self.reset_switch_budget(&session_key);
    }

    fn record_stream_failure(&self, request: &InferenceRequest, err: &InferenceExecutionError) {
        self.record_stream_member_failure(request, err);
    }
}

#[cfg(test)]
#[path = "pool_executor_tests.rs"]
mod pool_executor_tests;
