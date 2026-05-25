use super::*;

mod tests {
    use super::*;
    use crate::engine::circuit_breaker::CircuitBreakerConfig;
    use crate::engine::pool_router::RouterMember;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::executor::{InferenceRoutingKey, LlmStreamEvent};
    use awaken_contract::contract::inference::StopReason;
    use awaken_contract::contract::message::Message;
    use awaken_contract::registry_spec::{
        HomeStrategy, PoolMemberRole, PoolRoutingPolicy, PoolSwitchPolicy, StickyScope,
    };
    use std::sync::atomic::{AtomicU32, Ordering};

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
            routing_key: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    fn request_for_thread(thread_id: &str) -> InferenceRequest {
        InferenceRequest {
            routing_key: Some(InferenceRoutingKey::thread(thread_id)),
            ..request()
        }
    }

    fn request_for_thread_run(thread_id: &str, run_id: &str) -> InferenceRequest {
        InferenceRequest {
            routing_key: Some(InferenceRoutingKey {
                thread_id: Some(thread_id.to_string()),
                run_id: Some(run_id.to_string()),
                fallback: None,
            }),
            ..request()
        }
    }

    enum Behavior {
        AlwaysOk,
        AlwaysErr(InferenceExecutionError),
        FailTransientThenOk { fails: u32 },
        StreamErr(InferenceExecutionError),
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
                Behavior::StreamErr(_) => Ok(ok_result()),
            }
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match &self.behavior {
                    Behavior::AlwaysErr(err) => Err(err.clone()),
                    Behavior::StreamErr(err) => Ok(Box::pin(futures::stream::iter(vec![
                        Ok(LlmStreamEvent::TextDelta("partial".into())),
                        Err(err.clone()),
                    ])) as InferenceStream),
                    _ => Ok(Box::pin(futures::stream::iter(vec![Ok(LlmStreamEvent::Stop(
                        StopReason::EndTurn,
                    ))])) as InferenceStream),
                }
            })
        }

        fn name(&self) -> &str {
            &self.id
        }
    }

    fn router_over(ids: &[String], switch: PoolSwitchPolicy) -> PoolRouter {
        router_over_with_routing(ids, PoolRoutingPolicy::default(), switch)
    }

    fn router_over_with_routing(
        ids: &[String],
        routing: PoolRoutingPolicy,
        switch: PoolSwitchPolicy,
    ) -> PoolRouter {
        let members = ids
            .iter()
            .map(|id| RouterMember {
                model_id: id.clone(),
                role: PoolMemberRole::Member,
                weight: 1,
            })
            .collect();
        PoolRouter::new(members, routing, switch)
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
        pool_all_with_routing(
            home_key,
            behaviors,
            PoolRoutingPolicy::default(),
            switch,
            breaker,
        )
    }

    fn pool_all_with_routing(
        home_key: &str,
        behaviors: Vec<Behavior>,
        routing: PoolRoutingPolicy,
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
        let router = router_over_with_routing(&member_ids, routing, switch);
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
    async fn pool_executor_rejects_upstream_model_override_support() {
        let (pool, _) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker(),
        );
        assert!(!pool.supports_upstream_model_override());
    }

    #[tokio::test]
    async fn same_thread_reuses_same_member_for_cache_affinity() {
        let thread_id = "thread-cache-affinity";
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker(),
        );
        let home = home_of(thread_id, stubs.len());

        assert!(pool.execute(request_for_thread(thread_id)).await.is_ok());
        assert!(pool.execute(request_for_thread(thread_id)).await.is_ok());

        assert_eq!(
            stubs[home].call_count(),
            2,
            "same thread must stay on its selected home member"
        );
        let other_calls: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, stub)| stub.call_count())
            .sum();
        assert_eq!(other_calls, 0);
    }

    #[tokio::test]
    async fn different_threads_keep_independent_sticky_members() {
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker(),
        );
        let left = (0..200)
            .map(|i| format!("thread-left-{i}"))
            .find(|key| home_of(key, stubs.len()) == 0)
            .expect("left-home key");
        let right = (0..200)
            .map(|i| format!("thread-right-{i}"))
            .find(|key| home_of(key, stubs.len()) == 1)
            .expect("right-home key");

        assert!(pool.execute(request_for_thread(&left)).await.is_ok());
        assert!(pool.execute(request_for_thread(&left)).await.is_ok());
        assert!(pool.execute(request_for_thread(&right)).await.is_ok());
        assert!(pool.execute(request_for_thread(&right)).await.is_ok());

        assert_eq!(stubs[0].call_count(), 2);
        assert_eq!(stubs[1].call_count(), 2);
    }

    #[tokio::test]
    async fn run_scope_keys_sessions_by_run_not_thread() {
        let routing = PoolRoutingPolicy {
            sticky_scope: StickyScope::Run,
            ..PoolRoutingPolicy::default()
        };
        let (pool, stubs) = pool_all_with_routing(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            routing,
            PoolSwitchPolicy::default(),
            breaker(),
        );
        let left = (0..200)
            .map(|i| format!("run-left-{i}"))
            .find(|key| home_of(key, stubs.len()) == 0)
            .expect("left-home run key");
        let right = (0..200)
            .map(|i| format!("run-right-{i}"))
            .find(|key| home_of(key, stubs.len()) == 1)
            .expect("right-home run key");

        assert!(
            pool.execute(request_for_thread_run("same-thread", &left))
                .await
                .is_ok()
        );
        assert!(
            pool.execute(request_for_thread_run("same-thread", &right))
                .await
                .is_ok()
        );

        assert_eq!(stubs[0].call_count(), 1);
        assert_eq!(stubs[1].call_count(), 1);
    }

    #[tokio::test]
    async fn round_robin_homes_new_sessions_in_sequence() {
        let routing = PoolRoutingPolicy {
            home: HomeStrategy::RoundRobin,
            ..PoolRoutingPolicy::default()
        };
        let (pool, stubs) = pool_all_with_routing(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk, Behavior::AlwaysOk],
            routing,
            PoolSwitchPolicy::default(),
            breaker(),
        );

        for i in 0..3 {
            assert!(
                pool.execute(request_for_thread(&format!("thread-{i}")))
                    .await
                    .is_ok()
            );
        }

        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![1, 1, 1]
        );
    }

    #[tokio::test]
    async fn same_thread_stays_on_failover_member_after_switch() {
        let thread_id = "thread-failover-sticky";
        let (pool, stubs, home) = pool_home_fails(
            thread_id,
            2,
            Behavior::AlwaysErr(InferenceExecutionError::rate_limited("429")),
            PoolSwitchPolicy::default(),
            breaker(),
        );
        let failover = 1 - home;

        assert!(pool.execute(request_for_thread(thread_id)).await.is_ok());
        assert_eq!(stubs[home].call_count(), 1);
        assert_eq!(stubs[failover].call_count(), 1);

        assert!(pool.execute(request_for_thread(thread_id)).await.is_ok());
        assert_eq!(
            stubs[home].call_count(),
            1,
            "thread should not return to quota-limited home after switching"
        );
        assert_eq!(stubs[failover].call_count(), 2);
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

    #[tokio::test]
    async fn observed_mid_stream_failure_moves_next_attempt_to_failover() {
        let thread_id = "stream-failure";
        let (pool, stubs, home) = pool_home_fails(
            thread_id,
            2,
            Behavior::StreamErr(InferenceExecutionError::Provider("reset".into())),
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = request_for_thread(thread_id);
        let err = InferenceExecutionError::StreamInterrupted {
            cause: awaken_contract::contract::executor::InterruptCause::ConnectionReset,
            snapshot: Box::new(awaken_contract::contract::executor::InterruptSnapshot {
                text: None,
                completed_tool_calls: vec![],
                in_flight_tool: None,
                bytes_received: 0,
            }),
        };

        pool.record_stream_failure(&req, &err);
        assert!(pool.execute(req).await.is_ok());

        assert_eq!(stubs[home].call_count(), 0);
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert_eq!(others, 1);
    }
}
