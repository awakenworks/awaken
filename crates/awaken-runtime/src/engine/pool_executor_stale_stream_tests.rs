use super::pool_executor_test_support::*;

mod tests {
    use super::*;
    use awaken_runtime_contract::contract::executor::{InferenceExecutionError, LlmExecutor};
    use awaken_runtime_contract::registry_spec::PoolSwitchPolicy;
    use futures::StreamExt;

    #[tokio::test]
    async fn stale_stream_drop_does_not_clear_recovery_attempt() {
        let thread_id = (0..200)
            .map(|i| format!("stream-stale-drop-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let quota_err = InferenceExecutionError::overloaded("recover on failover");
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-stale-drop");
        let session_key = pool.inner.session_key(&req);
        let attempt_key = pool
            .inner
            .stream_attempt_key(&session_key, &req)
            .expect("logical attempt key");

        let first = pool
            .execute_stream(req.clone())
            .await
            .expect("first stream opens on m0");
        pool.record_stream_failure(&req, &quota_err);
        let second = pool
            .execute_stream(req.clone())
            .await
            .expect("recovery stream opens on m1");

        drop(first);

        let attempts = pool.inner.stream_attempts.read();
        let attempt = attempts
            .get(&attempt_key)
            .expect("recovery attempt remains cached");
        assert_eq!(attempt.active, Some(1));
        assert!(attempt.in_flight);
        assert_eq!(attempt.tried, vec![true, true]);
        drop(attempts);
        drop(second);
    }

    #[tokio::test]
    async fn stale_stream_success_does_not_clear_recovery_attempt() {
        let thread_id = (0..200)
            .map(|i| format!("stream-stale-success-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let quota_err = InferenceExecutionError::overloaded("recover on failover");
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-stale-success");
        let session_key = pool.inner.session_key(&req);
        let attempt_key = pool
            .inner
            .stream_attempt_key(&session_key, &req)
            .expect("logical attempt key");

        let mut first = pool
            .execute_stream(req.clone())
            .await
            .expect("first stream opens on m0");
        pool.record_stream_failure(&req, &quota_err);
        let second = pool
            .execute_stream(req.clone())
            .await
            .expect("recovery stream opens on m1");

        assert!(first.next().await.expect("stale stream event").is_ok());
        assert!(first.next().await.is_none());

        let attempts = pool.inner.stream_attempts.read();
        let attempt = attempts
            .get(&attempt_key)
            .expect("late stale success must not clear recovery attempt");
        assert_eq!(attempt.active, Some(1));
        assert!(attempt.in_flight);
        assert_eq!(attempt.tried, vec![true, true]);
        drop(attempts);
        drop(second);
    }

    #[tokio::test]
    async fn stale_stream_error_does_not_overwrite_recovery_attempt() {
        let thread_id = (0..200)
            .map(|i| format!("stream-stale-error-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let quota_err = InferenceExecutionError::overloaded("recover on failover");
        let stream_err = InferenceExecutionError::Provider("late reset".into());
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::StreamErr(stream_err), Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-stale-error");
        let session_key = pool.inner.session_key(&req);
        let attempt_key = pool
            .inner
            .stream_attempt_key(&session_key, &req)
            .expect("logical attempt key");

        let mut first = pool
            .execute_stream(req.clone())
            .await
            .expect("first stream opens on m0");
        pool.record_stream_failure(&req, &quota_err);
        let second = pool
            .execute_stream(req.clone())
            .await
            .expect("recovery stream opens on m1");

        assert!(first.next().await.expect("stale stream delta").is_ok());
        assert!(first.next().await.expect("stale stream error").is_err());

        let attempts = pool.inner.stream_attempts.read();
        let attempt = attempts
            .get(&attempt_key)
            .expect("late stale error must not clear recovery attempt");
        assert_eq!(attempt.active, Some(1));
        assert!(attempt.in_flight);
        assert!(!attempt.failure_observed);
        assert_eq!(attempt.tried, vec![true, true]);
        drop(attempts);
        drop(second);
    }

    #[tokio::test]
    async fn stale_external_success_does_not_clear_recovery_attempt() {
        let thread_id = (0..200)
            .map(|i| format!("stream-stale-external-success-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let quota_err = InferenceExecutionError::overloaded("recover on failover");
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-stale-external-success");
        let session_key = pool.inner.session_key(&req);
        let attempt_key = pool
            .inner
            .stream_attempt_key(&session_key, &req)
            .expect("logical attempt key");

        let first = pool
            .execute_stream(req.clone())
            .await
            .expect("first stream opens on m0");
        pool.record_stream_failure(&req, &quota_err);
        let second = pool
            .execute_stream(req.clone())
            .await
            .expect("recovery stream opens on m1");

        pool.record_stream_success(&req);

        let attempts = pool.inner.stream_attempts.read();
        let attempt = attempts
            .get(&attempt_key)
            .expect("request-scoped stale success must not clear recovery attempt");
        assert_eq!(attempt.active, Some(1));
        assert!(attempt.in_flight);
        assert_eq!(attempt.live_streams, 2);
        assert_eq!(attempt.tried, vec![true, true]);
        drop(attempts);
        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn stale_external_failure_does_not_penalize_recovery_attempt() {
        let thread_id = (0..200)
            .map(|i| format!("stream-stale-external-failure-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let quota_err = InferenceExecutionError::overloaded("recover on failover");
        let stale_err = InferenceExecutionError::Timeout("late timeout from stale stream".into());
        let cb = breaker_threshold(1);
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            cb.clone(),
        );
        let req = stream_request_for_thread(&thread_id, "logical-stale-external-failure");
        let session_key = pool.inner.session_key(&req);
        let attempt_key = pool
            .inner
            .stream_attempt_key(&session_key, &req)
            .expect("logical attempt key");

        let first = pool
            .execute_stream(req.clone())
            .await
            .expect("first stream opens on m0");
        pool.record_stream_failure(&req, &quota_err);
        let second = pool
            .execute_stream(req.clone())
            .await
            .expect("recovery stream opens on m1");
        assert!(
            cb.is_available("m1"),
            "recovery member starts healthy before stale callback"
        );

        pool.record_stream_failure(&req, &stale_err);

        assert!(
            cb.is_available("m1"),
            "request-scoped stale failure must not trip recovery member"
        );
        let attempts = pool.inner.stream_attempts.read();
        let attempt = attempts
            .get(&attempt_key)
            .expect("request-scoped stale failure must not clear recovery attempt");
        assert_eq!(attempt.active, Some(1));
        assert!(attempt.in_flight);
        assert!(!attempt.failure_observed);
        assert_eq!(attempt.live_streams, 2);
        assert_eq!(attempt.tried, vec![true, true]);
        drop(attempts);
        drop(first);
        drop(second);
    }
}
