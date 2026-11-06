//! Publication chooses the construction-time topology, never a live service handle.
use super::*;
use awaken_dispatch_contract::WakeSignal;

#[derive(Default)]
struct ObservedWake {
    calls: AtomicUsize,
    original: crate::LocalWakeSignal,
}
#[async_trait::async_trait]
impl WakeSignal for ObservedWake {
    async fn publish(&self) -> Result<(), awaken_dispatch_contract::DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.original.publish().await
    }
    async fn wait(&self) {
        self.original.wait().await;
    }
}

#[tokio::test]
async fn publication_uses_the_selected_local_hint_before_services_start() {
    // Causes: C1 local pool selected / remote Workers only; C2 service cold,
    // started, or drained. Effects: E1 exactly one original local latency hint
    // when C1 is local; E2 no local hint when remote. Table P1/P2 crosses both
    // C1 values with all three C2 phases. Pool/registry readiness is separate:
    // publication may never reopen admission or authorize a claim.
    // The only intentional correction is the cold local row: capability is
    // selected at construction, not discovered by retaining a later pool handle.
    for local in [false, true] {
        let wake = Arc::new(ObservedWake::default());
        let execution = if local {
            awaken_deployment_config::LocalExecution::InProcessPool(
                DispatchPoolSizing::LocalParallelism(std::num::NonZeroUsize::new(1).unwrap()),
            )
        } else {
            awaken_deployment_config::LocalExecution::RemoteWorkersOnly
        };
        let fixture = coordinator_fixture_with_wake(execution, wake.clone());
        for phase in ["cold", "started", "drained"] {
            if phase == "started" {
                fixture
                    .dispatch
                    .ensure_pool(
                        Some(DispatchBinding::local(fixture.owner.clone())),
                        Arc::new(NoCompletion),
                        || Ok(fixture.resolver.clone()),
                    )
                    .unwrap();
            } else if phase == "drained" {
                let _fence = fixture.dispatch.begin_drain().await;
            }
            let before = wake.calls.load(Ordering::SeqCst);
            fixture.dispatch.publication().hint_local_pool().await;
            assert_eq!(
                wake.calls.load(Ordering::SeqCst) - before,
                usize::from(local),
                "P local={local} phase={phase}: use the construction-time hint"
            );
            if phase == "drained" {
                assert!(
                    !fixture.dispatch.accepting_work().await,
                    "publication never grants readiness"
                );
                assert!(
                    !fixture.dispatch.resume_admission().await,
                    "publication never reopens drain"
                );
            }
        }
        let _fence = fixture.dispatch.begin_drain().await;
    }
}

#[tokio::test]
async fn publication_retains_the_original_outbox_and_cold_hint_delivery() {
    use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
    use awaken_dispatch_contract::{InboxRead as _, Outbox as _, PendingInput};
    // O1 configured local pool, still cold -> the original owner relays the
    // exact input before return, then retains a hint for the future listener.
    // O2 remote Workers -> the same relay, without a local hint.
    // O3 duplicate publication/relay -> one original pending record, no second
    // message or drainer. Both rows retain the original durable-control owner.
    // The pool and publication reuse one idempotent Outbox implementation.
    for local in [false, true] {
        let wake = Arc::new(ObservedWake::default());
        let execution = if local {
            awaken_deployment_config::LocalExecution::InProcessPool(
                DispatchPoolSizing::LocalParallelism(std::num::NonZeroUsize::new(1).unwrap()),
            )
        } else {
            awaken_deployment_config::LocalExecution::RemoteWorkersOnly
        };
        let fixture = coordinator_fixture_with_wake(execution, wake.clone());
        let publication = fixture.dispatch.publication();
        let input = PendingInput {
            message_id: "original-publication-message".into(),
            thread_id: ThreadId("original-thread".into()),
            run_id: RunId("original-run".into()),
            correlation_id: "original-ticket".into(),
            available_at_ms: None,
            context_messages: Vec::new(),
            result: awaken_runtime_contract::resume::ResumeResult::Continue,
        };
        publication
            .durable_run_control(fixture.owner.clone())
            .stage_input(input.clone())
            .await
            .unwrap();
        publication.publish_staged(&fixture.owner).await.unwrap();
        assert_eq!(
            wake.calls.load(Ordering::SeqCst),
            if local { 2 } else { 0 },
            "O1/O2 construction-selected hints"
        );
        let pending = fixture.owner.list(&input.thread_id).await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "O1/O2 original owner relays before return"
        );
        if local {
            // A local hint published before the service exists must remain
            // available when its original listener first starts waiting.
            // awaken-allow(wait): a one-shot bound verifies Notify's retained cold-start hint permit.
            tokio::time::timeout(std::time::Duration::from_secs(1), wake.wait())
                .await
                .expect("O1 retained hint");
            assert_eq!(
                fixture.owner.relay().await.unwrap(),
                0,
                "O1 original drainer sees the already-relayed input"
            );
        }
        publication.publish_staged(&fixture.owner).await.unwrap();
        assert_eq!(
            fixture.owner.relay().await.unwrap(),
            0,
            "O3 relay idempotency"
        );
        let pending = fixture.owner.list(&input.thread_id).await.unwrap();
        assert_eq!(pending.len(), 1, "O3 one original pending record");
        assert_eq!(pending[0].input, input, "O3 original identity and payload");
        assert!(
            !fixture.dispatch.has_started_services(),
            "publication constructs no service"
        );
    }
}

#[tokio::test]
async fn retained_publication_does_not_keep_the_terminal_service_alive() {
    // L1 a real terminal scanner starts; L2 a caller retains only publication;
    // L3 root service ownership ends -> the original scanner/resolver ends.
    // L4 publication remains usable over the original queue after L3. The
    // lifetime wait observes actual resolver release, never a guessed delay.
    let fixture = coordinator_fixture(awaken_deployment_config::LocalExecution::RemoteWorkersOnly);
    let publication = fixture.dispatch.publication();
    assert!(fixture.dispatch.ensure_terminal_reconciliation(
        true,
        Some(fixture.owner.clone()),
        Arc::new(NoCompletion),
        || Some(fixture.resolver.clone())
    ));
    drop(fixture.resolver);
    drop(fixture.dispatch);
    eventually(
        "L3 original resolver released despite retained publication",
        || fixture.dropped.load(Ordering::SeqCst),
    )
    .await;
    publication
        .publish_staged(&fixture.owner)
        .await
        .expect("L4 original queue still publishes");
}

#[tokio::test]
async fn publication_preserves_original_relay_failure_and_exact_retry() {
    use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
    use awaken_dispatch_contract::{
        DispatchError, Inbox as _, InboxRead as _, Outbox as _, PendingInput,
    };
    // F1 a real pending/outbox payload conflict in the original queue -> exact
    // Conflict, no local success hint, no partial relay. F2 remove only that
    // conflicting pending revision -> retry relays the originally staged input,
    // then hints locally. F3 duplicate retry -> one record. Cross local/remote:
    // a publisher never hides relay failure because a pool handle exists.
    for local in [false, true] {
        let wake = Arc::new(ObservedWake::default());
        let execution = if local {
            awaken_deployment_config::LocalExecution::InProcessPool(
                DispatchPoolSizing::LocalParallelism(std::num::NonZeroUsize::new(1).unwrap()),
            )
        } else {
            awaken_deployment_config::LocalExecution::RemoteWorkersOnly
        };
        let fixture = coordinator_fixture_with_wake(execution, wake.clone());
        let publication = fixture.dispatch.publication();
        let pending = PendingInput {
            message_id: "relay-conflict".into(),
            thread_id: ThreadId("thread".into()),
            run_id: RunId("run".into()),
            correlation_id: "ticket".into(),
            available_at_ms: None,
            context_messages: Vec::new(),
            result: awaken_runtime_contract::resume::ResumeResult::Continue,
        };
        let mut staged = pending.clone();
        staged.correlation_id = "original-staged-ticket".into();
        fixture.owner.append(pending.clone()).await.unwrap();
        fixture.owner.stage(staged.clone()).await.unwrap();
        let error = publication
            .publish_staged(&fixture.owner)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DispatchError::Conflict(_)),
            "F1 original failure kind"
        );
        assert_eq!(
            error.to_string(),
            "dispatch identity conflict: idempotency key `relay-conflict` was reused with another relayed payload",
            "F1 original diagnostic"
        );
        assert_eq!(wake.calls.load(Ordering::SeqCst), 0, "F1 no success hint");
        let rows = fixture.owner.list(&pending.thread_id).await.unwrap();
        assert_eq!(rows.len(), 1, "F1 no partial relay");
        assert_eq!(rows[0].input, pending, "F1 original pending unchanged");
        fixture
            .owner
            .retract(&pending.message_id, rows[0].revision)
            .await
            .unwrap();
        publication
            .publish_staged(&fixture.owner)
            .await
            .expect("F2 same staged input retry");
        assert_eq!(
            wake.calls.load(Ordering::SeqCst),
            usize::from(local),
            "F2 hint only after relay"
        );
        publication.publish_staged(&fixture.owner).await.unwrap();
        let rows = fixture.owner.list(&staged.thread_id).await.unwrap();
        assert_eq!(rows.len(), 1, "F3 retry has one effect");
        assert_eq!(rows[0].input, staged, "F2/F3 exact original staged payload");
    }
}
