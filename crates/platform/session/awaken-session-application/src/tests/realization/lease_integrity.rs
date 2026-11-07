//! A Session realization lease whose persisted generation is untrustworthy is
//! refused permanently at the Control boundary, and the begin loop never
//! retries it.

use super::*;

#[tokio::test]
async fn an_untrustworthy_persisted_lease_generation_refuses_begin_permanently_without_retry() {
    // C1 the Session root's persisted realization lease generation: 0, or
    // u64::MAX on an expired lease that begin must succeed. Effects: E1 the
    // Control failure is Internal (terminal, never retried) with the stable
    // integrity code; E2 the begin root-CAS loop reads the root once per call
    // and never commits (no CAS retry); E3 three calls refuse identically and
    // leave the durable root, and its epoch, unchanged.
    // | Rule | persisted epoch | E1 detail                     | E2 reads/call | E3 |
    // | L1   | 0               | ... a granted lease carries 0 | 1             | =  |
    // | L2   | u64::MAX        | ... has no successor          | 1             | =  |
    for (rule, epoch, cause) in [
        ("L1", 0, "a granted lease carries epoch 0"),
        ("L2", u64::MAX, "the persisted lease epoch has no successor"),
    ] {
        let durable = sqlite_session_repository("lease integrity repository").await;
        let session_id = format!("lease-integrity-{rule}");
        let mut session = persisted(&session_id, false, "idle");
        session.realization = Some(crate::session_resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch,
            expires_at_unix_ms: 1,
        });
        create(durable.as_ref(), session).await;
        let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
        let application = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let before = durable
            .get(&SessionId::from(session_id.as_str()))
            .await
            .unwrap();
        let expected =
            awaken_session_execution_contract::SessionRealizationControlFailure::Internal(format!(
                "Session realization lease: lease_generation_integrity_violated: {cause}"
            ));
        for attempt in 0..3 {
            let reads = repo.observed_gets.lock().unwrap().len();
            let refused = SessionRealizationControl::begin_session_realization(
                application.realizations.as_ref(),
                awaken_session_execution_contract::BeginSessionRealization {
                    expected_resources: None,
                    session_id: session_id.as_str().into(),
                    target: awaken_session_execution_contract::SessionRealizationTarget {
                        owner: "worker-b".into(),
                        runtime_incarnation: "worker-b/boot-1".into(),
                        lease_expires_at_unix_ms: u64::MAX,
                        reassign_existing_lease: true,
                    },
                },
            )
            .await;
            assert_eq!(
                refused.err(),
                Some(expected.clone()),
                "{rule}/E1 attempt {attempt}"
            );
            assert_eq!(
                expected.disposition(),
                awaken_session_execution_contract::SessionRealizationControlDisposition::Terminal,
                "{rule}/E1"
            );
            assert_eq!(
                repo.observed_gets.lock().unwrap().len() - reads,
                1,
                "{rule}/E2 one root read, no CAS retry, attempt {attempt}"
            );
            assert_eq!(
                durable
                    .get(&SessionId::from(session_id.as_str()))
                    .await
                    .unwrap(),
                before,
                "{rule}/E3 root unchanged, attempt {attempt}"
            );
        }
    }
}
