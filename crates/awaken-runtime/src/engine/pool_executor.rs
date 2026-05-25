//! [`PoolExecutor`] — a model pool that presents the single-model
//! [`LlmExecutor`] contract.
//!
//! Resolution builds one `PoolExecutor` per session over the pool's member
//! models, each paired with its own resolved provider executor. The agent id
//! is baked in as the home key, so the [`PoolRouter`] deterministically pins
//! the agent to one member (prompt-cache affinity); a shared
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor,
};
use awaken_contract::contract::inference::StreamResult;

use super::circuit_breaker::CircuitBreaker;
use super::pool_router::PoolRouter;

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
    home_key: String,
    members: Vec<PoolMemberExecutor>,
    router: PoolRouter,
    breaker: Arc<CircuitBreaker>,
    active: parking_lot::RwLock<Option<usize>>,
    switch_count: AtomicU32,
}

impl PoolExecutor {
    /// Build a pool executor. `members` and `router.members()` must align by
    /// index (same order). `home_key` is the agent id used for deterministic
    /// home selection; `breaker` is shared across sessions of the pool.
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
            home_key: home_key.into(),
            members,
            router,
            breaker,
            active: parking_lot::RwLock::new(None),
            switch_count: AtomicU32::new(0),
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

    fn switches_remain(&self) -> bool {
        match self.router.switch_policy().max_switches_per_session {
            Some(max) => self.switch_count.load(Ordering::SeqCst) < max,
            None => true,
        }
    }

    /// Resolve the active member for this session: home on first use, then a
    /// session-level failover if the active member's breaker has since opened.
    fn select_active(&self) -> usize {
        let health = self.health_mask();
        let mut guard = self.active.write();
        match *guard {
            None => {
                let home = self.router.select_home(&self.home_key, &health);
                *guard = Some(home);
                home
            }
            Some(current) => {
                let unhealthy = health.get(current).copied() == Some(false);
                if unhealthy
                    && self.router.switch_policy().on_circuit_open
                    && self.switches_remain()
                    && let Some(next) =
                        self.router
                            .select_failover(&self.home_key, current, &health)
                {
                    self.record_switch(current, next, "circuit open");
                    *guard = Some(next);
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
        current: usize,
        err: &InferenceExecutionError,
        tried: &[bool],
    ) -> Option<usize> {
        if !self.router.should_switch_on_error(err) || !self.switches_remain() {
            return None;
        }
        let mut mask = self.health_mask();
        for (i, attempted) in tried.iter().enumerate() {
            if *attempted {
                mask[i] = false;
            }
        }
        let next = self
            .router
            .select_failover(&self.home_key, current, &mask)?;
        self.record_switch(current, next, "error-driven");
        *self.active.write() = Some(next);
        Some(next)
    }

    fn record_switch(&self, from: usize, to: usize, reason: &str) {
        self.switch_count.fetch_add(1, Ordering::SeqCst);
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
        let mut idx = self.select_active();
        let mut tried = vec![false; self.members.len()];
        loop {
            tried[idx] = true;
            let req = self.request_for(idx, &request);
            match self.members[idx].executor.execute(req).await {
                Ok(result) => {
                    self.breaker.record_success(&self.members[idx].model_id);
                    return Ok(result);
                }
                Err(err) => {
                    self.record_failure(idx, &err);
                    match self.next_on_error(idx, &err, &tried) {
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
            let mut idx = self.select_active();
            let mut tried = vec![false; self.members.len()];
            loop {
                tried[idx] = true;
                let req = self.request_for(idx, &request);
                match self.members[idx].executor.execute_stream(req).await {
                    Ok(stream) => {
                        self.breaker.record_success(&self.members[idx].model_id);
                        return Ok(stream);
                    }
                    Err(err) => {
                        self.record_failure(idx, &err);
                        match self.next_on_error(idx, &err, &tried) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::circuit_breaker::CircuitBreakerConfig;
    use crate::engine::pool_router::RouterMember;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::inference::StopReason;
    use awaken_contract::contract::message::Message;
    use awaken_contract::registry_spec::{PoolMemberRole, PoolRoutingPolicy, PoolSwitchPolicy};

    fn ok_result() -> StreamResult {
        StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }
    }

    fn request() -> InferenceRequest {
        InferenceRequest {
            upstream_model: "pool-incoming".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    enum Behavior {
        AlwaysOk,
        AlwaysErr(InferenceExecutionError),
        FailTransientThenOk { fails: u32 },
    }

    struct StubExecutor {
        id: String,
        behavior: Behavior,
        calls: AtomicU32,
    }

    impl StubExecutor {
        fn new(id: &str, behavior: Behavior) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                behavior,
                calls: AtomicU32::new(0),
            })
        }

        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmExecutor for StubExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                Behavior::AlwaysOk => Ok(ok_result()),
                Behavior::AlwaysErr(err) => Err(err.clone()),
                Behavior::FailTransientThenOk { fails } => {
                    if n < *fails {
                        Err(InferenceExecutionError::Provider("transient".into()))
                    } else {
                        Ok(ok_result())
                    }
                }
            }
        }

        fn name(&self) -> &str {
            &self.id
        }
    }

    fn router_over(ids: &[String], switch: PoolSwitchPolicy) -> PoolRouter {
        let members = ids
            .iter()
            .map(|id| RouterMember {
                model_id: id.clone(),
                role: PoolMemberRole::Member,
                weight: 1,
            })
            .collect();
        PoolRouter::new(members, PoolRoutingPolicy::default(), switch)
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("m{i}")).collect()
    }

    /// Deterministic home index for `home_key` over `n` members.
    fn home_of(home_key: &str, n: usize) -> usize {
        router_over(&ids(n), PoolSwitchPolicy::default()).select_home(home_key, &vec![true; n])
    }

    /// Build a pool of `n` members where the home member runs `home_behavior`
    /// and every other member always succeeds. Returns the executor, the stub
    /// handles, and the home index.
    fn pool_home_fails(
        home_key: &str,
        n: usize,
        home_behavior: Behavior,
        switch: PoolSwitchPolicy,
        breaker: Arc<CircuitBreaker>,
    ) -> (PoolExecutor, Vec<Arc<StubExecutor>>, usize) {
        let member_ids = ids(n);
        let home = home_of(home_key, n);
        let mut home_behavior = Some(home_behavior);
        let mut stubs = Vec::new();
        let mut member_execs = Vec::new();
        for (i, id) in member_ids.iter().enumerate() {
            let behavior = if i == home {
                home_behavior.take().unwrap()
            } else {
                Behavior::AlwaysOk
            };
            let stub = StubExecutor::new(id, behavior);
            stubs.push(stub.clone());
            member_execs.push(PoolMemberExecutor {
                model_id: id.clone(),
                upstream_model: format!("{id}-upstream"),
                executor: stub as Arc<dyn LlmExecutor>,
            });
        }
        let router = router_over(&member_ids, switch);
        let pool = PoolExecutor::new("pool", home_key, member_execs, router, breaker);
        (pool, stubs, home)
    }

    /// Build a pool where every member runs the supplied behavior.
    fn pool_all(
        home_key: &str,
        behaviors: Vec<Behavior>,
        switch: PoolSwitchPolicy,
        breaker: Arc<CircuitBreaker>,
    ) -> (PoolExecutor, Vec<Arc<StubExecutor>>) {
        let member_ids = ids(behaviors.len());
        let mut stubs = Vec::new();
        let mut member_execs = Vec::new();
        for (id, behavior) in member_ids.iter().zip(behaviors) {
            let stub = StubExecutor::new(id, behavior);
            stubs.push(stub.clone());
            member_execs.push(PoolMemberExecutor {
                model_id: id.clone(),
                upstream_model: format!("{id}-upstream"),
                executor: stub as Arc<dyn LlmExecutor>,
            });
        }
        let router = router_over(&member_ids, switch);
        let pool = PoolExecutor::new("pool", home_key, member_execs, router, breaker);
        (pool, stubs)
    }

    fn breaker() -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default()))
    }

    fn breaker_threshold(n: u32) -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: n,
            ..CircuitBreakerConfig::default()
        }))
    }

    #[tokio::test]
    async fn routes_home_and_succeeds() {
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::AlwaysOk,
            PoolSwitchPolicy::default(),
            breaker(),
        );
        assert!(pool.execute(request()).await.is_ok());
        assert_eq!(stubs[home].call_count(), 1);
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert_eq!(others, 0, "only the home member runs");
    }

    #[tokio::test]
    async fn switches_to_other_member_on_quota() {
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::AlwaysErr(InferenceExecutionError::rate_limited("429")),
            PoolSwitchPolicy::default(),
            breaker(),
        );
        assert!(
            pool.execute(request()).await.is_ok(),
            "should switch off the quota-limited home member"
        );
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert!(others >= 1, "a fallback member served the request");
    }

    #[tokio::test]
    async fn switches_on_permanent_error() {
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::AlwaysErr(InferenceExecutionError::Unauthorized("401".into())),
            PoolSwitchPolicy::default(),
            breaker(),
        );
        assert!(pool.execute(request()).await.is_ok());
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert!(others >= 1);
    }

    #[tokio::test]
    async fn does_not_switch_on_transient_error() {
        let cb = breaker();
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::AlwaysErr(InferenceExecutionError::Provider("blip".into())),
            PoolSwitchPolicy::default(),
            cb.clone(),
        );
        let err = pool
            .execute(request())
            .await
            .expect_err("transient propagates");
        assert!(matches!(err, InferenceExecutionError::Provider(_)));
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert_eq!(others, 0, "transient must not switch members in-call");
        let _ = cb;
    }

    #[tokio::test]
    async fn does_not_switch_on_request_level_error() {
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::AlwaysErr(InferenceExecutionError::ContextOverflow("big".into())),
            PoolSwitchPolicy::default(),
            breaker(),
        );
        let err = pool
            .execute(request())
            .await
            .expect_err("request-level error propagates");
        assert!(matches!(err, InferenceExecutionError::ContextOverflow(_)));
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert_eq!(others, 0);
    }

    #[tokio::test]
    async fn rehomes_after_member_breaker_opens() {
        // Threshold 1: one transient failure opens the home member's breaker.
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::FailTransientThenOk { fails: 1 },
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        // First call: home fails transiently and opens its breaker.
        assert!(pool.execute(request()).await.is_err());
        // Second call: home is unhealthy, so the session fails over to the peer.
        assert!(pool.execute(request()).await.is_ok());
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert!(others >= 1, "second call should route to a healthy peer");
    }

    #[tokio::test]
    async fn respects_max_switches_per_session() {
        let switch = PoolSwitchPolicy {
            max_switches_per_session: Some(1),
            ..PoolSwitchPolicy::default()
        };
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![
                Behavior::AlwaysErr(InferenceExecutionError::rate_limited("429")),
                Behavior::AlwaysErr(InferenceExecutionError::rate_limited("429")),
                Behavior::AlwaysErr(InferenceExecutionError::rate_limited("429")),
            ],
            switch,
            breaker(),
        );
        assert!(pool.execute(request()).await.is_err());
        let total: u32 = stubs.iter().map(|s| s.call_count()).sum();
        assert_eq!(total, 2, "home + exactly one switch");
    }

    #[tokio::test]
    async fn execute_stream_switches_on_quota() {
        let (pool, stubs, home) = pool_home_fails(
            "agent-x",
            2,
            Behavior::AlwaysErr(InferenceExecutionError::overloaded("529")),
            PoolSwitchPolicy::default(),
            breaker(),
        );
        assert!(pool.execute_stream(request()).await.is_ok());
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert!(others >= 1);
    }
}
