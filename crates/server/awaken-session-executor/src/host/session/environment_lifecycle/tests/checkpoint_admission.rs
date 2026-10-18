//! Checkpoint upload authority and suspension/terminal admission.

use super::*;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_execution_contract::RealizationEffects as _;

struct PendingUploadStore {
    puts: std::sync::atomic::AtomicUsize,
    gate: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxCheckpointStore for PendingUploadStore {
    async fn put(
        &self,
        _metadata: &awaken_provisioning_contract::CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<
        awaken_provisioning_contract::StoredCheckpointObject,
        awaken_provisioning_contract::SandboxError,
    > {
        self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let receipt = awaken_provisioning_contract::StoredCheckpointObject {
            id: "exact-upload".into(),
            digest: awaken_resource_contract::content_id(&bytes),
            size_bytes: bytes.len() as u64,
        };
        let _permit = self.gate.acquire().await.unwrap();
        Ok(receipt)
    }
    async fn get(&self, _: &str) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        panic!("upload must not read another checkpoint")
    }
    async fn delete(&self, _: &str) -> Result<(), awaken_provisioning_contract::SandboxError> {
        panic!("ordinary upload must not dispose a checkpoint")
    }
}

#[tokio::test]
async fn checkpoint_upload_host_preserves_original_authority_across_lifecycle_and_store_waits() {
    use crate::realization_state::SessionRealizationAuthority as Authority;
    use awaken_provisioning_contract as pc;
    use awaken_provisioning_contract::test_support::FenceTamper as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    // Host-to-real-Local decision table H: direct/registered topology; lifecycle
    // lock/put-response wait; current/Session close/same-byte slot replacement/
    // Worker close/Worker expiry/renewal. Worker-only causes exclude direct Host.
    // H1 current/renew -> real snapshot and receipt with unchanged Control time.
    // H2 loss at lifecycle -> zero uploads. H3 loss at put response -> no receipt;
    // exact fresh retry reaches cached WAL or repeats the original pending key.
    // H4 same-byte new slot must not rebind the authority captured BEFORE wait.
    // Store here is a response gate, not a durability proof; Local's store-CAS
    // table independently checks replay bytes and key. No mock Sandbox path.
    for registered in [false, true] {
        for wait in ["lifecycle", "put"] {
            for change in [
                "current",
                "session-close",
                "slot-replace",
                "worker-close",
                "worker-expire",
                "renew",
            ] {
                if !registered && change.starts_with("worker-") {
                    continue;
                }
                let thread = "current-upload-host";
                let root = tempfile::tempdir().unwrap();
                let provider =
                    awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
                        .into_session_provider();
                let environment = environment(&provider, thread).await;
                let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
                let lease = session_resource::SessionRealizationLease {
                    owner: "upload-worker".into(),
                    runtime_incarnation: "boot".into(),
                    epoch: 2,
                    expires_at_unix_ms: 30_000,
                };
                let generation = generation(thread);
                let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
                    "workspace",
                    &awaken_session_types::SessionId::from(thread),
                    "suspend",
                    &generation,
                    1,
                    Some(lease.clone()),
                    None,
                );
                let fence = lease.sandbox_effect_fence(&operation.effect_id).unwrap();
                let worker = Arc::new(pc::test_support::EffectAuthority::new(&fence));
                let store = Arc::new(PendingUploadStore {
                    puts: AtomicUsize::new(0),
                    gate: tokio::sync::Semaphore::new(usize::from(wait == "lifecycle")),
                });
                let mut raw = crate::SessionExecutorBuilder::ephemeral(
                    Arc::new(crate::no_model::NoModelConfiguredExecutor),
                    "stub",
                )
                .await
                .with_environment_checkpoint_store(store.clone());
                if registered {
                    raw = raw.with_execution_authority(worker.clone());
                }
                let host = (raw).build().expect("validated Host dependencies");
                let install = || {
                    host.run_assembly.installed_sessions.install_realization(thread, |r| {
                        r.environment_mut().owner = resident(thread, environment.clone());
                        r.execution_mut().authority = Authority::Live(
                            awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                                lease.clone(),
                                0,
                            )
                            .unwrap()
                            .observed(awaken_authority_clock::authority_proof_started().unwrap()),
                        );
                    })
                };
                install();
                let request = awaken_session_state_contract::SandboxCheckpointRequest {
                    workspace_id: "workspace".into(),
                    session_id: thread.into(),
                    operation,
                    source_effect_id: format!("effect-{thread}"),
                    source_binding: binding,
                    generation,
                    format: "awaken-fs-tar-v1".into(),
                    created_at_unix_ms: 10,
                    expires_at_unix_ms: 20_000,
                    max_bytes: 1024 * 1024,
                };
                let managed = host.clone();
                let lifecycle = host
                    .run_assembly
                    .installed_sessions
                    .realizations()
                    .read_realization(thread, |r| r.environment().lifecycle.clone())
                    .expect("installed Sessions")
                    .unwrap();
                let held = if wait == "lifecycle" {
                    Some(lifecycle.lock().await)
                } else {
                    None
                };
                let mut pending = Box::pin(
                    managed
                        .realization_executor
                        .checkpoint_session_environment(thread, request.clone()),
                );
                std::future::poll_fn(|cx| {
                    assert!(
                        std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                        "H wait {wait}/{change}"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                assert_eq!(
                    store.puts.load(Ordering::SeqCst),
                    usize::from(wait == "put")
                );
                match change {
                    "session-close" => host
                        .run_assembly
                        .installed_sessions
                        .realizations()
                        .update_realization(thread, |r| {
                            r.execution_mut().authority = Authority::Closed(Some(lease.clone()))
                        })
                        .expect("installed Sessions"),
                    "slot-replace" => {
                        host.run_assembly.installed_sessions.remove(thread);
                        install();
                    }
                    "worker-close" => *worker.current.lock().unwrap() = None,
                    "worker-expire" => {
                        worker.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO
                    }
                    "renew" => {
                        let mut renewed = lease.clone();
                        renewed.expires_at_unix_ms += 1;
                        host.run_assembly.installed_sessions.realizations().update_realization(thread, |r| {
                            r.execution_mut().authority = Authority::Live(
                                awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                                    renewed, 0,
                                )
                                .unwrap()
                                .observed(awaken_authority_clock::authority_proof_started().unwrap()),
                            )
                        }).expect("installed Sessions");
                        {
                            let mut guard = worker.current.lock().unwrap();
                            guard.as_mut().unwrap().0.renew();
                        };
                    }
                    _ => {}
                }
                drop(held);
                store.gate.add_permits(1);
                let result = pending.await;
                let admitted = matches!(change, "current" | "renew");
                assert_eq!(
                    result.is_ok(),
                    admitted,
                    "H {registered}/{wait}/{change}: {result:?}"
                );
                assert_eq!(
                    store.puts.load(Ordering::SeqCst),
                    usize::from(wait == "put" || admitted)
                );
                if let Ok(receipt) = result {
                    assert_eq!(
                        receipt.checkpoint.expires_at_unix_ms,
                        request.expires_at_unix_ms
                    );
                }
                if !admitted {
                    // Independent fresh retry after denial; renewal rows retain
                    // their newer live receipt, proving expiry is not a WAL key.
                    install();
                    *worker.current.lock().unwrap() = Some((fence, Duration::from_secs(1)));
                }
                managed
                    .realization_executor
                    .checkpoint_session_environment(thread, request)
                    .await
                    .unwrap();
                assert_eq!(
                    store.puts.load(Ordering::SeqCst),
                    if wait == "put" && !admitted { 2 } else { 1 }
                );
            }
        }
    }
}

#[tokio::test]
async fn terminal_committed_checkpoint_requires_current_worker_and_session_after_delete() {
    use crate::host::tests::{
        RecordingTerminalCheckpointStore, disposing_terminal_environment,
        terminal_fixture_container_handle, terminal_recovery_effect,
    };
    use awaken_provisioning_contract as pc;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    // C1 committed checkpoint (no Sandbox reconstruction); C2 delete has
    // committed but its response is pending; C3 Worker revoke/expiry, Session
    // revoke/replacement, same-generation renewal, or caller cancellation.
    // Six schedules: R1 denied entry calls no store; R2 a late delete response
    // cannot complete under lost authority; R3 renewal may complete; R4 cancel
    // does not produce a receipt. R5 exact authorized retry uses the SAME id.
    // No get/put/provider is installed: this isolates the committed-object
    // branch from the pending-upload port and cannot accidentally test a mock
    // provider's independent authorization instead of the Host composition.
    // The fixture observes delete calls, not remote atomic commit fencing.
    for change in [
        "worker-revoke",
        "worker-expire",
        "session-revoke",
        "replace",
        "renew",
        "cancel",
    ] {
        let thread = "current-committed-checkpoint";
        let (lease, effect) = terminal_recovery_effect(thread, "checkpoint-worker");
        let fence = effect.sandbox_effect_fence().unwrap();
        let worker = Arc::new(pc::test_support::EffectAuthority::new(&fence));
        let events = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let host = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await
        .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: "exact-checkpoint".into(),
            delete_gate: Some(gate.clone()),
        }))
        .with_execution_authority(worker.clone()))
        .build()
        .expect("validated Host dependencies");
        host.run_assembly
            .installed_sessions
            .install_realization(thread, |r| {
                r.execution_mut().authority =
                    crate::realization_state::SessionRealizationAuthority::Live(
                        awaken_session_realization::test_support::test_observed_lease(
                            lease.clone(),
                        ),
                    );
            });
        let handle = terminal_fixture_container_handle(
            thread,
            &host.realization_executor.sandbox_spec(thread),
        );
        let (state, _, _) =
            disposing_terminal_environment(thread, &lease, &handle, "exact-checkpoint");
        *worker.current.lock().unwrap() = None;
        assert!(
            host.realization_executor
                .effects()
                .cleanup_terminal_checkpoint(
                    host.realization_executor.peers(),
                    &effect,
                    &state,
                    None,
                    None
                )
                .await
                .is_err(),
            "R1"
        );
        assert!(events.lock().unwrap().is_empty(), "R1");
        *worker.current.lock().unwrap() = Some((fence.clone(), Duration::from_secs(1)));
        let mut pending = Box::pin(
            host.realization_executor
                .effects()
                .cleanup_terminal_checkpoint(
                    host.realization_executor.peers(),
                    &effect,
                    &state,
                    None,
                    None,
                ),
        );
        tokio::select! {
            biased;
            result = &mut pending => panic!("C2 must wait: {result:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(events.lock().unwrap().as_slice(), &["delete-checkpoint"]);
        match change {
            "worker-revoke" => *worker.current.lock().unwrap() = None,
            "worker-expire" => worker.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO,
            "session-revoke" => host
                .run_assembly
                .installed_sessions
                .realizations()
                .update_realization(thread, |r| {
                    r.execution_mut().authority =
                        crate::realization_state::SessionRealizationAuthority::Closed(Some(
                            lease.clone(),
                        ));
                })
                .expect("installed Sessions"),
            "replace" | "renew" => host
                .run_assembly
                .installed_sessions
                .realizations()
                .update_realization(thread, |r| {
                    let mut next = lease.clone();
                    if change == "replace" {
                        next.epoch += 1;
                    }
                    next.expires_at_unix_ms += 10_000;
                    r.execution_mut().authority =
                        crate::realization_state::SessionRealizationAuthority::Live(
                            awaken_session_realization::test_support::test_observed_lease(next),
                        );
                })
                .expect("installed Sessions"),
            "cancel" => {}
            _ => unreachable!(),
        }
        if change == "cancel" {
            drop(pending);
            gate.add_permits(1);
        } else {
            gate.add_permits(1);
            assert_eq!(pending.await.is_ok(), change == "renew", "R2/R3");
        }
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "R2/R4 no next store operation"
        );
        // Reset only the fixture for exact-object retry; production renewal
        // cannot revive a closed/replaced Worker or Session incarnation.
        *worker.current.lock().unwrap() = Some((fence.clone(), Duration::from_secs(1)));
        host.run_assembly
            .installed_sessions
            .realizations()
            .update_realization(thread, |r| {
                r.execution_mut().authority =
                    crate::realization_state::SessionRealizationAuthority::Live(
                        awaken_session_realization::test_support::test_observed_lease(
                            lease.clone(),
                        ),
                    );
            })
            .expect("installed Sessions");
        host.realization_executor
            .effects()
            .cleanup_terminal_checkpoint(
                host.realization_executor.peers(),
                &effect,
                &state,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &["delete-checkpoint", "delete-checkpoint"],
            "R5"
        );
    }
}

#[tokio::test]
async fn suspension_and_terminal_admission_use_live_observation_and_never_reopen_closed_slots() {
    use crate::realization_state::SessionRealizationAuthority as Authority;
    use awaken_session_configuration_contract::resource::SessionRealizationLease;
    use awaken_session_realization_contract::SessionRealizationLeaseReceipt;

    // Cause/effect table: C1 absent/Uninstalled/Live/Closed root; C2 operation
    // fenced/unfenced; C3 exact/foreign/older/newer assertion; C4 observed proof
    // live/expired. E1 unfenced suspension is legacy-only on absent/Uninstalled;
    // E2 a fenced suspension or terminal effect requires C1 Live + C3 authorized
    // + C4 live; E3 all rejection rows leave the root unchanged (no resurrection).
    // The authority's absolute expiry is deliberately in the distant past of
    // this host's wall clock: receipt-relative liveness, not clock agreement,
    // decides E2. This covers Host admission, not provider fence acceptance.
    let current = SessionRealizationLease {
        owner: "control-owner".into(),
        runtime_incarnation: "control-incarnation".into(),
        epoch: 2,
        expires_at_unix_ms: 30_000,
    };
    for (rule, state, assertion, suspension, terminal) in [
        ("S1", "absent", "none", true, false),
        ("S2", "uninstalled", "none", true, false),
        ("S3", "closed-empty", "none", false, false),
        ("S4", "closed", "none", false, false),
        ("S5", "live", "none", false, true),
        ("S6", "live", "same", true, true),
        ("S7", "live", "older", true, true),
        ("S8", "live", "newer", false, false),
        ("S9", "live", "owner", false, false),
        ("S10", "live", "incarnation", false, false),
        ("S11", "live", "epoch", false, false),
        ("S12", "expired", "same", false, false),
        ("S13", "closed", "same", false, false),
        ("S14", "uninstalled", "same", false, false),
        ("S15", "absent", "same", false, false),
    ] {
        let host = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await)
            .build()
            .expect("validated Host dependencies");
        let root = match state {
            "absent" | "uninstalled" => Authority::Uninstalled,
            "closed-empty" => Authority::Closed(None),
            "closed" => Authority::Closed(Some(current.clone())),
            "expired" => Authority::Live(
                awaken_session_realization::test_support::test_expired_observed_lease(
                    current.clone(),
                ),
            ),
            "live" => Authority::Live(
                SessionRealizationLeaseReceipt::issued_at(current.clone(), 0)
                    .unwrap()
                    .observed(awaken_authority_clock::authority_proof_started().unwrap()),
            ),
            _ => unreachable!(),
        };
        if state != "absent" {
            host.run_assembly
                .installed_sessions
                .install_realization(rule, |r| r.execution_mut().authority = root.clone());
        }
        let mut asserted = current.clone();
        match assertion {
            "older" => asserted.expires_at_unix_ms -= 1,
            "newer" => asserted.expires_at_unix_ms += 1,
            "owner" => asserted.owner.push_str("-foreign"),
            "incarnation" => asserted.runtime_incarnation.push_str("-foreign"),
            "epoch" => asserted.epoch += 1,
            "none" | "same" => (),
            _ => unreachable!(),
        }
        let generation = generation(rule);
        let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
            "workspace",
            &awaken_session_types::SessionId::from(rule),
            "suspend",
            &generation,
            1,
            (assertion != "none").then_some(asserted.clone()),
            None,
        );
        assert_eq!(
            host.realization_executor
                .environments()
                .validate_suspend_operation(rule, &operation, &generation)
                .is_ok(),
            suspension,
            "{rule}/suspend"
        );
        assert_eq!(
            host.realization_executor
                .effects()
                .authorize_lease(&awaken_session_types::SessionId::from(rule), &asserted)
                .is_ok(),
            terminal,
            "{rule}/terminal"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(rule, |r| r.execution().authority.clone())
                .expect("installed Sessions"),
            (state != "absent").then_some(root),
            "{rule}/no root mutation"
        );
    }
}
