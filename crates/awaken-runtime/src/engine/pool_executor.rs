//! [`PoolExecutor`] presents a model pool through the single-model
//! [`LlmExecutor`] contract.
//!
//! The pool pins each routed session to one member for prompt-cache affinity,
//! while a shared [`CircuitBreaker`] carries member health across sessions.
//! Switching stays conservative: quota and permanent member errors may fail
//! over within the same call, transient failures are left to provider retry,
//! and request-level errors never switch because they would fail on every
//! member.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor,
};
use awaken_runtime_contract::contract::inference::StreamResult;
use awaken_runtime_contract::registry_spec::HomeStrategy;

use super::circuit_breaker::CircuitBreaker;
use super::pool_router::PoolRouter;

const MAX_POOL_SESSION_STATES: usize = 4096;
const MAX_POOL_STREAM_ATTEMPTS: usize = 4096;

#[path = "pool_executor_llm.rs"]
mod pool_executor_llm;
#[path = "pool_observed_stream.rs"]
mod pool_observed_stream;

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
    inner: Arc<PoolExecutorInner>,
}

struct PoolExecutorInner {
    pool_id: String,
    fallback_home_key: String,
    members: Vec<PoolMemberExecutor>,
    router: PoolRouter,
    breaker: Arc<CircuitBreaker>,
    sessions: parking_lot::RwLock<HashMap<String, PoolSessionState>>,
    stream_attempts: parking_lot::RwLock<HashMap<String, PoolStreamAttemptState>>,
    permanent_quarantine: parking_lot::RwLock<Vec<bool>>,
    home_sequence: AtomicU64,
    anonymous_session_sequence: AtomicU64,
    stream_attempt_sequence: AtomicUsize,
    session_sequence: AtomicUsize,
}

#[derive(Debug, Clone, Copy, Default)]
struct PoolSessionState {
    active: Option<usize>,
    switch_count: u32,
    /// Monotonic access stamp for LRU eviction, mirroring
    /// [`PoolStreamAttemptState::last_access`]. Bumped on every read/touch so
    /// `ensure_session_capacity` can evict the least-recently-used session.
    last_access: usize,
}

#[derive(Debug, Clone)]
struct PoolStreamAttemptState {
    active: Option<usize>,
    tried: Vec<bool>,
    failure_observed: bool,
    in_flight: bool,
    live_streams: usize,
    request_callbacks_ambiguous: bool,
    last_access: usize,
}

impl PoolStreamAttemptState {
    fn new(member_count: usize) -> Self {
        Self {
            active: None,
            tried: vec![false; member_count],
            failure_observed: false,
            in_flight: false,
            live_streams: 0,
            request_callbacks_ambiguous: false,
            last_access: 0,
        }
    }
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
        let member_count = members.len();
        Self {
            inner: Arc::new(PoolExecutorInner {
                pool_id: pool_id.into(),
                fallback_home_key: home_key.into(),
                members,
                router,
                breaker,
                sessions: parking_lot::RwLock::new(HashMap::new()),
                stream_attempts: parking_lot::RwLock::new(HashMap::new()),
                permanent_quarantine: parking_lot::RwLock::new(vec![false; member_count]),
                home_sequence: AtomicU64::new(0),
                anonymous_session_sequence: AtomicU64::new(0),
                stream_attempt_sequence: AtomicUsize::new(0),
                session_sequence: AtomicUsize::new(0),
            }),
        }
    }
}

impl PoolExecutorInner {
    /// Health of each member by index, honoring `on_circuit_open`: when the
    /// policy ignores circuit state, every member reads as healthy so the
    /// breaker never drives a switch.
    fn health_mask(&self) -> Vec<bool> {
        let quarantined = self.permanent_quarantine.read().clone();
        if self.router.switch_policy().on_circuit_open {
            self.members
                .iter()
                .enumerate()
                .map(|(idx, m)| {
                    !quarantined.get(idx).copied().unwrap_or(false)
                        && self.breaker.is_available(&m.model_id)
                })
                .collect()
        } else {
            (0..self.members.len())
                .map(|idx| !quarantined.get(idx).copied().unwrap_or(false))
                .collect()
        }
    }

    fn switches_remain(&self, switch_count: u32) -> bool {
        match self.router.switch_policy().max_switches_per_session {
            Some(max) => switch_count < max,
            None => true,
        }
    }

    fn session_key(&self, request: &InferenceRequest) -> String {
        let routing_key = request.routing_key.as_ref();
        if let Some(key) = routing_key.and_then(|key| key.for_scope(self.router.sticky_scope())) {
            return key;
        }
        // A request with only a logical_inference_id (no thread/run/fallback)
        // still needs a *stable* key: external stream-failure callbacks
        // recompute it to relocate the originating attempt, which an anonymous
        // per-call key would defeat — dropping the failure.
        if let Some(logical_id) = routing_key.and_then(|key| key.logical_inference_id.as_deref()) {
            return format!("{}\0logical\0{logical_id}", self.fallback_home_key);
        }
        let sequence = self
            .anonymous_session_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        tracing::warn!(
            pool = %self.pool_id,
            sticky_scope = ?self.router.sticky_scope(),
            "model pool request missing routing key; using anonymous session"
        );
        format!("{}\0anonymous\0{sequence}", self.fallback_home_key)
    }

    fn ensure_session_capacity(sessions: &mut HashMap<String, PoolSessionState>, current: &str) {
        if sessions.len() < MAX_POOL_SESSION_STATES || sessions.contains_key(current) {
            return;
        }
        // Evict the least-recently-accessed session (never `current`), mirroring
        // `ensure_stream_attempt_capacity`. HashMap iteration order is arbitrary,
        // so picking by `last_access` keeps hot sessions resident instead of
        // dropping whichever key the map happened to surface first.
        let victim = sessions
            .iter()
            .filter(|(key, _)| key.as_str() != current)
            .min_by_key(|(_, state)| state.last_access)
            .map(|(key, _)| key.clone());
        if let Some(victim) = victim {
            sessions.remove(&victim);
        }
    }

    fn next_session_sequence(&self) -> usize {
        self.session_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    fn ensure_stream_attempt_capacity(
        attempts: &mut HashMap<String, PoolStreamAttemptState>,
        current: &str,
    ) -> bool {
        if attempts.len() < MAX_POOL_STREAM_ATTEMPTS || attempts.contains_key(current) {
            return true;
        }
        let victim = attempts
            .iter()
            .filter(|(key, attempt)| key.as_str() != current && !attempt.in_flight)
            .min_by_key(|(_, attempt)| attempt.last_access)
            .map(|(key, _)| key.clone());
        if let Some(victim) = victim {
            attempts.remove(&victim);
            true
        } else {
            false
        }
    }

    fn next_stream_attempt_sequence(&self) -> usize {
        self.stream_attempt_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    fn stream_attempt_key(&self, session_key: &str, request: &InferenceRequest) -> Option<String> {
        request
            .routing_key
            .as_ref()
            .and_then(|key| key.logical_inference_id.as_ref())
            .map(|id| format!("{session_key}\0{id}"))
    }

    fn stream_tried_mask(&self, attempt_key: Option<&str>) -> Vec<bool> {
        let Some(attempt_key) = attempt_key else {
            return vec![false; self.members.len()];
        };
        self.stream_attempts
            .write()
            .get_mut(attempt_key)
            .map(|attempt| {
                attempt.last_access = self.next_stream_attempt_sequence();
                attempt.tried.clone()
            })
            .unwrap_or_else(|| vec![false; self.members.len()])
    }

    fn merge_stream_tried_mask(&self, tried: &mut [bool], attempt_key: Option<&str>) {
        let cached = self.stream_tried_mask(attempt_key);
        for (attempted, cached_attempted) in tried.iter_mut().zip(cached) {
            *attempted |= cached_attempted;
        }
    }

    fn mark_stream_active(&self, attempt_key: Option<&str>, idx: usize) -> bool {
        let Some(attempt_key) = attempt_key else {
            return false;
        };
        let mut attempts = self.stream_attempts.write();
        if !Self::ensure_stream_attempt_capacity(&mut attempts, attempt_key) {
            tracing::debug!(
                pool = %self.pool_id,
                attempt_key,
                member = %self.members[idx].model_id,
                "model pool stream attempt cache full; recording stream terminal events by captured member only"
            );
            return false;
        }
        let attempt = attempts
            .entry(attempt_key.to_string())
            .or_insert_with(|| PoolStreamAttemptState::new(self.members.len()));
        attempt.last_access = self.next_stream_attempt_sequence();
        if idx < attempt.tried.len() {
            attempt.tried[idx] = true;
        }
        attempt.active = Some(idx);
        attempt.failure_observed = false;
        attempt.request_callbacks_ambiguous |= attempt.live_streams > 0;
        attempt.live_streams = attempt.live_streams.saturating_add(1);
        attempt.in_flight = true;
        true
    }

    fn mark_stream_open_failure(&self, attempt_key: Option<&str>, idx: usize) {
        let Some(attempt_key) = attempt_key else {
            return;
        };
        let mut attempts = self.stream_attempts.write();
        if !Self::ensure_stream_attempt_capacity(&mut attempts, attempt_key) {
            tracing::debug!(
                pool = %self.pool_id,
                attempt_key,
                member = %self.members[idx].model_id,
                "model pool stream attempt cache full; keeping stream-open failure in local tried mask only"
            );
            return;
        }
        let attempt = attempts
            .entry(attempt_key.to_string())
            .or_insert_with(|| PoolStreamAttemptState::new(self.members.len()));
        attempt.last_access = self.next_stream_attempt_sequence();
        if idx < attempt.tried.len() {
            attempt.tried[idx] = true;
        }
        attempt.active = Some(idx);
        attempt.failure_observed = true;
        attempt.in_flight = attempt.live_streams > 0;
    }

    fn home_sequence_for_new_session(&self) -> Option<u64> {
        matches!(self.router.home_strategy(), HomeStrategy::RoundRobin)
            .then(|| self.home_sequence.fetch_add(1, Ordering::Relaxed))
    }

    /// Resolve the active member for this session: home on first use, then a
    /// session-level failover if the active member's breaker has since opened.
    fn select_active(&self, session_key: &str) -> usize {
        let health = self.health_mask();
        let access = self.next_session_sequence();
        let mut sessions = self.sessions.write();
        Self::ensure_session_capacity(&mut sessions, session_key);
        let state = sessions.entry(session_key.to_string()).or_default();
        state.last_access = access;
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
                    && self.switches_remain(state.switch_count)
                    && let Some(next) = self.router.select_failover(session_key, current, &health)
                {
                    self.record_switch(current, next, "member unavailable");
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
        if !self.router.should_switch_on_error(err) {
            return None;
        }
        let mut mask = self.health_mask();
        for (i, attempted) in tried.iter().enumerate() {
            if *attempted {
                mask[i] = false;
            }
        }
        let access = self.next_session_sequence();
        let mut sessions = self.sessions.write();
        Self::ensure_session_capacity(&mut sessions, session_key);
        let state = sessions.entry(session_key.to_string()).or_default();
        if state.active != Some(current) {
            state.last_access = access;
            return state
                .active
                .filter(|active| mask.get(*active).copied().unwrap_or(false));
        }
        if !self.switches_remain(state.switch_count) {
            state.last_access = access;
            return None;
        }
        let next = self.router.select_failover(session_key, current, &mask)?;
        self.record_switch(current, next, "error-driven");
        state.last_access = access;
        state.switch_count += 1;
        state.active = Some(next);
        Some(next)
    }

    fn next_on_unavailable(
        &self,
        session_key: &str,
        current: usize,
        tried: &[bool],
    ) -> Option<usize> {
        let mut mask = self.health_mask();
        for (i, attempted) in tried.iter().enumerate() {
            if *attempted {
                mask[i] = false;
            }
        }
        let access = self.next_session_sequence();
        let mut sessions = self.sessions.write();
        Self::ensure_session_capacity(&mut sessions, session_key);
        let state = sessions.entry(session_key.to_string()).or_default();
        if state.active != Some(current) {
            state.last_access = access;
            return state
                .active
                .filter(|active| mask.get(*active).copied().unwrap_or(false));
        }
        if !self.switches_remain(state.switch_count) {
            state.last_access = access;
            return None;
        }
        let next = self.router.select_failover(session_key, current, &mask)?;
        self.record_switch(current, next, "member unavailable");
        state.last_access = access;
        state.switch_count += 1;
        state.active = Some(next);
        Some(next)
    }

    fn check_member(&self, idx: usize) -> Result<(), InferenceExecutionError> {
        if self
            .permanent_quarantine
            .read()
            .get(idx)
            .copied()
            .unwrap_or(false)
        {
            return Err(InferenceExecutionError::Provider(format!(
                "model pool member {} is quarantined",
                self.members[idx].model_id
            )));
        }
        if self.router.switch_policy().on_circuit_open {
            self.breaker.check(&self.members[idx].model_id)
        } else {
            Ok(())
        }
    }

    fn error_driven_no_member_available_error(
        &self,
        fallback: InferenceExecutionError,
        tried: &[bool],
    ) -> InferenceExecutionError {
        if (0..self.members.len()).all(|idx| tried.get(idx).copied().unwrap_or(false)) {
            return InferenceExecutionError::PoolAttemptsExhausted;
        }
        self.no_member_available_error(fallback, tried)
    }

    fn no_member_available_error(
        &self,
        fallback: InferenceExecutionError,
        tried: &[bool],
    ) -> InferenceExecutionError {
        let health = self.health_mask();
        if !health.iter().any(|available| *available) {
            return InferenceExecutionError::AllModelsUnavailable;
        }
        let exhausted_untried = health
            .iter()
            .enumerate()
            .all(|(idx, available)| !*available || tried.get(idx).copied().unwrap_or(false));
        if exhausted_untried {
            InferenceExecutionError::PoolAttemptsExhausted
        } else {
            fallback
        }
    }

    fn reset_switch_budget(&self, session_key: &str) {
        let access = self.next_session_sequence();
        if let Some(state) = self.sessions.write().get_mut(session_key) {
            state.last_access = access;
            state.switch_count = 0;
        }
    }

    fn record_stream_member_failure(
        &self,
        request: &InferenceRequest,
        err: &InferenceExecutionError,
    ) {
        let session_key = self.session_key(request);
        let attempt_key = self.stream_attempt_key(&session_key, request);
        // A stream failure must be attributed to the member that actually
        // opened the stream, recorded in this attempt's `active` slot by
        // `mark_stream_active`. The precise per-item path
        // (`PoolObservedStream`) already records failures the inner stream
        // yields; the outer call only adds failures that path cannot see
        // (e.g. an idle stall where `next()` times out without an `Err`).
        //
        // If the originating member cannot be precisely resolved — no
        // attempt key, or the key is absent from the cache (the stream was
        // never opened through this pool, or the attempt was already
        // evicted) — we must NOT fall back to the currently active member:
        // a session-level failover may have moved `active` off the member
        // that opened the stream, so recording there would mis-attribute the
        // failure onto an innocent member's breaker. Skip recording instead.
        let Some(current) = attempt_key.as_deref().and_then(|key| {
            self.stream_attempts.read().get(key).and_then(|attempt| {
                if attempt.request_callbacks_ambiguous {
                    tracing::debug!(
                        pool = %self.pool_id,
                        attempt_key = key,
                        "model pool skipped ambiguous stream failure callback"
                    );
                    return None;
                }
                attempt.active
            })
        }) else {
            tracing::debug!(
                pool = %self.pool_id,
                has_attempt_key = attempt_key.is_some(),
                "model pool skipped stale stream failure callback"
            );
            return;
        };
        self.record_stream_attempt_failure_once(
            &session_key,
            attempt_key.as_deref(),
            current,
            err,
            false,
        );
    }

    fn record_stream_attempt_failure(
        &self,
        session_key: &str,
        attempt_key: Option<&str>,
        current: usize,
        err: &InferenceExecutionError,
    ) {
        self.record_failure(current, err);
        let tried = if let Some(attempt_key) = attempt_key {
            let mut attempts = self.stream_attempts.write();
            if Self::ensure_stream_attempt_capacity(&mut attempts, attempt_key) {
                let attempt = attempts
                    .entry(attempt_key.to_string())
                    .or_insert_with(|| PoolStreamAttemptState::new(self.members.len()));
                attempt.last_access = self.next_stream_attempt_sequence();
                if current < attempt.tried.len() {
                    attempt.tried[current] = true;
                }
                attempt.active = Some(current);
                attempt.in_flight = attempt.live_streams > 0;
                attempt.tried.clone()
            } else {
                let mut tried = vec![false; self.members.len()];
                tried[current] = true;
                tried
            }
        } else {
            let mut tried = vec![false; self.members.len()];
            tried[current] = true;
            tried
        };
        if self.router.should_switch_on_error(err) {
            let _ = self.next_on_error(session_key, current, err, &tried);
        } else if self.router.switch_policy().on_circuit_open
            && !self.breaker.is_available(&self.members[current].model_id)
        {
            let _ = self.next_on_unavailable(session_key, current, &tried);
        }
    }

    fn record_stream_attempt_success(
        &self,
        session_key: &str,
        attempt_key: Option<&str>,
        current: usize,
    ) {
        if let Some(attempt_key) = attempt_key {
            let mut attempts = self.stream_attempts.write();
            let Some(attempt) = attempts.get_mut(attempt_key) else {
                return;
            };
            attempt.live_streams = attempt.live_streams.saturating_sub(1);
            attempt.in_flight = attempt.live_streams > 0;
            if attempt.active != Some(current) {
                return;
            }
            attempts.remove(attempt_key);
        }
        self.breaker.record_success(&self.members[current].model_id);
        self.reset_switch_budget(session_key);
    }

    fn record_stream_attempt_failure_once(
        &self,
        session_key: &str,
        attempt_key: Option<&str>,
        current: usize,
        err: &InferenceExecutionError,
        stream_finished: bool,
    ) {
        if let Some(attempt_key) = attempt_key {
            let mut attempts = self.stream_attempts.write();
            let Some(attempt) = attempts.get_mut(attempt_key) else {
                return;
            };
            attempt.last_access = self.next_stream_attempt_sequence();
            if stream_finished {
                attempt.live_streams = attempt.live_streams.saturating_sub(1);
                attempt.in_flight = attempt.live_streams > 0;
            }
            if attempt.active != Some(current) {
                return;
            }
            if current < attempt.tried.len() {
                attempt.tried[current] = true;
            }
            let duplicate = attempt.failure_observed;
            attempt.failure_observed = true;
            attempt.in_flight = attempt.live_streams > 0;
            drop(attempts);
            if duplicate {
                return;
            }
        }
        self.record_stream_attempt_failure(session_key, attempt_key, current, err);
    }

    fn record_stream_attempt_abandoned(&self, attempt_key: Option<&str>, current: usize) {
        if let Some(attempt_key) = attempt_key {
            let mut attempts = self.stream_attempts.write();
            let Some(attempt) = attempts.get_mut(attempt_key) else {
                return;
            };
            attempt.live_streams = attempt.live_streams.saturating_sub(1);
            attempt.in_flight = attempt.live_streams > 0;
            if attempt.active != Some(current) {
                return;
            }
            attempt.last_access = self.next_stream_attempt_sequence();
            if !attempt.failure_observed {
                attempts.remove(attempt_key);
            }
        }
        self.breaker
            .record_abandoned_probe(&self.members[current].model_id);
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
        if self.router.switch_policy().on_permanent
            && Self::is_member_permanent_error(err)
            && let Some(quarantined) = self.permanent_quarantine.write().get_mut(idx)
        {
            *quarantined = true;
        }
        if err.counts_toward_circuit_breaker() {
            self.breaker.record_failure(&self.members[idx].model_id);
        }
    }

    fn is_member_permanent_error(err: &InferenceExecutionError) -> bool {
        matches!(
            err,
            InferenceExecutionError::Unauthorized(_) | InferenceExecutionError::ModelNotFound(_)
        )
    }

    fn request_for(&self, idx: usize, base: &InferenceRequest) -> InferenceRequest {
        let mut req = base.clone();
        req.upstream_model = self.members[idx].upstream_model.clone();
        req
    }
}

#[cfg(test)]
#[path = "pool_executor_stale_stream_tests.rs"]
mod pool_executor_stale_stream_tests;
#[cfg(test)]
#[path = "pool_executor_stream_tests.rs"]
mod pool_executor_stream_tests;
#[cfg(test)]
#[path = "pool_executor_test_support.rs"]
mod pool_executor_test_support;
#[cfg(test)]
#[path = "pool_executor_tests.rs"]
mod pool_executor_tests;
