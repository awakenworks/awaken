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
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use async_trait::async_trait;
use awaken_contract::contract::executor::InterruptCause;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, InterruptSnapshot, LlmExecutor,
    LlmStreamEvent,
};
use awaken_contract::contract::inference::StreamResult;
use awaken_contract::registry_spec::HomeStrategy;
use futures::Stream;

use super::circuit_breaker::CircuitBreaker;
use super::pool_router::PoolRouter;

const MAX_POOL_SESSION_STATES: usize = 4096;
const MAX_POOL_STREAM_ATTEMPTS: usize = 4096;

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
}

#[derive(Debug, Clone, Copy, Default)]
struct PoolSessionState {
    active: Option<usize>,
    switch_count: u32,
}

#[derive(Debug, Clone)]
struct PoolStreamAttemptState {
    active: Option<usize>,
    tried: Vec<bool>,
    failure_observed: bool,
}

impl PoolStreamAttemptState {
    fn new(member_count: usize) -> Self {
        Self {
            active: None,
            tried: vec![false; member_count],
            failure_observed: false,
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

    fn ensure_stream_attempt_capacity(
        attempts: &mut HashMap<String, PoolStreamAttemptState>,
        current: &str,
    ) {
        if attempts.len() < MAX_POOL_STREAM_ATTEMPTS || attempts.contains_key(current) {
            return;
        }
        if let Some(victim) = attempts.keys().find(|key| key.as_str() != current).cloned() {
            attempts.remove(&victim);
        }
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
            .read()
            .get(attempt_key)
            .map(|attempt| attempt.tried.clone())
            .unwrap_or_else(|| vec![false; self.members.len()])
    }

    fn mark_stream_active(&self, attempt_key: Option<&str>, idx: usize) {
        let Some(attempt_key) = attempt_key else {
            return;
        };
        let mut attempts = self.stream_attempts.write();
        Self::ensure_stream_attempt_capacity(&mut attempts, attempt_key);
        let attempt = attempts
            .entry(attempt_key.to_string())
            .or_insert_with(|| PoolStreamAttemptState::new(self.members.len()));
        if idx < attempt.tried.len() {
            attempt.tried[idx] = true;
        }
        attempt.active = Some(idx);
        attempt.failure_observed = false;
    }

    fn clear_stream_attempt(&self, attempt_key: Option<&str>) {
        if let Some(attempt_key) = attempt_key {
            self.stream_attempts.write().remove(attempt_key);
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

    fn next_on_unavailable(
        &self,
        session_key: &str,
        current: usize,
        tried: &[bool],
    ) -> Option<usize> {
        let current_switch_count = self
            .sessions
            .read()
            .get(session_key)
            .map(|state| state.switch_count)
            .unwrap_or_default();
        if !self.switches_remain(current_switch_count) {
            return None;
        }
        let mut mask = self.health_mask();
        for (i, attempted) in tried.iter().enumerate() {
            if *attempted {
                mask[i] = false;
            }
        }
        let next = self.router.select_failover(session_key, current, &mask)?;
        self.record_switch(current, next, "member unavailable");
        let mut sessions = self.sessions.write();
        Self::ensure_session_capacity(&mut sessions, session_key);
        let state = sessions.entry(session_key.to_string()).or_default();
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
        let attempt_key = self.stream_attempt_key(&session_key, request);
        let current = attempt_key
            .as_deref()
            .and_then(|key| self.stream_attempts.read().get(key).and_then(|a| a.active))
            .unwrap_or_else(|| self.select_active(&session_key));
        self.record_stream_attempt_failure_once(&session_key, attempt_key.as_deref(), current, err);
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
            Self::ensure_stream_attempt_capacity(&mut attempts, attempt_key);
            let attempt = attempts
                .entry(attempt_key.to_string())
                .or_insert_with(|| PoolStreamAttemptState::new(self.members.len()));
            if current < attempt.tried.len() {
                attempt.tried[current] = true;
            }
            attempt.active = Some(current);
            attempt.tried.clone()
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
        self.breaker.record_success(&self.members[current].model_id);
        self.reset_switch_budget(session_key);
        self.clear_stream_attempt(attempt_key);
    }

    fn record_stream_attempt_failure_once(
        &self,
        session_key: &str,
        attempt_key: Option<&str>,
        current: usize,
        err: &InferenceExecutionError,
    ) {
        if let Some(attempt_key) = attempt_key {
            let mut attempts = self.stream_attempts.write();
            Self::ensure_stream_attempt_capacity(&mut attempts, attempt_key);
            let attempt = attempts
                .entry(attempt_key.to_string())
                .or_insert_with(|| PoolStreamAttemptState::new(self.members.len()));
            if current < attempt.tried.len() {
                attempt.tried[current] = true;
            }
            let duplicate = attempt.active == Some(current) && attempt.failure_observed;
            attempt.active = Some(current);
            attempt.failure_observed = true;
            drop(attempts);
            if duplicate {
                return;
            }
        }
        self.record_stream_attempt_failure(session_key, attempt_key, current, err);
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
        if Self::is_member_permanent_error(err)
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

struct PoolObservedStream {
    inner: InferenceStream,
    pool: Arc<PoolExecutorInner>,
    session_key: String,
    attempt_key: Option<String>,
    member_idx: usize,
    finished: bool,
}

impl Stream for PoolObservedStream {
    type Item = Result<LlmStreamEvent, InferenceExecutionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Err(err))) => {
                this.finished = true;
                this.pool.record_stream_attempt_failure_once(
                    &this.session_key,
                    this.attempt_key.as_deref(),
                    this.member_idx,
                    &err,
                );
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                if !this.finished {
                    this.finished = true;
                    this.pool.record_stream_attempt_success(
                        &this.session_key,
                        this.attempt_key.as_deref(),
                        this.member_idx,
                    );
                }
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for PoolObservedStream {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.pool.record_stream_attempt_failure_once(
            &self.session_key,
            self.attempt_key.as_deref(),
            self.member_idx,
            &InferenceExecutionError::StreamInterrupted {
                cause: InterruptCause::IdleStall,
                snapshot: Box::new(InterruptSnapshot {
                    text: None,
                    completed_tool_calls: vec![],
                    in_flight_tool: None,
                    bytes_received: 0,
                }),
            },
        );
    }
}

#[async_trait]
impl LlmExecutor for PoolExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let inner = &self.inner;
        let session_key = inner.session_key(&request);
        let mut idx = inner.select_active(&session_key);
        let mut tried = vec![false; inner.members.len()];
        loop {
            tried[idx] = true;
            if let Err(err) = inner.check_member(idx) {
                match inner.next_on_unavailable(&session_key, idx, &tried) {
                    Some(next) => {
                        idx = next;
                        continue;
                    }
                    None => return Err(inner.no_member_available_error(err, &tried)),
                }
            }

            let req = inner.request_for(idx, &request);
            match inner.members[idx].executor.execute(req).await {
                Ok(result) => {
                    inner.breaker.record_success(&inner.members[idx].model_id);
                    inner.reset_switch_budget(&session_key);
                    return Ok(result);
                }
                Err(err) => {
                    inner.record_failure(idx, &err);
                    match inner.next_on_error(&session_key, idx, &err, &tried) {
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
            let inner = Arc::clone(&self.inner);
            let session_key = inner.session_key(&request);
            let attempt_key = inner.stream_attempt_key(&session_key, &request);
            let mut idx = inner.select_active(&session_key);
            let mut tried = inner.stream_tried_mask(attempt_key.as_deref());
            loop {
                tried[idx] = true;
                if let Err(err) = inner.check_member(idx) {
                    match inner.next_on_unavailable(&session_key, idx, &tried) {
                        Some(next) => {
                            idx = next;
                            continue;
                        }
                        None => return Err(inner.no_member_available_error(err, &tried)),
                    }
                }

                let req = inner.request_for(idx, &request);
                match inner.members[idx].executor.execute_stream(req).await {
                    Ok(stream) => {
                        inner.mark_stream_active(attempt_key.as_deref(), idx);
                        let observed = PoolObservedStream {
                            inner: stream,
                            pool: Arc::clone(&inner),
                            session_key,
                            attempt_key,
                            member_idx: idx,
                            finished: false,
                        };
                        return Ok(Box::pin(observed) as InferenceStream);
                    }
                    Err(err) => {
                        inner.record_failure(idx, &err);
                        if let Some(key) = attempt_key.as_deref() {
                            inner.mark_stream_active(Some(key), idx);
                        }
                        match inner.next_on_error(&session_key, idx, &err, &tried) {
                            Some(next) => {
                                idx = next;
                                tried = inner.stream_tried_mask(attempt_key.as_deref());
                            }
                            None => return Err(err),
                        }
                    }
                }
            }
        })
    }

    fn name(&self) -> &str {
        &self.inner.pool_id
    }

    fn supports_upstream_model_override(&self) -> bool {
        false
    }

    fn record_stream_success(&self, request: &InferenceRequest) {
        let session_key = self.inner.session_key(request);
        let attempt_key = self.inner.stream_attempt_key(&session_key, request);
        if let Some(idx) = attempt_key.as_deref().and_then(|key| {
            self.inner
                .stream_attempts
                .read()
                .get(key)
                .and_then(|attempt| attempt.active)
        }) {
            self.inner
                .breaker
                .record_success(&self.inner.members[idx].model_id);
            self.inner.reset_switch_budget(&session_key);
        }
        self.inner.clear_stream_attempt(attempt_key.as_deref());
    }

    fn record_stream_failure(&self, request: &InferenceRequest, err: &InferenceExecutionError) {
        self.inner.record_stream_member_failure(request, err);
    }
}

#[cfg(test)]
#[path = "pool_executor_stream_tests.rs"]
mod pool_executor_stream_tests;
#[cfg(test)]
#[path = "pool_executor_test_support.rs"]
mod pool_executor_test_support;
#[cfg(test)]
#[path = "pool_executor_tests.rs"]
mod pool_executor_tests;
