use super::pool_executor_test_support::*;

mod tests {
    use super::*;
    use crate::engine::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use awaken_runtime_contract::contract::executor::{
        InferenceExecutionError, InterruptCause, InterruptSnapshot, LlmExecutor, LlmStreamEvent,
    };
    use awaken_runtime_contract::registry_spec::PoolSwitchPolicy;
    use futures::StreamExt;
    use std::collections::HashMap;
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
        assert!(
            pool.execute_stream(request_for_thread("agent-x"))
                .await
                .is_ok()
        );
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
        // Open the stream so the originating member is recorded in the
        // attempt cache, then report the failure the way the loop runner
        // does. The external failure must resolve precisely to the member
        // that opened the stream (home) — never an arbitrary fallback.
        let req = stream_request_for_thread(thread_id, "logical-observed");
        let _stream = pool
            .execute_stream(req.clone())
            .await
            .expect("stream opens");

        pool.record_stream_failure(&req, &stream_interrupted());
        assert!(pool.execute(request_for_thread(thread_id)).await.is_ok());

        assert_eq!(stubs[home].call_count(), 1, "only the stream open hit home");
        let others: u32 = stubs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != home)
            .map(|(_, s)| s.call_count())
            .sum();
        assert_eq!(others, 1, "recorded failure on home re-homes the next call");
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
    async fn external_stream_failure_resolved_from_attempt_attributes_to_originating_member() {
        // When the originating member IS precisely resolvable (the attempt is
        // still cached), a late stream failure must land on that member even
        // though the session has since failed over to another member.
        let thread_id = (0..200)
            .map(|i| format!("stream-attr-active-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let cb = breaker_threshold(1);
        let (pool, stubs) = pool_all(
            &thread_id,
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            cb.clone(),
        );

        // Open a stream on the home member (m0); its attempt records active=m0.
        let req_a = stream_request_for_thread(&thread_id, "logical-a");
        let _stream_a = pool
            .execute_stream(req_a.clone())
            .await
            .expect("opens on m0");

        // Force the *session* to fail over to m1 by opening m0's breaker, then
        // running a fresh logical inference that observes m0 unhealthy. The
        // session's active member is now m1, but logical-a's attempt still
        // points at m0.
        cb.record_failure("m0");
        let req_b = stream_request_for_thread(&thread_id, "logical-b");
        let _stream_b = pool
            .execute_stream(req_b)
            .await
            .expect("session fails over to m1");
        assert_eq!(stubs[1].call_count(), 1, "session moved to m1");

        // Heal m0 so only a precisely-attributed late failure could re-open it.
        cb.record_success("m0");
        assert!(cb.is_available("m0"));
        assert!(cb.is_available("m1"));

        // Report logical-a's late failure. It must land on m0 (originating),
        // never on m1 (the current session active).
        pool.record_stream_failure(&req_a, &stream_interrupted());

        assert!(
            !cb.is_available("m0"),
            "late failure re-opened originating m0"
        );
        assert!(
            cb.is_available("m1"),
            "post-failover active m1 must not absorb m0's late failure"
        );
    }

    #[tokio::test]
    async fn unresolvable_stream_failure_skips_recording_instead_of_blaming_active() {
        // The originating member CANNOT be precisely resolved (no stream was
        // opened for this attempt key through the pool). The pre-fix fallback
        // resolved `current` via `select_active`, recording the failure onto
        // whichever member the session had failed over to — an innocent member.
        // The fix must skip recording entirely so no arbitrary breaker trips.
        let thread_id = (0..200)
            .map(|i| format!("stream-attr-skip-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let cb = breaker_threshold(1);
        let (pool, stubs) = pool_all(
            &thread_id,
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            cb.clone(),
        );

        // Drive the session onto m1 by opening m0's breaker, then running an
        // inference that homes on m0, observes it unhealthy, and fails over.
        cb.record_failure("m0");
        let _stream_b = pool
            .execute_stream(stream_request_for_thread(&thread_id, "logical-open"))
            .await
            .expect("session fails over to m1");
        assert_eq!(stubs[1].call_count(), 1, "session active is now m1");
        cb.record_success("m0");
        assert!(cb.is_available("m0"));
        assert!(cb.is_available("m1"));

        // Report a failure for a logical inference whose stream was never
        // opened through this pool: its attempt key is absent from the cache.
        let orphan = stream_request_for_thread(&thread_id, "logical-never-opened");
        pool.record_stream_failure(&orphan, &stream_interrupted());

        assert!(
            cb.is_available("m0") && cb.is_available("m1"),
            "an unresolvable stream failure must not be charged to any member"
        );
    }

    #[tokio::test]
    async fn logical_only_stream_failure_is_recorded_and_steers_failover() {
        // A request carrying only a logical_inference_id (no thread/run/
        // fallback) must yield a *stable* session key. With the old anonymous
        // key, `record_stream_failure` recomputed a fresh session — and thus a
        // different attempt key — so it could not find the originating attempt
        // and dropped the failure entirely. The stable logical key must let the
        // external failure land on the originating member and steer the next
        // attempt onto the failover member instead of bouncing back.
        let home_key = "agent-x";
        let logical_id = (0..400)
            .map(|i| format!("logical-{i}"))
            .find(|lid| home_of(&format!("{home_key}\0logical\0{lid}"), 2) == 0)
            .expect("logical-only session homes on m0");
        let cb = breaker_threshold(1);
        let (pool, stubs) = pool_all(
            home_key,
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            cb.clone(),
        );
        let req = stream_request_logical_only(&logical_id);

        // Open the stream on the home member (m0); the attempt records active=m0.
        let _stream = pool
            .execute_stream(req.clone())
            .await
            .expect("logical-only stream opens on m0");

        // An idle-stall style external failure for the same logical inference.
        pool.record_stream_failure(&req, &stream_interrupted());

        // The failure must be attributed to m0 (originating), opening its
        // breaker; m1 must remain untouched.
        assert!(
            !cb.is_available("m0"),
            "logical-only stream failure must be recorded on the originating member"
        );
        assert!(cb.is_available("m1"));

        // The next attempt for this logical inference must fail over to m1, not
        // bounce back to the already-tried m0 under a fresh anonymous session.
        let mut second = pool
            .execute_stream(req)
            .await
            .expect("recovery opens on failover member");
        assert!(second.next().await.expect("recovery delta").is_ok());
        assert_eq!(stubs[0].call_count(), 1, "m0 only opened the first stream");
        assert_eq!(stubs[1].call_count(), 1, "recovery moved to m1");
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
    async fn dropping_stream_does_not_count_as_member_failure() {
        let thread_id = (0..200)
            .map(|i| format!("stream-cancel-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker_threshold(1),
        );
        let req = stream_request_for_thread(&thread_id, "logical-cancel");

        let stream = pool
            .execute_stream(req.clone())
            .await
            .expect("stream opens");
        drop(stream);

        let mut second = pool
            .execute_stream(req)
            .await
            .expect("cancelled stream should not open breaker");
        assert!(second.next().await.expect("second event").is_ok());
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![2, 0],
            "dropping a stream must not be recorded as provider failure or trigger failover"
        );
    }

    #[tokio::test]
    async fn dropping_stream_after_recorded_failure_preserves_attempt_history() {
        let thread_id = (0..200)
            .map(|i| format!("stream-idle-drop-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let quota_err = InferenceExecutionError::overloaded("recover on another member");
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::StreamErr(quota_err.clone())],
            PoolSwitchPolicy::default(),
            breaker_threshold(10),
        );
        let req = stream_request_for_thread(&thread_id, "logical-idle-drop");

        let first = pool
            .execute_stream(req.clone())
            .await
            .expect("first stream opens on m0");
        pool.record_stream_failure(&req, &quota_err);
        drop(first);

        let mut second = pool
            .execute_stream(req.clone())
            .await
            .expect("second stream opens on failover");
        assert!(second.next().await.expect("second delta").is_ok());
        assert!(second.next().await.expect("second error").is_err());

        let _third = pool
            .execute_stream(req)
            .await
            .expect("third attempt remains on current member");
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![1, 2],
            "drop after an externally recorded failure must not erase the prior tried member"
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
    async fn dropping_half_open_stream_releases_probe_without_counting_failure() {
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

    #[tokio::test]
    async fn stream_open_error_does_not_leave_in_flight_attempt() {
        let thread_id = "stream-open-error";
        let logical_id = "logical-open-error";
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysErr(InferenceExecutionError::Provider(
                "open failed".into(),
            ))],
            PoolSwitchPolicy::default(),
            breaker_threshold(10),
        );
        let req = stream_request_for_thread(thread_id, logical_id);

        assert!(pool.execute_stream(req).await.is_err());

        let attempt_key = format!("{thread_id}\0{logical_id}");
        {
            let attempts = pool.inner.stream_attempts.read();
            let attempt = attempts
                .get(&attempt_key)
                .expect("open failure should leave evictable attempt history");
            assert!(!attempt.in_flight);
            assert!(attempt.failure_observed);
            assert_eq!(attempt.active, Some(0));
            assert_eq!(attempt.tried, vec![true]);
        }

        let mut attempts = pool.inner.stream_attempts.write();
        for i in 0..super::super::MAX_POOL_STREAM_ATTEMPTS - 1 {
            let mut active = super::super::PoolStreamAttemptState::new(1);
            active.in_flight = true;
            active.last_access = i + 1;
            attempts.insert(format!("active-{i}"), active);
        }
        super::super::PoolExecutorInner::ensure_stream_attempt_capacity(
            &mut attempts,
            "new-attempt",
        );

        assert!(
            !attempts.contains_key(&attempt_key),
            "stream-open errors must remain evictable instead of orphaning in-flight state"
        );
    }

    #[test]
    fn stream_attempt_capacity_preserves_in_flight_attempts() {
        let mut attempts = HashMap::new();
        let mut active = super::super::PoolStreamAttemptState::new(2);
        active.in_flight = true;
        active.last_access = 1;
        attempts.insert("active".to_string(), active);

        for i in 1..super::super::MAX_POOL_STREAM_ATTEMPTS {
            let mut stale = super::super::PoolStreamAttemptState::new(2);
            stale.last_access = i + 1;
            attempts.insert(format!("stale-{i}"), stale);
        }

        super::super::PoolExecutorInner::ensure_stream_attempt_capacity(&mut attempts, "current");

        assert!(attempts.contains_key("active"));
        assert_eq!(attempts.len(), super::super::MAX_POOL_STREAM_ATTEMPTS - 1);
    }

    #[tokio::test]
    async fn stream_open_failures_without_logical_id_do_not_revisit_members() {
        let (pool, stubs) = pool_all(
            "agent-x",
            vec![
                Behavior::AlwaysErr(InferenceExecutionError::overloaded("m0 quota")),
                Behavior::AlwaysErr(InferenceExecutionError::overloaded("m1 quota")),
            ],
            PoolSwitchPolicy::default(),
            breaker(),
        );

        let err = match pool.execute_stream(request()).await {
            Ok(_) => panic!("both members should be exhausted"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            InferenceExecutionError::PoolAttemptsExhausted
        ));
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![1, 1],
            "a single stream-open call must not retry an already-tried member"
        );
    }

    #[tokio::test]
    async fn stream_open_failures_with_full_attempt_cache_keep_local_tried_mask() {
        let thread_id = (0..200)
            .map(|i| format!("stream-full-cache-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let (pool, stubs) = pool_all(
            &thread_id,
            vec![
                Behavior::AlwaysErr(InferenceExecutionError::overloaded("m0 quota")),
                Behavior::AlwaysErr(InferenceExecutionError::overloaded("m1 quota")),
            ],
            PoolSwitchPolicy::default(),
            breaker(),
        );
        {
            let mut attempts = pool.inner.stream_attempts.write();
            for i in 0..super::super::MAX_POOL_STREAM_ATTEMPTS {
                let mut active = super::super::PoolStreamAttemptState::new(2);
                active.in_flight = true;
                active.last_access = i + 1;
                attempts.insert(format!("active-{i}"), active);
            }
        }

        let err = match pool
            .execute_stream(stream_request_for_thread(&thread_id, "logical-full-cache"))
            .await
        {
            Ok(_) => panic!("both members should be exhausted"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            InferenceExecutionError::PoolAttemptsExhausted
        ));
        assert_eq!(
            stubs.iter().map(|s| s.call_count()).collect::<Vec<_>>(),
            vec![1, 1],
            "local tried state must survive when the attempt cache rejects a new key"
        );
        assert_eq!(
            pool.inner.stream_attempts.read().len(),
            super::super::MAX_POOL_STREAM_ATTEMPTS,
            "full in-flight cache remains capped"
        );
    }

    #[tokio::test]
    async fn stream_terminal_failure_records_member_when_attempt_cache_is_full() {
        let thread_id = (0..200)
            .map(|i| format!("stream-full-cache-terminal-{i}"))
            .find(|key| home_of(key, 2) == 0)
            .expect("thread home on m0");
        let cb = breaker_threshold(1);
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::StreamErr(InferenceExecutionError::Provider(
                "reset".into(),
            ))],
            PoolSwitchPolicy::default(),
            cb.clone(),
        );
        {
            let mut attempts = pool.inner.stream_attempts.write();
            for i in 0..super::super::MAX_POOL_STREAM_ATTEMPTS {
                let mut active = super::super::PoolStreamAttemptState::new(1);
                active.in_flight = true;
                active.live_streams = 1;
                active.last_access = i + 1;
                attempts.insert(format!("active-{i}"), active);
            }
        }

        let mut stream = pool
            .execute_stream(stream_request_for_thread(
                &thread_id,
                "logical-full-cache-terminal",
            ))
            .await
            .expect("stream opens even when attempt cache cannot track a new key");
        assert!(stream.next().await.expect("stream delta").is_ok());
        assert!(stream.next().await.expect("stream error").is_err());

        assert!(
            !cb.is_available("m0"),
            "terminal stream failure must still hit the captured member breaker"
        );
        assert_eq!(
            pool.inner.stream_attempts.read().len(),
            super::super::MAX_POOL_STREAM_ATTEMPTS,
            "full in-flight cache remains capped"
        );
    }

    #[test]
    fn stream_attempt_capacity_is_hard_cap_when_all_entries_are_in_flight() {
        let mut attempts = HashMap::new();
        for i in 0..super::super::MAX_POOL_STREAM_ATTEMPTS {
            let mut active = super::super::PoolStreamAttemptState::new(2);
            active.in_flight = true;
            active.last_access = i + 1;
            attempts.insert(format!("active-{i}"), active);
        }

        let accepted = super::super::PoolExecutorInner::ensure_stream_attempt_capacity(
            &mut attempts,
            "overflow",
        );

        assert!(
            !accepted,
            "a full active cache must reject new tracked keys"
        );
        assert_eq!(attempts.len(), super::super::MAX_POOL_STREAM_ATTEMPTS);
        assert!(!attempts.contains_key("overflow"));
    }

    #[test]
    fn session_capacity_evicts_least_recently_accessed() {
        let mut sessions = HashMap::new();
        // The least-recently-accessed session carries the smallest stamp.
        let lru = super::super::PoolSessionState {
            active: Some(0),
            switch_count: 0,
            last_access: 1,
        };
        sessions.insert("lru".to_string(), lru);

        for i in 1..super::super::MAX_POOL_SESSION_STATES {
            sessions.insert(
                format!("hot-{i}"),
                super::super::PoolSessionState {
                    active: Some(0),
                    switch_count: 0,
                    last_access: i + 1,
                },
            );
        }

        super::super::PoolExecutorInner::ensure_session_capacity(&mut sessions, "current");

        assert!(
            !sessions.contains_key("lru"),
            "the least-recently-accessed session must be evicted, not an arbitrary key"
        );
        assert_eq!(sessions.len(), super::super::MAX_POOL_SESSION_STATES - 1);
    }

    #[tokio::test]
    async fn lru_session_is_evicted_under_capacity_pressure() {
        let (pool, _stubs) = pool_all(
            "agent-x",
            vec![Behavior::AlwaysOk, Behavior::AlwaysOk],
            PoolSwitchPolicy::default(),
            breaker(),
        );

        // Create one session, then fill the table to capacity with newer ones.
        pool.execute(request_for_thread("victim")).await.unwrap();
        for i in 0..super::super::MAX_POOL_SESSION_STATES - 1 {
            pool.execute(request_for_thread(&format!("filler-{i}")))
                .await
                .unwrap();
        }
        assert_eq!(
            pool.inner.sessions.read().len(),
            super::super::MAX_POOL_SESSION_STATES
        );

        // Re-touch "victim" so it is no longer the LRU; "filler-0" now is.
        pool.execute(request_for_thread("victim")).await.unwrap();

        // One more distinct session forces a single LRU eviction.
        pool.execute(request_for_thread("overflow")).await.unwrap();

        let sessions = pool.inner.sessions.read();
        assert_eq!(sessions.len(), super::super::MAX_POOL_SESSION_STATES);
        assert!(
            sessions.contains_key("victim"),
            "a recently-accessed session must survive eviction"
        );
        assert!(
            !sessions.contains_key("filler-0"),
            "the least-recently-accessed session must be the one evicted"
        );
        assert!(sessions.contains_key("overflow"));
    }
}
