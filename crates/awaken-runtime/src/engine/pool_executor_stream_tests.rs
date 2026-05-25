use super::pool_executor_test_support::*;

mod tests {
    use super::*;
    use crate::engine::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InterruptCause, InterruptSnapshot, LlmExecutor, LlmStreamEvent,
    };
    use awaken_contract::registry_spec::PoolSwitchPolicy;
    use futures::StreamExt;
    use std::sync::Arc;

    fn stream_interrupted() -> InferenceExecutionError {
        InferenceExecutionError::StreamInterrupted {
            cause: InterruptCause::ConnectionReset,
            snapshot: Box::new(InterruptSnapshot {
                text: None,
                completed_tool_calls: vec![],
                in_flight_tool: None,
                bytes_received: 0,
            }),
        }
    }

    #[tokio::test]
    async fn stream_recovery_obeys_switch_budget_when_breaker_opens() {
        let thread_id = (0..200)
            .map(|i| format!("stream-budget-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let switch = PoolSwitchPolicy {
            max_switches_per_session: Some(0),
            ..PoolSwitchPolicy::default()
        };
        let (pool, stubs) = pool_all(
            &thread_id,
            vec![
                Behavior::StreamErr(InferenceExecutionError::Provider("reset".into())),
                Behavior::AlwaysOk,
            ],
            switch,
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-budget");

        let mut first = pool.execute_stream(req.clone()).await.expect("first opens");
        assert!(first.next().await.expect("first delta").is_ok());
        assert!(first.next().await.expect("first error").is_err());

        let err = match pool.execute_stream(req).await {
            Ok(_) => panic!("switch budget should prevent failover"),
            Err(err) => err,
        };
        assert!(matches!(err, InferenceExecutionError::Provider(_)));
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![1, 0]
        );
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

        pool.record_stream_failure(&req, &stream_interrupted());
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

    #[tokio::test]
    async fn stream_failure_records_actual_member_after_session_switches() {
        let thread_id = (0..200)
            .map(|i| format!("stream-attribution-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![
                Behavior::StreamErr(InferenceExecutionError::Provider("reset".into())),
                Behavior::AlwaysOk,
            ],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req_a = stream_request_for_thread(&thread_id, "logical-a");
        let mut stream_a = pool.execute_stream(req_a).await.expect("stream opens");

        let req_b = stream_request_for_thread(&thread_id, "logical-b");
        pool.record_stream_failure(&req_b, &stream_interrupted());

        assert!(matches!(
            stream_a.next().await,
            Some(Ok(LlmStreamEvent::TextDelta(_)))
        ));
        assert!(stream_a.next().await.expect("stream error").is_err());

        assert!(
            pool.execute(request_for_thread(&thread_id)).await.is_ok(),
            "m1 must remain available; m0 stream failure must not be recorded on m1"
        );
        assert_eq!(stubs[0].call_count(), 1);
        assert_eq!(stubs[1].call_count(), 1);
    }

    #[tokio::test]
    async fn logical_stream_attempts_do_not_switch_on_transient_failure() {
        let thread_id = (0..200)
            .map(|i| format!("stream-transient-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![
                Behavior::StreamErr(InferenceExecutionError::Provider("reset".into())),
                Behavior::AlwaysOk,
            ],
            PoolSwitchPolicy::default(),
            breaker_threshold(10),
        );
        let req = stream_request_for_thread(&thread_id, "logical-response");

        let mut first = pool.execute_stream(req.clone()).await.expect("first opens");
        assert!(first.next().await.expect("first delta").is_ok());
        assert!(first.next().await.expect("first error").is_err());

        let mut second = pool
            .execute_stream(req.clone())
            .await
            .expect("second opens on same member");
        assert!(second.next().await.expect("second delta").is_ok());
        assert!(second.next().await.expect("second error").is_err());

        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![2, 0],
            "transient mid-stream failures must not bypass switch policy"
        );
    }

    #[tokio::test]
    async fn logical_stream_attempts_do_not_revisit_policy_switched_members() {
        let thread_id = (0..200)
            .map(|i| format!("stream-multihop-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![
                Behavior::StreamErr(InferenceExecutionError::Provider("reset-a".into())),
                Behavior::StreamErr(InferenceExecutionError::Provider("reset-b".into())),
            ],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-response");

        let mut first = pool.execute_stream(req.clone()).await.expect("first opens");
        assert!(first.next().await.expect("first delta").is_ok());
        assert!(first.next().await.expect("first error").is_err());

        let mut second = pool
            .execute_stream(req.clone())
            .await
            .expect("second opens on failover");
        assert!(second.next().await.expect("second delta").is_ok());
        assert!(second.next().await.expect("second error").is_err());

        let err = match pool.execute_stream(req).await {
            Ok(_) => panic!("both members already tried in this logical inference"),
            Err(err) => err,
        };
        assert!(matches!(err, InferenceExecutionError::AllModelsUnavailable));
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![1, 1],
            "the third attempt must not jump back to m0"
        );
    }

    #[tokio::test]
    async fn dropping_half_open_stream_releases_probe_with_failure() {
        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: std::time::Duration::ZERO,
            half_open_max: 1,
        }));
        cb.record_failure("m0");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            cb,
        );

        let first = pool
            .execute_stream(request())
            .await
            .expect("half-open probe opens stream");
        drop(first);

        let _second = pool
            .execute_stream(request())
            .await
            .expect("abandoned probe should not strand half-open state");
        assert_eq!(stubs[0].call_count(), 2);
    }
}
