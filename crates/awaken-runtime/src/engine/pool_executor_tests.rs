mod tests {
    use super::super::pool_executor_test_support::*;
    use awaken_contract::contract::executor::{InferenceExecutionError, LlmExecutor};
    use awaken_contract::registry_spec::{
        HomeStrategy, PoolRoutingPolicy, PoolSwitchPolicy, StickyScope,
    };

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
        let thread_id = (0..200)
            .map(|i| format!("permanent-quarantine-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let (pool, stubs, home) = pool_home_fails(
            &thread_id,
            2,
            Behavior::AlwaysErr(InferenceExecutionError::Unauthorized("401".into())),
            PoolSwitchPolicy::default(),
            breaker(),
        );
        assert!(pool.execute(request_for_thread(&thread_id)).await.is_ok());
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert!(others >= 1);

        let second_thread = (0..200)
            .map(|i| format!("permanent-quarantine-new-{i}"))
            .find(|key| home_of(key, 2) == home)
            .expect("new thread also homes on quarantined member");
        assert!(
            pool.execute(request_for_thread(&second_thread))
                .await
                .is_ok()
        );
        assert_eq!(
            stubs[home].call_count(),
            1,
            "quarantined permanent-error member must not be retried by a new session"
        );
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
    async fn all_open_members_short_circuit_without_provider_calls() {
        let cb = breaker_threshold(1);
        cb.record_failure("m0");
        cb.record_failure("m1");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            cb,
        );

        let err = pool
            .execute(request())
            .await
            .expect_err("all open members should short-circuit");

        assert!(matches!(err, InferenceExecutionError::AllModelsUnavailable));
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).sum::<u32>(),
            0,
            "open breakers must prevent provider calls"
        );
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
}
