use super::state::ProjectedEnvironmentOwner;
use super::*;
use crate::session_slot::UnboundSessionEnvironmentOrigin;

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
    use crate::session_slot::SessionRealizationAuthority as Authority;
    use awaken_provisioning_contract as pc;
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
                    crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
                let environment = environment(&provider, thread).await;
                let binding = serde_json::to_string(&environment.handle()).unwrap();
                let lease = awaken_session_contract::SessionRealizationLease {
                    owner: "upload-worker".into(),
                    runtime_incarnation: "boot".into(),
                    epoch: 2,
                    expires_at_unix_ms: 30_000,
                };
                let generation = generation(thread);
                let operation = awaken_session_contract::SessionEnvironmentOperation::new(
                    "workspace",
                    thread,
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
                let mut raw =
                    SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
                        .with_environment_checkpoint_store(store.clone());
                if registered {
                    raw.execution_authority = Some(worker.clone());
                }
                let host = Arc::new(raw);
                let install = || {
                    host.session_slots.update(thread, |slot| {
                        slot.environment_owner = resident(thread, environment.clone());
                        slot.realization_authority = Authority::Live(
                            awaken_session_contract::SessionRealizationLeaseReceipt::issued_at(
                                lease.clone(),
                                0,
                            )
                            .unwrap()
                            .observed(awaken_local_process::authority_proof_started().unwrap()),
                        );
                    })
                };
                install();
                let request = awaken_session_contract::SandboxCheckpointRequest {
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
                let managed = crate::ManagedHost::new(host.clone());
                let lifecycle = host
                    .session_slots
                    .read(thread, |slot| slot.lifecycle.clone())
                    .unwrap();
                let held = if wait == "lifecycle" {
                    Some(lifecycle.lock().await)
                } else {
                    None
                };
                let mut pending =
                    Box::pin(managed.checkpoint_environment_continuation(thread, request.clone()));
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
                    "session-close" => host.session_slots.update(thread, |slot| {
                        slot.realization_authority = Authority::Closed(Some(lease.clone()))
                    }),
                    "slot-replace" => {
                        host.session_slots.remove(thread);
                        install();
                    }
                    "worker-close" => *worker.current.lock().unwrap() = None,
                    "worker-expire" => {
                        worker.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO
                    }
                    "renew" => {
                        let mut renewed = lease.clone();
                        renewed.expires_at_unix_ms += 1;
                        host.session_slots.update(thread, |slot| {
                            slot.realization_authority = Authority::Live(
                                awaken_session_contract::SessionRealizationLeaseReceipt::issued_at(
                                    renewed, 0,
                                )
                                .unwrap()
                                .observed(awaken_local_process::authority_proof_started().unwrap()),
                            )
                        });
                        worker
                            .current
                            .lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .0
                            .expires_at_unix_ms += 1;
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
                    .checkpoint_environment_continuation(thread, request)
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
        let mut host =
            SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
                .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
                    events: events.clone(),
                    expected_id: "exact-checkpoint".into(),
                    delete_gate: Some(gate.clone()),
                }));
        host.execution_authority = Some(worker.clone());
        host.session_slots.update(thread, |slot| {
            slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
                crate::session_slot::test_observed_lease(lease.clone()),
            );
        });
        let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
        let (state, _, _) =
            disposing_terminal_environment(thread, &lease, &handle, "exact-checkpoint");
        *worker.current.lock().unwrap() = None;
        assert!(
            host.cleanup_terminal_checkpoint(&effect, &state, None, None)
                .await
                .is_err(),
            "R1"
        );
        assert!(events.lock().unwrap().is_empty(), "R1");
        *worker.current.lock().unwrap() = Some((fence.clone(), Duration::from_secs(1)));
        let mut pending = Box::pin(host.cleanup_terminal_checkpoint(&effect, &state, None, None));
        tokio::select! {
            biased;
            result = &mut pending => panic!("C2 must wait: {result:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(events.lock().unwrap().as_slice(), &["delete-checkpoint"]);
        match change {
            "worker-revoke" => *worker.current.lock().unwrap() = None,
            "worker-expire" => worker.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO,
            "session-revoke" => host.session_slots.update(thread, |slot| {
                slot.realization_authority =
                    crate::session_slot::SessionRealizationAuthority::Closed(Some(lease.clone()));
            }),
            "replace" | "renew" => host.session_slots.update(thread, |slot| {
                let mut next = lease.clone();
                if change == "replace" {
                    next.epoch += 1;
                }
                next.expires_at_unix_ms += 10_000;
                slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
                    crate::session_slot::test_observed_lease(next),
                );
            }),
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
        host.session_slots.update(thread, |slot| {
            slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
                crate::session_slot::test_observed_lease(lease.clone()),
            );
        });
        host.cleanup_terminal_checkpoint(&effect, &state, None, None)
            .await
            .unwrap();
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &["delete-checkpoint", "delete-checkpoint"],
            "R5"
        );
    }
}

fn environment_effect_fence(thread: &str) -> awaken_provisioning_contract::SandboxEffectFence {
    awaken_provisioning_contract::SandboxEffectFence::new(
        format!("environment-lifecycle-fixture:{thread}"),
        "environment-lifecycle-tests",
        "environment-lifecycle-tests",
        1,
        u64::MAX,
    )
    .expect("test Environment effect fence")
}

async fn environment(
    provider: &crate::session_environment::SessionEnvironmentProvider,
    thread: &str,
) -> Arc<crate::session_environment::SessionEnvironment> {
    // Fixture cause/effect rule: C1 lifecycle tests later perform exact,
    // spec-aware adoption; therefore C2 creation must carry the same current
    // Realization fingerprint and incarnation evidence as production. E1 use
    // the canonical fenced provider edge and emit one V2 handle. A legacy
    // unfenced create would test the decode-only V1 compatibility path instead.
    let effect_fence = environment_effect_fence(thread);
    Arc::new(
        provider
            .create_effective_for_effect(
                &crate::provisioning::agent_run_sandbox_spec(thread),
                Some(&effect_fence),
                Some(
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &effect_fence,
                    ),
                ),
                None,
                awaken_sandbox_container::ContainerRealizationIntent::Create,
            )
            .await
            .expect("test Environment"),
    )
}

fn disposal_authorization(
    prepared: awaken_provisioning_contract::SandboxEffectFence,
    preparation_fingerprint: &str,
) -> awaken_provisioning_contract::SandboxDisposalAuthorization {
    let preparation = awaken_provisioning_contract::SandboxDisposalPreparation::new(
        prepared.clone(),
        preparation_fingerprint,
    )
    .unwrap();
    let current = awaken_provisioning_contract::SandboxEffectFence::new(
        preparation.operation_id().unwrap(),
        prepared.owner,
        prepared.runtime_incarnation,
        prepared.epoch,
        prepared.expires_at_unix_ms,
    )
    .unwrap();
    preparation.authorize(current).unwrap()
}

async fn dispose_current_environment(
    environment: &crate::session_environment::SessionEnvironment,
    thread: &str,
) {
    // Fixture cause/effect rule: C1 a current V2 Environment may not cross the
    // legacy unfenced disposal edge; C2 provider preparation must retain the
    // exact create-effect predecessor. E1 prepare that current predecessor,
    // derive the typed aggregate authorization, then perform physical disposal.
    let prepared = environment
        .prepare_disposal_for_effect(
            &environment_effect_fence(thread),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                &environment_effect_fence(thread),
            ),
        )
        .await
        .expect("prepare current Environment disposal");
    let authorization = disposal_authorization(
        prepared,
        "environment-lifecycle-current-disposal-preparation",
    );
    environment
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("dispose current Environment through aggregate effect fence");
}

fn generation(thread: &str) -> awaken_session_contract::SandboxGeneration {
    awaken_session_contract::SandboxGeneration::new(thread, 1, u64::MAX, "environment", "image")
}

#[test]
fn suspension_and_terminal_admission_use_live_observation_and_never_reopen_closed_slots() {
    use crate::session_slot::SessionRealizationAuthority as Authority;
    use awaken_session_contract::{SessionRealizationLease, SessionRealizationLeaseReceipt};

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
        let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
        let root = match state {
            "absent" | "uninstalled" => Authority::Uninstalled,
            "closed-empty" => Authority::Closed(None),
            "closed" => Authority::Closed(Some(current.clone())),
            "expired" => Authority::Live(crate::session_slot::test_expired_observed_lease(
                current.clone(),
            )),
            "live" => Authority::Live(
                SessionRealizationLeaseReceipt::issued_at(current.clone(), 0)
                    .unwrap()
                    .observed(awaken_local_process::authority_proof_started().unwrap()),
            ),
            _ => unreachable!(),
        };
        if state != "absent" {
            host.session_slots
                .update(rule, |slot| slot.realization_authority = root.clone());
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
        let operation = awaken_session_contract::SessionEnvironmentOperation::new(
            "workspace",
            rule,
            "suspend",
            &generation,
            1,
            (assertion != "none").then_some(asserted.clone()),
            None,
        );
        assert_eq!(
            host.validate_suspend_operation(rule, &operation, &generation)
                .is_ok(),
            suspension,
            "{rule}/suspend"
        );
        assert_eq!(
            host.authorize_terminal_cleanup_lease(rule, &asserted)
                .is_ok(),
            terminal,
            "{rule}/terminal"
        );
        assert_eq!(
            host.session_slots
                .read(rule, |slot| slot.realization_authority.clone()),
            (state != "absent").then_some(root),
            "{rule}/no root mutation"
        );
    }
}

fn identity(thread: &str) -> BoundSessionEnvironmentIdentity {
    BoundSessionEnvironmentIdentity::Durable {
        effect_id: format!("effect-{thread}"),
        generation: generation(thread),
    }
}

fn source_disposal_authorization(
    preparation_operation_id: &str,
) -> awaken_provisioning_contract::SandboxDisposalAuthorization {
    let prepared = awaken_provisioning_contract::SandboxEffectFence::new(
        preparation_operation_id,
        "source-disposal-test-owner",
        "source-disposal-test-runtime",
        1,
        u64::MAX,
    )
    .unwrap();
    disposal_authorization(prepared, "source-disposal-test-preparation")
}

async fn install_active_worker_relay(
    host: &SharedHost,
    thread: &str,
) -> (
    crate::mcp_relay::McpRelay,
    awaken_session_contract::McpGenerationRef,
) {
    let generation = awaken_session_contract::McpGenerationRef {
        session_id: thread.into(),
        attachment_id: awaken_session_contract::McpAttachmentId("docs".into()),
        generation: awaken_session_contract::McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let request = awaken_session_contract::StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test/sse")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let server = crate::mcp::McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: crate::mcp::McpTransportMaterialKind::Http {
            url: "https://mcp.example.test/sse".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("relay-secret")),
            refresh: None,
        },
    };
    let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
    assert!(host.mcp_relay.set(relay.clone()).is_ok());
    relay.set_route(&generation, &server);
    host.insert_mcp_projection(crate::session_slot::McpGenerationProjection {
        receipt: awaken_session_contract::McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: Some(
                awaken_runtime_contract::CredentialRealizationKind::WorkerRelay,
            ),
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: Some(server),
        native_wiring: None,
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: crate::session_slot::McpProjectionState::Active,
    })
    .unwrap();
    (relay, generation)
}

struct NonOwningEnvironmentBindingSink;

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for NonOwningEnvironmentBindingSink {
    async fn authorize(
        &self,
        _intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned)
    }

    async fn persist(
        &self,
        _receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        panic!("a non-owning binding sink must not persist")
    }
}

fn restore_request(thread: &str) -> awaken_session_contract::SandboxRestoreRequest {
    let generation = generation(thread);
    let suspend = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        6,
        None,
        None,
    );
    let checkpoint = awaken_session_contract::SandboxCheckpointRef {
        id: format!("checkpoint-{thread}"),
        format: "portable-test-v1".into(),
        digest: format!("digest-{thread}"),
        size_bytes: 7,
        created_at_unix_ms: 10,
        expires_at_unix_ms: u64::MAX,
        environment_fingerprint: generation.environment_fingerprint.clone(),
        base_image_fingerprint: generation.base_image_fingerprint.clone(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: suspend.effect_id,
    };
    let restore = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "restore",
        &generation,
        7,
        None,
        Some(&checkpoint),
    );
    awaken_session_contract::SandboxRestoreRequest {
        workspace_id: "workspace".into(),
        session_id: thread.into(),
        effect_id: restore.effect_id,
        generation_id: generation.id,
        checkpoint,
    }
}

#[test]
fn restoring_owner_is_only_an_exact_request_fence_until_durable_adoption() {
    // Cause/effect table: C1 Vacant receives exact Restoring request A; C2 A is
    // replayed; C3 request B or mismatched Resident authority arrives; C4 the
    // aggregate commits Resident for A. Effects: E1 one hidden Awaiting fence
    // with no Arc; E2 no-write replay; E3 reject and retain A; E4 move only to
    // spec-aware pending durable adoption, never Resident. Rules: F1=C1=>E1,
    // F2=C2=>E2, F3=C3=>E3, F4=C4=>E4. Terminal physical disposal deliberately
    // does not mutate this owner: aggregate acknowledgement is the sole final
    // projection-retirement edge and is covered by the Host terminal table.
    let request = restore_request("restore-owner");
    let mut owner = SessionEnvironmentOwner::Vacant;
    owner.begin_restore(&request).expect("F1/E1");
    assert!(!owner.has_local_environment(), "F1/E1 no local Arc");
    owner.begin_restore(&request).expect("F2/E2");

    let other = restore_request("other-restore-owner");
    assert!(owner.begin_restore(&other).is_err(), "F3/E3");

    let generation = generation("restore-owner");
    let exact_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: request.effect_id.clone(),
        generation: generation.clone(),
    };
    let mismatched_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: other.effect_id.clone(),
        generation: generation.clone(),
    };
    assert!(
        owner
            .install_projection(
                ProjectedEnvironmentOwner::AwaitingAdoption {
                    identity: mismatched_identity,
                    binding: "binding".into(),
                },
                false
            )
            .is_err(),
        "F3/E3"
    );
    owner
        .install_projection(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: exact_identity,
                binding: "binding".into(),
            },
            false,
        )
        .expect("F4/E4");
    assert!(matches!(
        owner,
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
    ));
}

#[test]
fn closed_pending_adoption_discards_only_the_pre_observation_identity() {
    // Cause/effect table: C1 pending durable owner A is captured before a
    // provider observation; C2 same-binding identity B replaces A; C3 closed
    // evidence returns for A. E1 stale A cannot clear B. With C1 still current,
    // E2 the same evidence clears only A so rebuild can proceed.
    // | Rule | Current owner | Observed identity | Effect |
    // | P1 | B, same binding | A | retain B |
    // | P2 | A | A | Vacant |
    let thread = "closed-pending-adoption-fence";
    let binding = "exact-binding";
    let observed = identity(thread);
    let replacement = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "replacement-effect".into(),
        generation: generation(thread),
    };
    let mut owner = SessionEnvironmentOwner::Vacant;
    owner
        .install_projection(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: replacement.clone(),
                binding: binding.into(),
            },
            false,
        )
        .expect("P1 replacement projection");
    assert!(
        !owner.discard_closed_pending_adoption(&observed, binding),
        "P1/E1"
    );
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
            identity,
            binding: current,
        }) if identity == &replacement && current == binding
    ));

    owner = SessionEnvironmentOwner::Vacant;
    owner
        .install_projection(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: observed.clone(),
                binding: binding.into(),
            },
            false,
        )
        .expect("P2 observed projection");
    assert!(
        owner.discard_closed_pending_adoption(&observed, binding),
        "P2/E2"
    );
    assert!(matches!(owner, SessionEnvironmentOwner::Vacant), "P2/E2");
}

#[tokio::test]
async fn resident_owner_absorbs_only_its_exact_monotonic_resource_reservation() {
    // Resident reservation cause/effect graph. Causes: C1 a durable Resident
    // retains one exact Arc, identity and V2 binding; C2 the provider Arc has
    // actually reserved a superset of owned paths; C3 the root projection keeps
    // the same durable generation; C4 the projected handle names the same or a
    // different substrate. Effects: E1 C1+C2+C3+same substrate atomically
    // advances identity+binding on that one owner while retaining the Arc; C5 a
    // delayed root observation carries another effect identity after the same
    // handle was already projected. E2 a foreign substrate is rejected with the
    // owner unchanged; E3 C5 is absorbed without rolling the locally published
    // identity backward. Exact replay is already covered by
    // ordinary projection tests, and the provisioning contract owns
    // path-loss/legacy/different-generation negatives.
    //
    // | Rule | Arc reserved | generation | substrate | Effect |
    // | P1 | yes | same | same | E1 |
    // | P2 | yes | same | different | E2 |
    // | P3 | already projected | same | same | E3 |
    let thread = "resident-resource-reservation";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let resident_environment = environment(&provider, thread).await;
    let initial_binding = serde_json::to_string(&resident_environment.handle()).unwrap();
    let initial_identity = identity(thread);
    let mut owner = SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
        identity: initial_identity,
        binding: initial_binding,
        environment: resident_environment.clone(),
    });

    resident_environment
        .reserve_owned_path("/workspace/live.txt")
        .expect("P1 reserve provider-owned path");
    let reserved_binding = serde_json::to_string(&resident_environment.handle()).unwrap();
    let reserved_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "resource-reservation-effect".into(),
        generation: generation(thread),
    };
    owner
        .install_projection(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: reserved_identity.clone(),
                binding: reserved_binding.clone(),
            },
            false,
        )
        .expect("P1 absorb exact root reservation");
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Resident(owned)
            if owned.identity == reserved_identity
                && owned.binding == reserved_binding
                && Arc::ptr_eq(&owned.environment, &resident_environment)
    ));

    let same_binding_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "same-binding-resource-receipt".into(),
        generation: generation(thread),
    };
    owner
        .install_projection(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: same_binding_identity.clone(),
                binding: reserved_binding.clone(),
            },
            false,
        )
        .expect("P3 absorb an unordered receipt for the same physical handle");
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Resident(owned)
            if owned.identity == reserved_identity
                && owned.binding == reserved_binding
                && Arc::ptr_eq(&owned.environment, &resident_environment)
    ));

    let foreign = environment(&provider, "foreign-resource-reservation").await;
    foreign
        .reserve_owned_path("/workspace/foreign.txt")
        .expect("P2 reserve foreign path");
    let foreign_binding = serde_json::to_string(&foreign.handle()).unwrap();
    assert!(
        owner
            .install_projection(
                ProjectedEnvironmentOwner::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "foreign-resource-reservation-effect".into(),
                        generation: generation(thread),
                    },
                    binding: foreign_binding,
                },
                false
            )
            .is_err(),
        "P2 foreign substrate remains fenced"
    );
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Resident(owned)
            if owned.identity == reserved_identity
                && owned.binding == reserved_binding
                && Arc::ptr_eq(&owned.environment, &resident_environment)
    ));
}

#[tokio::test]
async fn realization_revocation_absorbs_only_a_compatible_durable_receipt() {
    // Revoked-owner cause/effect graph. C1 a durable Resident enters the sole
    // RealizationRevocation phase; C2 Control has committed a later Resource
    // receipt for the same generation and exact physical Arc; C3 a projection
    // instead names a different generation. E1 C1+C2 absorbs the unordered
    // receipt while preserving the exact Retiring fence and Arc; E2 C1+C3
    // rejects and preserves that revoked owner.
    //
    // | Rule | cause | generation/handle | Effect |
    // | V1 | realization revocation | same | E1, retain fence |
    // | V2 | realization revocation | different | E2 |
    let thread = "revoked-resource-receipt";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.handle()).unwrap();
    let mut owner = resident(thread, environment.clone());
    let retirement = owner
        .begin_retirement(
            SessionEnvironmentRetirementCause::RealizationRevocation,
            RetirementSelection::Current,
        )
        .unwrap()
        .expect("V1 revoked owner");

    let replacement = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "committed-resource-receipt".into(),
        generation: generation(thread),
    };
    owner
        .install_projection(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: replacement.clone(),
                binding: binding.clone(),
            },
            false,
        )
        .expect("V1 same physical generation absorbs its receipt");
    let retained = owner.clone();
    assert!(
        matches!(&owner, SessionEnvironmentOwner::Retiring(current)
            if current.exact_matches(&retirement)),
        "V1 unordered same-binding observation retains the exact retirement fence"
    );

    assert!(
        owner
            .install_projection(
                ProjectedEnvironmentOwner::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "foreign-generation-receipt".into(),
                        generation: awaken_session_contract::SandboxGeneration::new(
                            thread,
                            2,
                            u64::MAX,
                            "environment",
                            "image",
                        ),
                    },
                    binding,
                },
                false
            )
            .is_err(),
        "V2 different generation is rejected"
    );
    assert!(matches!(
        (&owner, &retained),
        (SessionEnvironmentOwner::Retiring(current), SessionEnvironmentOwner::Retiring(before))
            if current.exact_matches(before)
    ));
}

fn retry_input_mount() -> awaken_provisioning_contract::MountRequirement {
    awaken_provisioning_contract::MountRequirement {
        mount_id: "retry-input".into(),
        source: awaken_provisioning_contract::MountSource::InlineBytes {
            contents: b"retry".to_vec(),
            content_hash: None,
        },
        mount_path: ".mnt/retry-input".into(),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::Session,
        required: true,
    }
}

fn resident(
    thread: &str,
    environment: Arc<crate::session_environment::SessionEnvironment>,
) -> SessionEnvironmentOwner {
    SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
        identity: identity(thread),
        binding: serde_json::to_string(&environment.handle()).unwrap(),
        environment,
    })
}

#[tokio::test]
async fn cancelled_cleanup_keeps_the_exact_owner_retiring_and_hidden_from_tools() {
    // Cause/effect graph: C1 Resident exact owner; C2 cleanup changes phase
    // before its first fallible/cancellable provider await; C3 that Future
    // is cancelled; C4 a different terminal effect tries to consume it.
    // Effects: E1 external reader stops seeing the owner at C2; E2 C3 retains
    // phase+effect+generation+binding+Arc for retry; E3 C4 cannot relabel it.
    // Decision rules R18/R19: C1+C2+C3 => E1+E2; C2+C4 => E3.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, "cancel-retirement").await;
    let owner = Arc::new(tokio::sync::Mutex::new(resident(
        "cancel-retirement",
        environment,
    )));
    let entered = Arc::new(tokio::sync::Notify::new());
    let task = {
        let owner = owner.clone();
        let entered = entered.clone();
        tokio::spawn(async move {
            let retirement = {
                let mut owner = owner.lock().await;
                let retirement = owner
                    .begin_retirement(
                        SessionEnvironmentRetirementCause::Terminal {
                            effect_id: "terminal-effect".into(),
                        },
                        RetirementSelection::Current,
                    )
                    .unwrap()
                    .unwrap();
                assert!(owner.resident().is_none(), "R18/E1");
                retirement
            };
            entered.notify_one();
            std::future::pending::<()>().await;
            retirement
        })
    };
    entered.notified().await;
    task.abort();
    let _ = task.await;
    let mut owner = owner.lock().await;
    assert!(
        matches!(&*owner, SessionEnvironmentOwner::Retiring(_)),
        "R19/E2"
    );
    assert!(owner.resident().is_none(), "R18/E1");
    assert!(
        owner
            .begin_retirement(
                SessionEnvironmentRetirementCause::Terminal {
                    effect_id: "different-terminal-effect".into(),
                },
                RetirementSelection::Current,
            )
            .is_err(),
        "R19/E3"
    );
    assert!(
        matches!(
            &*owner,
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::Terminal { effect_id },
                ..
            }) if effect_id == "terminal-effect"
        ),
        "R19/E3"
    );
}

#[tokio::test]
async fn source_free_projection_does_not_reopen_mcp_before_retiring_owner_is_disposed() {
    // Cause/effect table for checkpoint-expiry ordering: Q1 a quiescence fence
    // is closed while its source owner is Resident; Q2 cleanup has moved that
    // owner to Retiring but physical disposal is incomplete; Q3 the durable
    // projection is already source-free Unmaterialized; Q4 exact cleanup later
    // proves Terminated/Vacant. Effects: E1 Q2+Q3 retains the fence; E2 only
    // Q3+Q4 atomically consumes it, admitting the fresh realization generation.
    let thread = "expiry-before-source-dispose";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_test_resident_session_environment(thread, environment.clone());
    let generation = generation(thread);
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        2,
        None,
        None,
    );
    host.session_slots
        .close_mcp_realization_admission(
            thread,
            crate::session_slot::McpQuiescenceAdmissionFence::new(
                &operation,
                "source-effect",
                "source-binding",
                &generation,
            ),
        )
        .unwrap();

    assert!(
        !host.discard_session_environment(thread, &environment).await,
        "Q2 keeps a live source in Retiring"
    );
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    )
    .unwrap();
    assert!(
        !host.session_slots.mcp_realization_admitted(thread),
        "E1 expiry before disposal cannot reopen"
    );

    dispose_current_environment(&environment, thread).await;
    assert!(
        host.discard_session_environment(thread, &environment).await,
        "Q4 exact retry confirms Terminated/Vacant"
    );
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    )
    .unwrap();
    assert!(
        host.session_slots.mcp_realization_admitted(thread),
        "E2 exact source-free+Vacant projection admits a new realization"
    );
}

#[tokio::test]
async fn discard_keeps_live_owner_and_exact_retry_clears_after_termination() {
    // Cause/effect decision table: C1 discard observes exact Resident Arc;
    // C2 stop/status still reports Ready; C3 the same provider owner later
    // reports Terminated; C4 retry supplies that same Arc. Effects: E1 C2
    // returns false and retains hidden Retiring; E2 C3+C4 exact-clears. Rules
    // R19/R21: C1+C2=>E1; C1+C2+C3+C4=>E2. The separate ABA rule owns !C4.
    let thread = "discard-live-retry";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_test_resident_session_environment(thread, environment.clone());

    assert!(
        !host.discard_session_environment(thread, &environment).await,
        "R19/E1 live owner is retained"
    );
    assert!(
        host.session_environment(thread).await.is_none(),
        "R19/E1 hidden"
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(_))
        ),
        "R19/E1"
    );

    dispose_current_environment(&environment, thread).await;
    assert!(
        host.discard_session_environment(thread, &environment).await,
        "R21/E2 exact retry clears"
    );
    assert!(host.session_environment_owner_is_vacant(thread), "R21/E2");
}

#[tokio::test]
async fn only_exact_terminated_status_clears_a_retiring_owner() {
    // Cause/effect decision table: C1 exact Retiring owner; C2 status is
    // Provisioning/Ready; C3 status query is Err (unknown); C4 status is
    // Terminated. Effects: E1 C2 retains; E2 C3 returns Err and retains;
    // E3 only C4 clears. Rules R19/R21: C1+C2=>E1, C1+C3=>E2,
    // C1+C4=>E3. A dispose Err returns before this status gate and therefore
    // has the same retained-owner effect as C3.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, "status-retirement").await;
    let mut owner = resident("status-retirement", environment);
    let retirement = owner
        .begin_retirement(
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal-effect".into(),
            },
            RetirementSelection::Current,
        )
        .unwrap()
        .unwrap();

    assert!(
        !owner
            .observe_retirement_status::<()>(
                &retirement,
                Ok(awaken_provisioning_contract::SandboxStatus::Provisioning),
            )
            .unwrap(),
        "R19/E1"
    );
    assert!(
        !owner
            .observe_retirement_status::<()>(
                &retirement,
                Ok(awaken_provisioning_contract::SandboxStatus::Ready),
            )
            .unwrap(),
        "R19/E1"
    );
    assert!(
        owner
            .observe_retirement_status(&retirement, Err("unknown"))
            .is_err(),
        "R19/E2"
    );
    assert!(
        matches!(owner, SessionEnvironmentOwner::Retiring(_)),
        "R19/E1+E2"
    );
    assert!(
        owner
            .observe_retirement_status::<()>(
                &retirement,
                Ok(awaken_provisioning_contract::SandboxStatus::Terminated),
            )
            .unwrap(),
        "R21/E3"
    );
    assert!(matches!(owner, SessionEnvironmentOwner::Vacant), "R21/E3");
}

#[tokio::test]
async fn terminal_takeover_is_highest_priority_and_other_causes_cannot_overwrite() {
    // Cause/effect decision table: C1 an exact owner is already Retiring for
    // Unpublished/RecoveryDiscard/Revocation/Checkpoint; C2 Terminal arrives;
    // C3 a nonterminal or different Terminal cause arrives instead. Effects:
    // E1 C1+C2 preserves identity+binding+Arc and replaces only the cause;
    // E2 C1+C3 is rejected with the original cause unchanged. Rules R19/R21:
    // each lower cause + Terminal => E1; every other different-cause pair => E2.
    let thread = "terminal-takeover";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let generation = generation(thread);
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let lower_causes = [
        SessionEnvironmentRetirementCause::UnpublishedCandidate,
        SessionEnvironmentRetirementCause::RecoveryDiscard,
        SessionEnvironmentRetirementCause::RealizationRevocation,
        SessionEnvironmentRetirementCause::CheckpointSource {
            operation,
            generation,
        },
    ];
    for lower in lower_causes {
        let mut owner = resident(thread, environment.clone());
        let prior = owner
            .begin_retirement(lower, RetirementSelection::Current)
            .unwrap()
            .unwrap();
        let terminal = owner
            .begin_retirement(
                SessionEnvironmentRetirementCause::Terminal {
                    effect_id: "terminal-effect".into(),
                },
                RetirementSelection::Current,
            )
            .unwrap()
            .unwrap();
        assert!(prior.owned.exact_matches(&terminal.owned), "R21/E1");
        assert!(matches!(
            terminal.cause,
            SessionEnvironmentRetirementCause::Terminal { ref effect_id }
                if effect_id == "terminal-effect"
        ));
        let before = terminal.clone();
        assert!(
            owner
                .begin_retirement(
                    SessionEnvironmentRetirementCause::RealizationRevocation,
                    RetirementSelection::Current,
                )
                .is_err(),
            "R19/E2"
        );
        assert!(matches!(
            &owner,
            SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&before)
        ));
    }
}

#[tokio::test]
async fn legacy_revocation_rebuild_consumes_only_exact_typed_observation() {
    // Legacy rebuild cause/effect table: C1 an aggregate-Unowned LegacyDirect
    // owner is Retiring after realization revocation; C2 the caller has no
    // claimed-realization authority; C3 the exact provider observation is Ready
    // or DefinitivelyUnavailable. Effects: E1 C2 retains the exact Retiring
    // owner and performs no I/O; E2 Ready disposes the exact old root and clears
    // only that owner; E3 Unavailable consumes its exact closed evidence and
    // clears only that owner. Rules CP1=C1+C2=>E1, CP2=C1+Ready=>E2,
    // CP3=C1+Unavailable=>E3. The claimed end-to-end creation edge is owned by
    // `claimed_rebuild_disposes_legacy_v1_before_projection_install`; this table
    // deliberately tests only the reusable typed-observation owner. Provisioning,
    // incompatible, indeterminate, and foreign-incarnation rows remain
    // fail-closed under the provider observation/closed-evidence tables.
    for (thread, preclosed) in [
        ("claimed-legacy-revocation-ready", false),
        ("claimed-legacy-revocation-absent", true),
    ] {
        assert_legacy_rebuild_observation(thread, preclosed).await;
    }
}

async fn assert_legacy_rebuild_observation(thread: &str, preclosed: bool) {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, install_complete_projection_for_snapshot, test_activation,
    };

    let root = tempfile::tempdir().unwrap();
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(root.path()));
    // Fixture constraint: the complete projection uses the production Managed
    // runtime only to select the same frozen provider. This test then calls the
    // typed lifecycle owner directly; claimed end-to-end ordering remains in the
    // synchronizer test named above.
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let snapshot = test_activation(thread, "claimed-legacy-publication").snapshot;
    install_complete_projection_for_snapshot(
        &managed,
        thread,
        "workspace",
        eager_environment(),
        &snapshot,
    )
    .await;
    // The retained wrapper and rebuild observation must use the same provider
    // selected by the frozen projection. A second LocalProvider root would be
    // a parallel physical authority and could not observe this exact marker.
    let provider = host
        .projected_session_environment_provider(thread, None)
        .expect("C1 frozen Environment provider");
    let spec = host.sandbox_spec_for_provider(thread, provider);
    // LegacyDirect is the aggregate-Unowned creation row. Reuse the single
    // unfenced test entry into production creation so the handle is the exact
    // marker-free V1 that this compatibility path owns; a fenced V2 fixture
    // would require aggregate disposal authorization that does not exist here.
    let environment = Arc::new(
        host.create_session_environment(provider, &spec)
            .await
            .expect("C1 exact LegacyDirect Environment"),
    );
    let retired_sentinel = root
        .path()
        .join("sandboxes")
        .join(thread)
        .join("retired-owner-sentinel");
    std::fs::write(&retired_sentinel, b"owned by the retiring Environment")
        .expect("C1 retiring root sentinel");
    host.install_test_resident_session_environment(thread, environment.clone());

    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    {
        let _lifecycle = lifecycle.lock().await;
        assert!(
            host.retire_session_environment_for_revocation(thread)
                .await
                .unwrap(),
            "C2 exact revocation"
        );
    }
    let retired = host
        .session_slots
        .read(thread, |slot| slot.environment_owner.clone())
        .unwrap();
    assert!(
        matches!(
            &retired,
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Bound(owned),
            }) if matches!(
                owned.identity,
                BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::Direct(_)
                )
            ) && Arc::ptr_eq(&owned.environment, &environment)
        ),
        "C2 exact legacy/direct owner"
    );

    let unclaimed = match host
        .ctx_for_snapshot(
            thread,
            Some(snapshot.root_agent_id.0.as_str()),
            Some(snapshot.clone()),
        )
        .await
    {
        Ok(_) => panic!("CP1 unclaimed lookup rebuilt the Environment"),
        Err(error) => error,
    };
    assert!(
        unclaimed
            .to_string()
            .contains("Environment transition must be recovered"),
        "CP1/E1"
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(current)) if current.exact_matches(
                match &retired {
                    SessionEnvironmentOwner::Retiring(current) => current,
                    _ => unreachable!(),
                }
            )
        ),
        "CP1/E1 exact Retiring owner retained"
    );
    if preclosed {
        environment
            .dispose()
            .await
            .expect("CP3 exact root already unavailable");
        assert_eq!(
            environment.status().await.unwrap(),
            awaken_provisioning_contract::SandboxStatus::Terminated,
            "CP3 unavailable observation precondition"
        );
    }

    {
        let _lifecycle = lifecycle.lock().await;
        host.rebuild_claimed_legacy_environment_after_revocation(thread, provider)
            .await
            .expect("CP2/CP3 exact typed observation consumes the retirement");
    }
    assert!(
        !retired_sentinel.exists(),
        "CP2/E2 or CP3/E3 exact retired root is absent"
    );
    assert_eq!(
        environment.status().await.unwrap(),
        awaken_provisioning_contract::SandboxStatus::Terminated,
        "CP2/E2 or CP3/E3 old wrapper remains terminal"
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Vacant)
        ),
        "CP2/E2 or CP3/E3 exact Retiring fence is consumed"
    );
}

#[tokio::test]
async fn resume_rebuilds_instead_of_reusing_a_cached_quiesced_runtime() {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, install_complete_projection_for_snapshot,
        managed_test_host, test_activation,
    };

    // Resume recovery cause/effect table: C1 a canonical Managed fixture owns a
    // cached Runtime around one aggregate-Unowned LegacyDirect Environment; C2
    // realization revocation moves that owner to Retiring and clears the cache
    // through the one retirement transition; C3 the resume helper requests a
    // context. Effects: E1 C2 leaves no cached Runtime to reuse; E2 C3 performs
    // the same exact legacy/direct rebuild path and returns a distinct
    // Runtime/Environment/Hand tuple; E3 the retiring root is disposed before
    // same-id recreation. Rules RR1=C1+C2=>E1, RR2=C1+C2+C3=>E2+E3.
    let thread = "cached-resume-revocation";
    let root = tempfile::tempdir().unwrap();
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(root.path()));
    let managed = managed_test_host(host.clone());
    let snapshot = test_activation(thread, "cached-resume-publication").snapshot;
    install_complete_projection_for_snapshot(
        &managed,
        thread,
        "workspace",
        eager_environment(),
        &snapshot,
    )
    .await;
    let provider = host
        .projected_session_environment_provider(thread, None)
        .expect("RR1 canonical projected provider");
    let spec = host.sandbox_spec_for_provider(thread, provider);
    let environment = Arc::new(
        host.create_session_environment(provider, &spec)
            .await
            .expect("RR1 exact LegacyDirect Environment"),
    );
    let retired_sentinel = root
        .path()
        .join("sandboxes")
        .join(thread)
        .join("retired-owner-sentinel");
    std::fs::write(&retired_sentinel, b"owned by the retiring Environment")
        .expect("RR1 retiring root sentinel");
    host.install_test_resident_session_environment(thread, environment.clone());
    let original_hand = environment.tool_executor();
    let agent_id = snapshot.root_agent_id.0.clone();
    let cached = host
        .ctx_for_snapshot(thread, Some(agent_id.as_str()), Some(snapshot))
        .await
        .expect("RR1 cached Runtime");
    assert!(Arc::ptr_eq(cached.env.as_ref().unwrap(), &environment));

    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    {
        let _lifecycle = lifecycle.lock().await;
        assert!(
            host.retire_session_environment_for_revocation(thread)
                .await
                .unwrap(),
            "RR1 exact revocation"
        );
    }
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.is_none())
            .unwrap_or(false),
        "RR1/E1 revocation clears the cached Runtime"
    );

    let resumed = host
        .ctx_for_resume(thread)
        .await
        .expect("RR2 resume recovery");
    let rebuilt = resumed.env.as_ref().expect("E1 rebuilt Environment");
    assert!(!Arc::ptr_eq(&cached, &resumed), "RR2/E2 fresh Runtime");
    assert!(
        !Arc::ptr_eq(rebuilt, &environment),
        "RR2/E2 closed owner replaced"
    );
    assert!(
        !Arc::ptr_eq(&original_hand, &rebuilt.tool_executor()),
        "RR2/E2 rebuilt Environment owns a fresh Hand"
    );
    assert!(
        !retired_sentinel.exists(),
        "RR2/E3 retiring root was removed before same-id recreation"
    );
}

#[tokio::test]
async fn recovery_status_returns_the_canonical_adoption_disposition() {
    use crate::host::worker_resolver::test_support::AdoptionModel;

    // Cause/effect decision table: C1 an exact ordinary Direct Resident is
    // recovered through its explicitly selected provider with rebuild enabled;
    // C2 provider observation is Ready; C3 it is Terminal. Effects: E1 C1+C2
    // returns Ready and reuses the same Environment owner and Runtime Arcs with
    // zero Candidate, binding persistence, or publication; E2 C1+C3 returns RebuildRequired,
    // clears the exact closed owner, records its binding as rebuild input, and
    // never restores the Runtime. Rules R21: Ready=>E1; Terminal=>E2. Provider
    // selection is an input to this canonical API, so the removed historical
    // frozen-provider present/absent rows are no longer separate decisions.
    // Fixture constraint: executable context construction uses the production
    // DispatchSessionRuntime composition; no test-only context path is restored.
    let ready_thread = "ordinary-ready-recovery";
    let ready_storage = tempfile::tempdir().unwrap();
    let ready_host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(ready_storage.path()),
    );
    let _ready_managed =
        crate::ManagedHost::new(ready_host.clone()).install_dispatch_session_runtime();
    let ready_runtime = ready_host
        .ctx_for(ready_thread, None)
        .await
        .expect("R21/C1 ordinary direct Runtime");
    let ready = ready_runtime.env.clone().expect("R21/C2 Ready Environment");
    let ready_binding = serde_json::to_string(&ready.handle()).unwrap();
    let ready_owner = ready_host
        .session_slots
        .read(ready_thread, |slot| slot.environment_owner.clone())
        .expect("R21/C1 exact Resident owner");
    assert!(
        ready_host
            .session_slots
            .read(ready_thread, |slot| slot.published_snapshot.is_none())
            .unwrap_or(false),
        "R21/C5 ordinary direct Runtime has no frozen provider projection"
    );
    let disposition = ready_host
        .adopt_bound_session_environment(
            ready_thread,
            Some(&ready_binding),
            &ready_host.session_provider,
            None,
            true,
        )
        .await
        .expect("R21/E1 Ready direct owner reactivates");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::Ready,
        "R21/E1"
    );
    let resident = ready_host
        .session_environment(ready_thread)
        .await
        .expect("R21/E1 Resident restored");
    assert!(Arc::ptr_eq(&resident, &ready), "R21/E1 exact Arc reused");
    assert!(
        matches!(
            (
                &ready_owner,
                ready_host
                    .session_slots
                    .read(ready_thread, |slot| slot.environment_owner.clone())
                    .as_ref(),
            ),
            (
                SessionEnvironmentOwner::Resident(expected),
                Some(SessionEnvironmentOwner::Resident(current)),
            ) if current.exact_matches(expected)
        ),
        "R21/E1 exact identity is not republished"
    );
    let restored_runtime = ready_host
        .session_slots
        .read(ready_thread, |slot| slot.runtime.clone())
        .flatten()
        .expect("R21/E1 Runtime restored");
    assert!(
        Arc::ptr_eq(&restored_runtime, &ready_runtime),
        "R21/E1 exact Runtime Arc reused"
    );

    let missing_thread = "terminated-recovery-missing-provider";
    let missing_storage = tempfile::tempdir().unwrap();
    let missing_host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(missing_storage.path()),
    );
    let _missing_managed =
        crate::ManagedHost::new(missing_host.clone()).install_dispatch_session_runtime();
    let missing_runtime = missing_host
        .ctx_for(missing_thread, None)
        .await
        .expect("R21/C1 missing-provider Runtime");
    let terminated = missing_runtime
        .env
        .clone()
        .expect("R21/C3 terminated Environment");
    let terminated_binding = serde_json::to_string(&terminated.handle()).unwrap();
    terminated.dispose().await.expect("C3 terminated provider");
    let disposition = missing_host
        .adopt_bound_session_environment(
            missing_thread,
            Some(&terminated_binding),
            &missing_host.session_provider,
            None,
            true,
        )
        .await
        .expect("R21/E2 Terminal owner requests rebuild");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::RebuildRequired,
        "R21/E2"
    );
    assert!(
        missing_host.session_environment_owner_is_vacant(missing_thread),
        "R21/E2 exact closed owner cleared"
    );
    assert!(
        missing_host
            .session_slots
            .read(missing_thread, |slot| slot.runtime.is_none())
            .unwrap_or(false),
        "R21/E2 Terminal owner never restores its Runtime"
    );
}

#[tokio::test]
async fn closed_revoked_durable_environment_can_enter_canonical_rebuild() {
    // Cause/effect table: C1 exact durable Resident was revoked; C2 provider
    // subsequently proves that physical incarnation closed; C3 frozen policy
    // permits rebuild. R1 C1+C2+C3 => consume the exact revoked owner, retain
    // source binding/generation for aggregate Rebuild, never reuse its Hand.
    // Revocation is not a competing RecoveryDiscard intent: changing its cause
    // would be rejected by the retirement priority fence before any rebuild.
    let thread = "closed-revoked-durable-rebuild";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.handle()).unwrap();
    let source_generation = generation(thread).id;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.session_slots.update(thread, |slot| {
        slot.environment_owner = resident(thread, environment.clone());
    });
    host.retire_current_environment(
        thread,
        SessionEnvironmentRetirementCause::RealizationRevocation,
        RetirementSelection::Exact(&environment),
    )
    .expect("C1 exact revocation")
    .expect("C1 retained owner");
    dispose_current_environment(&environment, thread).await;
    // R0 C1+C2+!C3 => reject and retain the exact revoked owner; only the
    // frozen aggregate policy may authorize the subsequent R1 rebuild.
    assert!(
        host.adopt_bound_session_environment(
            thread,
            Some(&binding),
            &provider,
            Some(&source_generation),
            false
        )
        .await
        .is_err()
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(_))
        ),
        "R0 retain revoked owner"
    );
    let disposition = host
        .adopt_bound_session_environment(
            thread,
            Some(&binding),
            &provider,
            Some(&source_generation),
            true,
        )
        .await
        .expect("R1 closed revoked owner must permit canonical rebuild");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::RebuildRequired
    );
    assert!(
        host.session_environment_owner_is_vacant(thread),
        "R1 closed owner consumed"
    );
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.environment_rebuild_source.clone())
            .flatten(),
        Some((binding, Some(source_generation))),
        "R1 exact aggregate rebuild predecessor retained"
    );
}

#[tokio::test]
async fn closed_recovery_cannot_override_protected_retirements() {
    // C1 exact closed physical owner; C2 Terminal or Checkpoint owns retirement;
    // C3 rebuild is enabled. R2 C1+C2+C3 => error, identical protected fence,
    // no rebuild predecessor. Physical absence is not lifecycle authority.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    for terminal in [false, true] {
        let thread = if terminal {
            "closed-terminal"
        } else {
            "closed-checkpoint"
        };
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.handle()).unwrap();
        let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
        host.session_slots.update(thread, |slot| {
            slot.environment_owner = resident(thread, environment.clone());
        });
        let cause = if terminal {
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal".into(),
            }
        } else {
            let generation = generation(thread);
            SessionEnvironmentRetirementCause::CheckpointSource {
                operation: awaken_session_contract::SessionEnvironmentOperation::new(
                    "workspace",
                    thread,
                    "suspend",
                    &generation,
                    1,
                    None,
                    None,
                ),
                generation,
            }
        };
        let protected = host
            .retire_current_environment(thread, cause, RetirementSelection::Exact(&environment))
            .unwrap()
            .unwrap();
        dispose_current_environment(&environment, thread).await;
        assert!(
            host.adopt_bound_session_environment(thread, Some(&binding), &provider, None, true)
                .await
                .is_err(),
            "R2 protected intent"
        );
        assert!(
            matches!(host.session_slots.read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(current)) if current.exact_matches(&protected.owner)),
            "R2 identical fence"
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.environment_rebuild_source.is_none())
                .unwrap(),
            "R2 no rebuild"
        );
    }
}

#[tokio::test]
async fn stale_same_binding_retirement_cannot_clear_an_aba_replacement() {
    // Cause/effect graph: C1 two wrappers carry the same durable identity,
    // handle, and binding; C2 their Arc identities differ; C3 stale owner
    // reports Terminated after replacement entered Retiring. Effects: E1 C3
    // cannot clear replacement; E2 replacement's exact observation clears.
    // Decision rules R17/R19: C1+C2+C3=>E1; exact replacement=>E2.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let first = environment(&provider, "aba-retirement").await;
    let second = Arc::new(
        provider
            .adopt(
                &crate::provisioning::agent_run_sandbox_spec("aba-retirement"),
                &first.handle(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(first.handle(), second.handle(), "R17/C1");
    assert!(!Arc::ptr_eq(&first, &second), "R17/C2");

    // R17 also covers revoked-owner completion: neither same binding nor same
    // generation can substitute for the exact captured wrapper identity.
    for cause in [
        SessionEnvironmentRetirementCause::RecoveryDiscard,
        SessionEnvironmentRetirementCause::RealizationRevocation,
    ] {
        let mut owner = resident("aba-retirement", first.clone());
        let stale = owner
            .begin_retirement(cause.clone(), RetirementSelection::Current)
            .unwrap()
            .unwrap();
        owner = resident("aba-retirement", second.clone());
        let replacement = owner
            .begin_retirement(cause, RetirementSelection::Current)
            .unwrap()
            .unwrap();
        assert!(
            !owner
                .observe_retirement_status::<()>(
                    &stale,
                    Ok(awaken_provisioning_contract::SandboxStatus::Terminated),
                )
                .unwrap(),
            "R17/E1"
        );
        assert!(
            matches!(owner, SessionEnvironmentOwner::Retiring(_)),
            "R17/E1"
        );
        assert!(
            owner
                .observe_retirement_status::<()>(
                    &replacement,
                    Ok(awaken_provisioning_contract::SandboxStatus::Terminated),
                )
                .unwrap(),
            "R17/E2"
        );
    }
}

#[tokio::test]
async fn checkpoint_disposal_cannot_consume_a_different_prepared_durable_owner() {
    // Cause/effect graph: C1 Preparation retains a durable G1 source; C2 the
    // later physical Disposal asserts G2 for the same binding; C3 its typed
    // provider authorization is otherwise closed. Effects: E1 Disposal fails
    // before provider I/O; E2 the exact G1 identity, binding, and Arc remain
    // Retiring under their original Preparation. Decision rule R19/R20:
    // C1+C2+C3 => E1+E2 zero physical effect. Cold source reconstruction is
    // separately covered by the canonical end-to-end source Preparation test
    // `source_release_preparation_is_withheld_for_an_absent_pod_with_live_claim`.
    let thread = "checkpoint-owner-fence";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.handle()).unwrap();
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.session_slots.update(thread, |slot| {
        // Test fixture write remains inside the sole lifecycle-owner module.
        slot.environment_owner = resident(thread, environment.clone());
    });
    let prepared_generation = generation(thread);
    let prepared_operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &prepared_generation,
        7,
        None,
        None,
    );
    host.retain_checkpoint_source_environment_for_disposal(
        thread,
        &prepared_operation,
        &prepared_generation,
        &binding,
        &environment,
    )
    .expect("C1 exact Preparation owner");
    let asserted_generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        2,
        u64::MAX,
        "other-environment",
        "other-image",
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &asserted_generation,
        7,
        None,
        None,
    );
    let authorization = source_disposal_authorization(&prepared_operation.effect_id);

    let error = host
        .dispose_prepared_checkpoint_source_environment(
            thread,
            &operation,
            &asserted_generation,
            &binding,
            &authorization,
        )
        .await
        .expect_err("R19/E1 mismatched identity is denied");
    assert!(error.to_string().contains("retained Preparation owner"));
    let retained = host
        .session_slots
        .read(thread, |slot| match &slot.environment_owner {
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause:
                    SessionEnvironmentRetirementCause::CheckpointSource {
                        operation,
                        generation,
                    },
                owned: RetiringEnvironmentOwner::Bound(owned),
            }) if operation == &prepared_operation && generation == &prepared_generation => {
                Some(owned.clone())
            }
            _ => None,
        })
        .flatten()
        .expect("R20/E2 exact owner remains in its Preparation");
    assert_eq!(retained.identity, identity(thread), "R20/E2");
    assert!(Arc::ptr_eq(&retained.environment, &environment), "R20/E2");
}

#[test]
fn legacy_suspending_projection_keeps_exact_binding_provenance_without_fabrication() {
    // Cause/effect rule: C1 an upgrade reads a legacy Suspending row whose
    // source effect was not serialized; C2 its generation and binding remain
    // exact. Effect E1 project one `LegacyDirect` pending owner containing the
    // exact durable binding and an explicit absent effect, never a fabricated
    // `Durable` generation identity. New suspend rows take the sibling Durable
    // branch because their source effect is non-empty.
    let thread = "legacy-suspending-projection";
    let generation = generation(thread);
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Suspending {
            operation,
            source_effect_id: Box::new(String::new()),
            source_binding: "legacy-binding".into(),
            generation,
            suspend_phase: awaken_session_contract::SuspendPhase::ReadyToDispose,
            checkpoint: None,
            source_release_preparation: None,
        },
    )
    .expect("E1 legacy projection");
    assert!(matches!(
        host.session_slots
            .read(thread, |slot| slot.environment_owner.clone()),
        Some(SessionEnvironmentOwner::Preparing(
            SessionEnvironmentPreparation::AwaitingAdoption {
                identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::DurableBinding {
                        binding,
                        effect: crate::session_slot::LegacyEnvironmentEffect::Absent,
                    }
                ),
                binding: owner_binding,
            }
        )) if binding == "legacy-binding" && owner_binding == binding
    ));
}

#[tokio::test]
async fn durable_adoption_without_an_owning_sink_preserves_exact_projected_identity() {
    use awaken_session_contract::SessionRuntime as _;

    // Cause/effect decision table: C1 a legacy durable Resident projection
    // supplies one exact binding identity; C2 provider adoption returns the
    // matching hidden candidate; C3 the binding sink is absent; C4 a configured
    // sink explicitly does not own this Thread. Effects: E1 C1+C2+(C3|C4)
    // returns the unchanged projected identity; E2 no new Direct receipt is
    // fabricated and a non-owner never receives persist. Rules D1=C3=>E1+E2,
    // D2=C4=>E1+E2. An owning sink remains the separate Store-read authority.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    for (suffix, install_non_owner) in [("absent", false), ("non-owner", true)] {
        let thread = format!("durable-adoption-{suffix}");
        let environment = environment(&provider, &thread).await;
        let binding = serde_json::to_string(&environment.handle()).unwrap();
        let expected = BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::DurableBinding {
                binding: binding.clone(),
                effect: crate::session_slot::LegacyEnvironmentEffect::Absent,
            },
        );
        let host = Arc::new(SharedHost::new(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        ));
        host.install_session_environment_owner_projection(
            &thread,
            "workspace",
            &awaken_session_contract::SessionEnvironmentState::Resident {
                binding,
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        )
        .expect("C1 exact durable projection");
        if install_non_owner {
            crate::ManagedHost::new(host.clone())
                .install_environment_binding_sink(Arc::new(NonOwningEnvironmentBindingSink));
        }
        let candidate = host
            .begin_session_environment_adoption(&thread, environment)
            .expect("C2 hidden adoption candidate");
        assert_eq!(
            candidate.origin,
            UnboundSessionEnvironmentOrigin::DurableAdoption(expected.clone()),
            "C1+C2"
        );
        let actual = host
            .persist_environment_before_publish(&thread, &candidate)
            .await
            .expect("D1/D2 direct adoption fallback");
        assert_eq!(actual, expected, "E1/E2 {suffix}");
    }
}

#[tokio::test]
async fn ordinary_candidate_without_a_sink_gets_an_explicit_direct_receipt() {
    // Cause/effect decision table: C1 a direct Thread has no binding sink; C2
    // its hidden candidate originated from Create; C3 it originated from plain
    // Adoption without durable projected identity. Effects: E1 C1+C2 emits one
    // LegacyDirect Create receipt; E2 C1+C3 emits one LegacyDirect Adopt
    // receipt; both retain the exact Session and physical binding and claim no
    // durable generation. Rules O1=C1+C2=>E1, O2=C1+C3=>E2.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    for (thread, kind) in [
        (
            "ordinary-create-fallback",
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
        ),
        (
            "ordinary-adopt-fallback",
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
        ),
    ] {
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.handle()).unwrap();
        let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
        let candidate = match kind {
            awaken_session_contract::SessionEnvironmentEffectKind::Create => host
                .begin_session_environment_preparation(thread, environment)
                .expect("O1 create candidate"),
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt => host
                .begin_session_environment_adoption(thread, environment)
                .expect("O2 adoption candidate"),
            awaken_session_contract::SessionEnvironmentEffectKind::Rebuild { .. }
            | awaken_session_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                ..
            } => unreachable!("O1/O2 enumerate only direct Create and Adopt effects"),
        };
        let actual = host
            .persist_environment_before_publish(thread, &candidate)
            .await
            .expect("O1/O2 direct fallback");
        let BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::Direct(receipt),
        ) = actual
        else {
            panic!("ordinary candidate acquired a fabricated durable identity")
        };
        assert_eq!(receipt.session_id, thread, "E1/E2 Session");
        assert_eq!(receipt.kind, kind, "E1/E2 effect kind");
        assert_eq!(receipt.binding, binding, "E1/E2 binding");
        assert!(receipt.realization.is_none(), "E1/E2 direct authority");
    }
}

#[tokio::test]
async fn durable_projection_cannot_publish_a_hidden_candidate() {
    // Cause/effect rule: C1 provider return is held as one hidden Candidate;
    // C2 a matching durable Resident projection is installed before the binding
    // sink returns; C3 the sink later supplies its exact Store-read identity.
    // Effects: E1 C1+C2 remains the same hidden Candidate with zero tool exposure;
    // E2 only C3 publishes the exact Arc as Resident. This fences projection
    // replay from becoming a second publication authority.
    let thread = "candidate-projection-fence";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let candidate = host
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("C1 Candidate");
    let generation = generation(thread);
    let effect_id = "store-read-effect".to_string();
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: candidate.binding.clone(),
            effect_id: Some(effect_id.clone()),
            generation: Some(generation.clone()),
            idle_since_unix_ms: None,
        },
    )
    .expect("C2 matching projection");
    let retained = host
        .prepared_session_environment(thread)
        .expect("E1 Candidate remains hidden");
    assert!(candidate.exact_matches(&retained), "E1");
    assert!(host.session_environment(thread).await.is_none(), "E1");

    let published = host
        .publish_prepared_session_environment(
            thread,
            &candidate,
            BoundSessionEnvironmentIdentity::Durable {
                effect_id,
                generation,
            },
        )
        .expect("C3 Store-read publication");
    assert!(Arc::ptr_eq(&published, &environment), "E2");
}

#[tokio::test]
async fn committed_hidden_create_candidate_is_terminally_retired_without_publication() {
    // Cause/effect decision table for the Dream cancel/create race: C1 one
    // Create Candidate owns the exact provider Arc; C2 the Session root commit
    // returns a durable identity; C3 the derivative dispatch binding is fenced
    // before Resident publication; C4 terminal cleanup takes over. Effects:
    // E1 C1+C2+C3 retains one hidden DurableCreate with the original Create
    // retry kind and exact binding; E2 no tool-visible Resident appears; E3
    // C4 converts that same Arc and durable identity into the sole Bound
    // terminal-retirement owner. Rule D1=C1+C2+C3=>E1+E2;
    // D2=D1+C4=>E3. A mismatched replay is rejected by the Kani-proved closed
    // Candidate commit reducer and cannot relabel Create as Adopt.
    let thread = "committed-candidate-terminal-takeover";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let candidate = host
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("C1 hidden Create Candidate");
    let identity = identity(thread);
    let committed = host
        .retain_committed_session_environment_candidate(thread, &candidate, identity.clone())
        .expect("C2/C3 retain Store-read identity");
    assert!(matches!(
        &committed.origin,
        UnboundSessionEnvironmentOrigin::DurableCreate(current) if current == &identity
    ));
    assert_eq!(
        committed.effect_kind(),
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        "E1"
    );
    assert_eq!(
        host.durable_session_environment_binding(thread).as_deref(),
        Some(candidate.binding.as_str()),
        "E1"
    );
    assert!(host.session_environment(thread).await.is_none(), "E2");

    let cause = SessionEnvironmentRetirementCause::Terminal {
        effect_id: "terminal-candidate-takeover".into(),
    };
    let retirement = host
        .session_slots
        .update(thread, |slot| {
            slot.environment_owner
                .begin_retirement_with_committed_candidate(&committed, cause)
        })
        .expect("C4 terminal takeover");
    let RetiringEnvironmentOwner::Bound(owned) = retirement.owned else {
        panic!("E3 committed Candidate must become one Bound terminal owner")
    };
    assert_eq!(owned.identity, identity, "E3 identity");
    assert_eq!(owned.binding, candidate.binding, "E3 binding");
    assert!(Arc::ptr_eq(&owned.environment, &environment), "E3 Arc");
    assert!(
        host.session_environment(thread).await.is_none(),
        "E3 hidden"
    );
}

#[tokio::test]
async fn candidate_revocation_and_projection_preserve_exact_commit_evidence() {
    use awaken_session_contract::SessionEnvironmentEffectKind::{Adopt, Create};
    // Causes: C1 Create/Adopt origin; C2 Store-read identity retained or absent;
    // C3 selected Arc exact/foreign; C4 revocation/unpublished cleanup;
    // C5 projected identity exact/same-generation receipt/new generation;
    // C6 projected binding exact/foreign. Effects and decision rules:
    // CR1 !C3 => no transition, original Candidate unchanged.
    // CR2 C3+C2+revocation => same Arc/binding/identity becomes Bound Retiring.
    // CR3 C3+(!C2 or cleanup) => Unbound Retiring, no invented durable identity.
    // CP1 !C6 or (C2+new generation) => reject, same Candidate retained.
    // CP2 all remaining projections => accept but NEVER publish a Candidate.
    // Fully cross C1-C4 for retirement and C1+C2 with each independent identity
    // conflict for projection. Same-generation receipt stutter is intentional;
    // without Store readback a projection alone cannot establish commitment.
    // RP1 exact retired binding + revocation => confirm projection without
    // changing Bound/Unbound phase; RP2 wrong binding/cleanup cause => reject.
    // RA1 exact revocation + same physical rebound target => one new hidden
    // Adopt Candidate, preserving only known identity; RA2 foreign target,
    // stale retirement, wrong cause or replay => reject without owner change.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let owned = environment(&provider, "candidate-evidence").await;
    let foreign = environment(&provider, "candidate-foreign").await;
    let rebound = Arc::new(
        provider
            .rebind_retained(
                &crate::provisioning::agent_run_sandbox_spec("candidate-evidence"),
                &owned.handle(),
                &owned,
                None,
                None,
            )
            .await
            .unwrap(),
    );
    for kind in [Create, Adopt] {
        for committed in [false, true] {
            let mut base = SessionEnvironmentOwner::Vacant;
            let mut candidate = base.begin_preparing(owned.clone(), kind.clone()).unwrap();
            let identity = identity("candidate-evidence");
            if committed {
                candidate = base
                    .retain_committed_candidate(&candidate, identity.clone())
                    .unwrap();
            }
            for exact in [false, true] {
                for cause in [
                    SessionEnvironmentRetirementCause::RealizationRevocation,
                    SessionEnvironmentRetirementCause::UnpublishedCandidate,
                ] {
                    let mut owner = base.clone();
                    let retirement = owner
                        .begin_retirement(
                            cause.clone(),
                            RetirementSelection::Exact(if exact { &owned } else { &foreign }),
                        )
                        .unwrap();
                    if !exact {
                        assert!(retirement.is_none(), "CR1");
                        assert!(
                            owner
                                .unpublished_candidate()
                                .unwrap()
                                .exact_matches(&candidate)
                        );
                        continue;
                    }
                    let retirement = retirement.unwrap();
                    assert!(Arc::ptr_eq(&retirement.owned.environment(), &owned));
                    match &retirement.owned {
                        RetiringEnvironmentOwner::Bound(bound) => {
                            assert!(
                                committed
                                    && cause
                                        == SessionEnvironmentRetirementCause::RealizationRevocation,
                                "CR2"
                            );
                            assert_eq!(bound.identity, identity);
                            assert_eq!(bound.binding, candidate.binding);
                        }
                        RetiringEnvironmentOwner::Unbound(unbound) => {
                            assert!(
                                !committed
                                    || cause
                                        == SessionEnvironmentRetirementCause::UnpublishedCandidate,
                                "CR3"
                            );
                            assert!(unbound.exact_matches(&candidate));
                        }
                    }
                    for exact_binding in [false, true] {
                        let mut projected_owner = owner.clone();
                        let result = projected_owner.install_projection(
                            ProjectedEnvironmentOwner::AwaitingAdoption {
                                identity: identity.clone(),
                                binding: if exact_binding {
                                    candidate.binding.clone()
                                } else {
                                    "foreign".into()
                                },
                            },
                            true,
                        );
                        assert_eq!(
                            result.is_ok(),
                            exact_binding
                                && cause
                                    == SessionEnvironmentRetirementCause::RealizationRevocation,
                            "RP1/RP2"
                        );
                        assert!(
                            matches!(&projected_owner, SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement)),
                            "RP no promotion"
                        );
                    }
                    assert!(
                        owner
                            .prepare_retiring_realization_adoption(&retirement, foreign.clone())
                            .is_err(),
                        "RA2 wrong target"
                    );
                    assert!(
                        matches!(&owner, SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement))
                    );
                    let mut stale = retirement.clone();
                    stale.cause = SessionEnvironmentRetirementCause::RecoveryDiscard;
                    assert!(
                        owner
                            .prepare_retiring_realization_adoption(&stale, rebound.clone())
                            .is_err(),
                        "RA2 stale owner"
                    );
                    let result =
                        owner.prepare_retiring_realization_adoption(&retirement, rebound.clone());
                    if cause == SessionEnvironmentRetirementCause::RealizationRevocation {
                        result.expect("RA1");
                        let fresh = owner.unpublished_candidate().unwrap();
                        assert!(Arc::ptr_eq(&fresh.environment, &rebound));
                        assert_eq!(fresh.binding, candidate.binding);
                        assert_eq!(fresh.effect_kind(), Adopt);
                        assert_eq!(fresh.durable_identity(), committed.then_some(&identity));
                        let fresh = fresh.clone();
                        assert!(
                            owner
                                .prepare_retiring_realization_adoption(&retirement, rebound.clone())
                                .is_err(),
                            "RA2 replay"
                        );
                        assert!(owner.unpublished_candidate().unwrap().exact_matches(&fresh));
                    } else {
                        assert!(result.is_err(), "RA2 wrong cause");
                        assert!(
                            matches!(&owner, SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement))
                        );
                    }
                }
            }
            for rule in ["exact", "receipt", "generation", "binding"] {
                let mut owner = base.clone();
                let mut projected = identity.clone();
                let mut binding = candidate.binding.clone();
                if let BoundSessionEnvironmentIdentity::Durable {
                    effect_id,
                    generation,
                } = &mut projected
                {
                    match rule {
                        "receipt" => effect_id.push_str("-successor"),
                        "generation" => generation.id.push_str("-foreign"),
                        "binding" => binding.push_str("-foreign"),
                        _ => {}
                    }
                }
                let result = owner.install_projection(
                    ProjectedEnvironmentOwner::AwaitingAdoption {
                        identity: projected,
                        binding,
                    },
                    true,
                );
                assert_eq!(
                    result.is_err(),
                    rule == "binding" || (committed && rule == "generation"),
                    "CP1/CP2 {kind:?} {committed} {rule}"
                );
                assert!(
                    owner
                        .unpublished_candidate()
                        .unwrap()
                        .exact_matches(&candidate),
                    "CP1/CP2 stays hidden"
                );
            }
        }
    }
}

#[tokio::test]
async fn unacknowledged_candidate_adoption_requires_aggregate_authority() {
    // UA1 exact retained V2 Candidate + no installed aggregate lease/sink =>
    // reject before Provider observation/adoption and keep exact Retiring owner.
    // A raw local handle is provenance for recovery, never authority by itself.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let thread = "unacknowledged-no-authority";
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let candidate = host
        .begin_session_environment_preparation(thread, environment.clone())
        .unwrap();
    let retirement = host
        .retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RealizationRevocation,
            RetirementSelection::Exact(&environment),
        )
        .unwrap()
        .unwrap();
    let result = host
        .adopt_bound_session_environment(thread, Some(&candidate.binding), &provider, None, false)
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("UA1 unowned adoption admitted"),
    };
    assert!(
        error
            .to_string()
            .contains("requires current aggregate adoption authority"),
        "UA1: {error}"
    );
    host.session_slots
        .read(thread, |slot| {
            assert!(matches!(&slot.environment_owner,
        SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement.owner)))
        })
        .unwrap();
    assert!(host.session_environment(thread).await.is_none());
}

#[tokio::test]
async fn stale_unmaterialized_projection_cannot_unbind_a_candidate_or_resident() {
    // Projection-order cause/effect decision table: C1 an Unmaterialized
    // FrozenSessionProjection is read before the lifecycle guard; C2 the exact
    // create Candidate is installed while C1 waits; C3 the binding sink commits
    // and the same Candidate becomes Resident before C1 installs; C4 durable
    // retirement has already moved the owner to Retiring/Vacant. Effects: E1
    // C1+C2 retains the exact hidden Candidate and returns success; E2 C1+C3
    // retains the exact Resident Arc/identity and returns success; E3 C1+C4 may
    // confirm Vacant but never disposes a live owner. Rules S1=C1+C2=>E1,
    // S2=C1+C3=>E2; the existing retirement tests own S3=C1+C4=>E3. This makes
    // stale projection replay a no-write operation instead of an unrecoverable
    // rescheduling loop.
    let thread = "stale-unmaterialized-projection";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let candidate = host
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("S1 Candidate");

    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    )
    .expect("S1 stale projection is a no-write replay");
    let retained = host
        .prepared_session_environment(thread)
        .expect("S1 Candidate retained");
    assert!(candidate.exact_matches(&retained), "S1 exact Candidate");
    assert!(
        host.session_environment(thread).await.is_none(),
        "S1 hidden"
    );

    let published = host
        .publish_prepared_session_environment(thread, &candidate, identity(thread))
        .expect("S2 publish exact Candidate");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    )
    .expect("S2 stale projection is a no-write replay");
    let resident = host
        .session_environment(thread)
        .await
        .expect("S2 Resident retained");
    assert!(Arc::ptr_eq(&published, &resident), "S2 exact Resident Arc");
    assert!(Arc::ptr_eq(&environment, &resident), "S2 provider Arc");
}

#[tokio::test]
async fn durable_activity_generation_is_the_only_background_quiescence_key() {
    // Cause/effect table for the background-task/C2-b proof-sync overlap: C1 an
    // exact durable Resident owns one Arc+handle; C2 its physical sandbox id is
    // different from the aggregate Sandbox generation id; C3 SharedEnvironment
    // work is active; C4 that work releases. Effects: E1 the owner projects only
    // the durable id; E2 querying the physical id observes no false ownership;
    // E3 durable-id quiescence cannot prove before C4; E4 it proves after C4.
    // LegacyDirect retains the physical-id branch in the same owner method, and
    // an Environment-free context retains the existing `brain` key.
    let thread = "durable-background-generation";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let physical_id = environment.handle().sandbox_id;
    let durable_generation = generation(thread);
    assert_ne!(physical_id, durable_generation.id, "C2");
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.session_slots.update(thread, |slot| {
        slot.environment_owner = SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
            identity: BoundSessionEnvironmentIdentity::Durable {
                effect_id: format!("effect-{thread}"),
                generation: durable_generation.clone(),
            },
            binding: serde_json::to_string(&environment.handle()).unwrap(),
            environment: environment.clone(),
        });
    });
    let activity_generation = host
        .resident_environment_activity_generation_id(thread, &environment)
        .expect("C1 exact Resident activity projection");
    assert_eq!(activity_generation, durable_generation.id, "E1");

    let release = Arc::new(tokio::sync::Notify::new());
    host.memory
        .background()
        .spawn(
            crate::background::BackgroundWorkClass::SharedEnvironment {
                session_id: thread.into(),
                generation_id: activity_generation.clone(),
            },
            {
                let release = release.clone();
                async move { release.notified().await }
            },
        )
        .await;
    assert!(
        host.memory
            .background()
            .quiesce_shared_environment(thread, &physical_id, std::time::Duration::from_millis(1),)
            .await,
        "E2 physical id is not the durable activity owner"
    );
    assert!(
        !host
            .memory
            .background()
            .quiesce_shared_environment(
                thread,
                &activity_generation,
                std::time::Duration::from_millis(1),
            )
            .await,
        "E3 no quiescence proof while exact durable activity remains"
    );
    release.notify_one();
    assert!(
        host.memory
            .background()
            .quiesce_shared_environment(
                thread,
                &activity_generation,
                std::time::Duration::from_secs(1),
            )
            .await,
        "E4 exact durable activity release admits quiescence"
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_background_fence_preserves_resident_and_retiring_outputs_for_retry() {
    use crate::host::worker_resolver::test_support::{
        eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    use awaken_provisioning_contract::SandboxStatus;

    // Cause/effect table: C1 the sole owner is either Resident or a retryable
    // Retiring::Bound; C2 identity is Durable or LegacyDirect and therefore
    // selects the durable generation or physical sandbox key respectively; C3
    // exact SharedEnvironment work remains active beyond the bounded wait; C4
    // that work releases and the identical aggregate-authorized Preparation
    // retries; C5 an Agent-authored Skill remains run-scoped without an explicit
    // PromotionGate; C6 the aggregate durably records that receipt and projects the
    // separate physical Disposal. Effects: E1 C3 returns classified Unavailable
    // with zero Skill/Artifact harvest and zero physical disposal while retaining
    // the exact Arc under the terminal retirement fence; E2 the other identity
    // key cannot substitute; E3 C4 returns one Artifact Preparation receipt,
    // leaves C5 unpublished, and leaves the physical source Ready; E4 C6 performs
    // one disposal and only the subsequent
    // durable acknowledgement removes the owner. Rules T1-T4 cross
    // Resident/Retiring with Durable/Legacy under C3=>E1+E2, C4=>E3, C5=>E4.
    for (thread, retiring, durable) in [
        ("terminal-background-resident-durable", false, true),
        ("terminal-background-retiring-durable", true, true),
        ("terminal-background-resident-legacy", false, false),
        ("terminal-background-retiring-legacy", true, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let host = Arc::new(
            SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
                .with_store_dir(root.path())
                .with_skill_store(root.path().join("skill-store")),
        );
        let activation = test_activation(thread, &format!("terminal-background-{thread}"));
        let mut projection = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &activation.snapshot,
        );
        host.install_dispatch_frozen_session_projection(thread, projection.clone())
            .await
            .expect("C1 install complete physical projection before creation");
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: format!("terminal-background-worker-{thread}"),
            runtime_incarnation: format!("terminal-background-worker-{thread}:incarnation"),
            epoch: 1,
            expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                + 60_000,
        };
        let create_fence = lease
            .sandbox_effect_fence(format!("terminal-background-create-{thread}"))
            .expect("C1 create exact durable Sandbox fence");
        let spec = host.sandbox_spec(thread);
        let physical = host
            .provider
            .create_sandbox_for_effect(
                &spec,
                &create_fence,
                None,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&create_fence),
            )
            .await
            .expect("C1 create provider-effective terminal Sandbox");
        let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            physical,
        ));
        let physical_id = environment.handle().sandbox_id;
        let durable_generation = generation(thread);
        assert_ne!(physical_id, durable_generation.id, "C2");
        let binding = serde_json::to_string(&environment.handle()).unwrap();
        let identity = if durable {
            BoundSessionEnvironmentIdentity::Durable {
                effect_id: create_fence.operation_id.clone(),
                generation: durable_generation.clone(),
            }
        } else {
            BoundSessionEnvironmentIdentity::LegacyDirect(
                LegacyDirectEnvironmentProvenance::DurableBinding {
                    binding: binding.clone(),
                    effect: crate::session_slot::LegacyEnvironmentEffect::Absent,
                },
            )
        };
        let (activity_generation_id, other_generation_id) = if durable {
            (durable_generation.id.clone(), physical_id.clone())
        } else {
            (physical_id.clone(), durable_generation.id.clone())
        };
        let owned = BoundSessionEnvironment {
            identity,
            binding: binding.clone(),
            environment: environment.clone(),
        };
        // A changed aggregate Environment is a new root-CAS snapshot.
        projection.revision.0 += 1;
        projection.environment = awaken_session_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: durable.then(|| create_fence.operation_id.clone()),
            generation: durable.then(|| durable_generation.clone()),
            idle_since_unix_ms: None,
        };
        host.session_slots.update(thread, |slot| {
            slot.environment_owner = SessionEnvironmentOwner::Resident(owned.clone());
        });

        let output = root
            .path()
            .join("sandboxes")
            .join(thread)
            .join(spec.outputs_path.trim_start_matches('/'))
            .join("report.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, format!("report-{thread}")).unwrap();
        let skill_dir = root
            .path()
            .join("sandboxes")
            .join(thread)
            .join("skills")
            .join("notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: authored before terminal retry\n---\nretain me",
        )
        .unwrap();
        assert!(
            environment
                .scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR)
                .unwrap()
                .iter()
                .any(|skill| skill.id == "notes"),
            "C5 physical authored Skill exists before cleanup"
        );
        let release = Arc::new(tokio::sync::Notify::new());
        host.memory
            .background()
            .spawn(
                crate::background::BackgroundWorkClass::SharedEnvironment {
                    session_id: thread.into(),
                    generation_id: activity_generation_id.clone(),
                },
                {
                    let release = release.clone();
                    async move { release.notified().await }
                },
            )
            .await;
        if retiring {
            let realization = host.session_slots.realization_lock(thread);
            let _realization = realization.lock().await;
            let lifecycle = host
                .session_slots
                .read(thread, |slot| slot.lifecycle.clone())
                .unwrap();
            let _lifecycle = lifecycle.lock().await;
            assert!(
                host.retire_session_environment_for_revocation(thread)
                    .await
                    .unwrap(),
                "C1 public revocation transition"
            );
            assert_eq!(
                host.registered_thread_workspace(thread).as_deref(),
                Some("workspace"),
                "C1 Retiring owner keeps exact terminal harvest scope"
            );
        }
        // Real takeover ordering: revocation first clears the old realization
        // generation while retaining the exact physical owner; only then does
        // Control's terminal assignment install its new cleanup lease.
        host.install_test_terminal_cleanup_projection(
            &awaken_session_contract::SessionTerminalCleanupAssignment {
                session_id: thread.into(),
                projection: projection.clone(),
                authority: crate::session_slot::test_lease_receipt(lease.clone()),
            },
        )
        .await
        .expect("C1 install exact terminal projection after any revocation");

        let mut aggregate = awaken_session_contract::PersistedSession::frozen_with_budget(
            thread,
            projection.baseline.clone(),
            awaken_session_contract::SessionResourceState::from_active(
                projection.resources.clone(),
            ),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        aggregate.environment = projection.environment.clone();
        aggregate.realization = Some(lease.clone());
        assert!(aggregate.ensure_terminal_cleanup_fence());
        aggregate
            .freeze_terminal_cleanup_targets(std::iter::empty(), 0, 0)
            .expect("C3 freeze exact root terminal target");
        let command = aggregate
            .terminal_cleanup
            .command_for(thread, thread)
            .expect("C3 aggregate terminal command");
        let effect =
            awaken_session_contract::SessionTerminalCleanupEffect::new(command, lease.clone());
        let inherited_provider_disposal = aggregate
            .authorize_terminal_cleanup_effect(&effect)
            .expect("C3 aggregate preparation authorization");
        let authorization =
            awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                projection.workspace_id.clone(),
                inherited_provider_disposal,
            )
            .expect("C3 closed terminal preparation authorization");
        let blocked = tokio::spawn({
            let host = host.clone();
            let effect = effect.clone();
            let authorization = authorization.clone();
            async move {
                host.prepare_terminal_cleanup_effect(effect, authorization)
                    .await
            }
        });
        // A paused Tokio runtime may auto-advance once the quiescence timeout
        // is the only runnable future, so the classified timeout itself is the
        // stable evidence; observing an intermediate scheduler state is not.
        let error = blocked.await.unwrap().unwrap_err();
        assert_eq!(error.kind, HostErrorKind::Unavailable, "E1: {error:?}");
        assert_eq!(
            error.code, "session_terminal_background_not_quiescent",
            "E1"
        );
        assert!(
            host.file_application()
                .unwrap()
                .list("workspace", Some(thread))
                .await
                .unwrap()
                .is_empty(),
            "E1 zero harvest"
        );
        assert!(
            host.skills
                .definitions("workspace")
                .await
                .unwrap()
                .is_empty(),
            "E1 zero Skill harvest"
        );
        assert_eq!(
            environment.status().await.unwrap(),
            SandboxStatus::Ready,
            "E1"
        );
        let retained = host
            .session_slots
            .read(thread, |slot| slot.environment_owner.clone())
            .unwrap();
        assert!(
            matches!(
                retained,
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    cause: SessionEnvironmentRetirementCause::Terminal { ref effect_id },
                    owned: RetiringEnvironmentOwner::Bound(ref current),
                }) if effect_id == effect.operation_id() && current.exact_matches(&owned)
            ),
            "E1 exact Arc retained under the canonical terminal fence"
        );
        assert!(
            host.memory
                .background()
                .has_shared_environment_work(thread, &activity_generation_id),
            "E2 exact owner-selected activity remains visible"
        );
        assert!(
            !host
                .memory
                .background()
                .has_shared_environment_work(thread, &other_generation_id),
            "E2 the other identity domain is not a substitute activity key"
        );

        release.notify_one();
        let preparation = host
            .prepare_terminal_cleanup_effect(effect.clone(), authorization)
            .await
            .expect("C4 identical Preparation retry");
        assert_eq!(
            preparation.artifact_receipts.len(),
            1,
            "E3 one Preparation harvest receipt"
        );
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace", Some(thread))
                .await
                .unwrap()
                .len(),
            1,
            "E3 one durable Artifact"
        );
        assert!(
            host.skills
                .definitions("workspace")
                .await
                .unwrap()
                .is_empty(),
            "E3 cleanup cannot self-promote an Agent-authored Skill; explicit publication remains the canonical owner"
        );
        assert_eq!(
            environment.status().await.unwrap(),
            SandboxStatus::Ready,
            "E3 Preparation is non-destructive"
        );

        let repository_preparation =
            awaken_session_contract::SessionCleanupRepositoryPreparation::new(
                thread,
                &projection.workspace_id,
                &aggregate.resources,
            )
            .expect("C5 prepare aggregate Repository retirement");
        aggregate
            .record_terminal_cleanup_preparation(
                &projection.workspace_id,
                &lease,
                preparation,
                Some(repository_preparation),
            )
            .expect("C5 durably admit exact Preparation");
        host.acknowledge_terminal_cleanup_preparation(&effect).await;
        let disposal_command = match aggregate
            .terminal_cleanup_work_action()
            .expect("C5 project canonical terminal action")
            .expect("C5 complete Preparation projects Disposal")
        {
            awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => command,
            action => panic!("C5 expected Disposal, got {action:?}"),
        };
        let disposal_effect = awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
            disposal_command,
            lease.clone(),
        );
        let disposal_receipt = host
            .dispose_terminal_cleanup_effect(disposal_effect.clone())
            .await
            .expect("C5 aggregate-authorized physical Disposal");
        assert_eq!(
            environment.status().await.unwrap(),
            SandboxStatus::Terminated,
            "E4 one physical disposal"
        );
        assert!(
            host.session_slots.contains(thread),
            "E4 waits for durable ack"
        );
        aggregate
            .record_terminal_cleanup_disposal(
                &projection.workspace_id,
                &lease,
                disposal_receipt,
                "terminal background test cleanup",
            )
            .expect("C5 durably admit exact Disposal receipt");
        host.acknowledge_terminal_cleanup_disposal(&disposal_effect)
            .await;
        assert!(
            !host.session_slots.contains(thread),
            "E4 owner removed after ack"
        );
    }
}

#[tokio::test]
async fn terminal_unmaterialized_reconstruction_replays_until_durable_disposal_ack() {
    use crate::host::worker_resolver::test_support::{
        eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    // Causes: C1 Unmaterialized aggregate with a provider-proved Ready root but
    // no published binding; C2 Preparation response loss; C3 durable Preparation
    // admission; C4 Disposal response loss; C5 durable Disposal acknowledgement.
    // Effects: U1 C1 reconstructs one hidden Unbound terminal owner, without
    // Artifact harvest or physical deletion; U2 C2 reuses the exact Arc/handle;
    // U3 C3 authorizes deletion without inventing a durable Environment binding;
    // U4 C4 retries with that same retained owner; U5 only C5 removes the slot.
    // The real Local provider owns marker identity and byte deletion. The real
    // aggregate owns both receipt gates; an empty dispatch queue is no proof.
    let thread = "terminal-unmaterialized-replay";
    let root = tempfile::tempdir().unwrap();
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
        .with_store_dir(root.path());
    let activation = test_activation(thread, "terminal-unmaterialized");
    let projection = empty_frozen_projection_for_snapshot(
        "workspace",
        eager_environment(),
        &activation.snapshot,
    );
    assert!(matches!(
        projection.environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "terminal-worker".into(),
        runtime_incarnation: "terminal-worker:boot".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    host.install_test_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
    )
    .await
    .unwrap();
    let intent = awaken_session_contract::SessionEnvironmentEffectIntent::new(
        thread,
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        Some(lease.clone()),
    )
    .for_environment(projection.baseline.environment.config_fingerprint.0.clone());
    let fence = lease.sandbox_effect_fence(intent.effect_id()).unwrap();
    let spec = host.sandbox_spec(thread);
    let physical = host
        .provider
        .create_sandbox_for_effect(
            &spec,
            &fence,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
        )
        .await
        .unwrap();
    // The creation response is not published to the Host or Session root.
    drop(physical);
    let output = root
        .path()
        .join("sandboxes")
        .join(thread)
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("unpublished.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"unpublished source must not become an Artifact").unwrap();
    let mut aggregate = awaken_session_contract::PersistedSession::frozen_with_budget(
        thread,
        projection.baseline.clone(),
        awaken_session_contract::SessionResourceState::from_active(projection.resources.clone()),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    aggregate.realization = Some(lease.clone());
    assert!(aggregate.ensure_terminal_cleanup_fence());
    aggregate
        .freeze_terminal_cleanup_targets(std::iter::empty(), 0, 0)
        .unwrap();
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        aggregate
            .terminal_cleanup
            .command_for(thread, thread)
            .unwrap(),
        lease.clone(),
    );
    let authorization =
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            effect.clone(),
            projection.workspace_id.clone(),
            aggregate
                .authorize_terminal_cleanup_effect(&effect)
                .unwrap(),
        )
        .unwrap();
    let first = host
        .prepare_terminal_cleanup_effect(effect.clone(), authorization.clone())
        .await
        .expect("U1 provider-proved reconstruction");
    assert!(
        first.artifact_receipts.is_empty(),
        "U1 no unpublished output harvest"
    );
    assert!(output.exists(), "U1 Preparation cannot delete");
    let retained = host
        .session_slots
        .read(thread, |slot| slot.environment_owner.clone())
        .unwrap();
    let SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
        owned: RetiringEnvironmentOwner::Unbound(ref candidate),
        ..
    }) = retained
    else {
        panic!("U1 exactly one Unbound terminal owner")
    };
    assert!(
        candidate.durable_identity().is_none(),
        "U1 no fabricated receipt"
    );
    assert!(
        host.session_environment(thread).await.is_none(),
        "U1 hidden"
    );
    // U6 crosses foreign terminal operation and stale lease with both phases.
    // Each must preserve the physical bytes; the exact valid retry still works.
    let mut rejected_owners = vec![SessionEnvironmentOwner::Retiring(
        RetiringSessionEnvironment {
            owned: RetiringEnvironmentOwner::Unbound(candidate.clone()),
            cause: SessionEnvironmentRetirementCause::Terminal {
                effect_id: "foreign-preparation".into(),
            },
        },
    )];
    for wrong in &rejected_owners {
        host.session_slots
            .update(thread, |slot| slot.environment_owner = wrong.clone());
        host.prepare_terminal_cleanup_effect(effect.clone(), authorization.clone())
            .await
            .expect_err("U6 wrong ownership cannot prepare");
        assert!(
            output.exists(),
            "U6 rejected Preparation keeps physical bytes"
        );
    }
    host.session_slots
        .update(thread, |slot| slot.environment_owner = retained.clone());
    let mut stale = effect.clone();
    stale.lease.epoch += 1;
    let stale_authorization =
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            stale.clone(),
            projection.workspace_id.clone(),
            None,
        )
        .unwrap();
    host.prepare_terminal_cleanup_effect(stale, stale_authorization)
        .await
        .expect_err("U6 foreign realization cannot prepare");
    assert!(output.exists(), "U6 stale Preparation keeps physical bytes");
    let preparation = host
        .prepare_terminal_cleanup_effect(effect.clone(), authorization)
        .await
        .expect("U2 identical Preparation after response loss");
    assert_eq!(first, preparation, "U2 stable receipt");
    let same_owner = || {
        host.session_slots
            .read(thread, |slot| match &slot.environment_owner {
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    owned: RetiringEnvironmentOwner::Unbound(current),
                    ..
                }) => {
                    Arc::ptr_eq(&current.environment, &candidate.environment)
                        && current.binding == candidate.binding
                        && current.durable_identity().is_none()
                }
                _ => false,
            })
            .unwrap_or(false)
    };
    assert!(same_owner(), "U2 exact original owner");
    aggregate
        .record_terminal_cleanup_preparation(
            &projection.workspace_id,
            &lease,
            preparation,
            Some(
                awaken_session_contract::SessionCleanupRepositoryPreparation::new(
                    thread,
                    &projection.workspace_id,
                    &aggregate.resources,
                )
                .unwrap(),
            ),
        )
        .unwrap();
    host.acknowledge_terminal_cleanup_preparation(&effect).await;
    let awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } =
        aggregate.terminal_cleanup_work_action().unwrap().unwrap()
    else {
        panic!("U3 aggregate must authorize Disposal")
    };
    let disposal =
        awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(command, lease.clone());
    // A Disposal can reconstruct under its own exact operation only when the
    // provider has supplied it. A retained unrelated terminal owner is not that
    // predecessor; neither a valid receipt nor its fingerprint relabels it.
    for wrong in rejected_owners.drain(..) {
        host.session_slots
            .update(thread, |slot| slot.environment_owner = wrong);
        host.dispose_terminal_cleanup_effect(disposal.clone())
            .await
            .expect_err("U6 wrong ownership cannot dispose");
        assert!(output.exists(), "U6 rejected Disposal keeps physical bytes");
    }
    host.session_slots
        .update(thread, |slot| slot.environment_owner = retained.clone());
    let mut stale_disposal = disposal.clone();
    stale_disposal.lease.epoch += 1;
    host.dispose_terminal_cleanup_effect(stale_disposal)
        .await
        .expect_err("U6 foreign realization cannot dispose");
    assert!(same_owner(), "U6 denied effects retain original owner");
    assert!(output.exists(), "U6 stale Disposal keeps physical bytes");
    let first_disposal = host
        .dispose_terminal_cleanup_effect(disposal.clone())
        .await
        .expect("U3 exact aggregate-authorized Unbound disposal");
    assert!(!output.exists(), "U3 real physical bytes removed");
    assert!(same_owner(), "U3 retain until durable acknowledgement");
    let receipt = host
        .dispose_terminal_cleanup_effect(disposal.clone())
        .await
        .expect("U4 replay physical Disposal after response loss");
    assert_eq!(first_disposal, receipt, "U4 stable disposal receipt");
    assert!(same_owner(), "U4 retained exact owner");
    aggregate
        .record_terminal_cleanup_disposal(
            &projection.workspace_id,
            &lease,
            receipt,
            "unmaterialized replay",
        )
        .unwrap();
    host.acknowledge_terminal_cleanup_disposal(&disposal).await;
    assert!(
        !host.session_slots.contains(thread),
        "U5 only durable ack releases slot"
    );
}

#[tokio::test]
async fn cold_checkpoint_disposal_cannot_seed_an_owner_without_preparation() {
    // Cause/effect rule: C1 a durable source tuple is exact; C2 the local owner
    // is Vacant because no aggregate-approved Preparation retained it; C3 a
    // typed provider authorization is supplied to the physical-only boundary.
    // Effects: E1 Disposal fails before provider I/O; E2 the owner stays Vacant
    // and no adoption/pending identity is fabricated. R20: C1+C2+C3=>E1+E2.
    // Cold adoption belongs exclusively to the preceding source Preparation
    // phase and is covered end-to-end by the canonical Host continuation tests.
    let thread = "cold-checkpoint-source";
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let generation = generation(thread);
    let binding = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
        "non-resumable",
        thread,
    ))
    .unwrap();
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        9,
        None,
        None,
    );
    let authorization = source_disposal_authorization(&operation.effect_id);
    let error = host
        .dispose_prepared_checkpoint_source_environment(
            thread,
            &operation,
            &generation,
            &binding,
            &authorization,
        )
        .await
        .expect_err("R20/E1 physical Disposal requires retained Preparation");
    assert!(
        error.to_string().contains("no retained Preparation owner"),
        "R20/E1: {error}"
    );
    assert!(host.session_environment_owner_is_vacant(thread), "R20/E2");
}

#[tokio::test]
async fn conflicting_revocation_preserves_active_worker_relay_until_compatible_drain() {
    // Cause/effect decision table: C1 an exact Bound owner is already Retiring
    // for the highest-priority Terminal cause; C2 its WorkerRelay generation is
    // Active with an open exact Route; C3 lower-priority revocation arrives;
    // C4 the canonical exact MCP drain is later invoked compatibly. Effects:
    // E1 C1+C2+C3 returns Err with the same Retiring owner; E2 exact Route,
    // open fence, and Active projection are unmodified and still admit calls;
    // E3 C4 alone closes/removes the Route, marks Removed, and returns proof.
    // Rules: R1=C1+C2+C3=>E1+E2; R2=R1+C4=>E3. Retirement admission therefore
    // precedes every MCP mutation; an errored revocation cannot consume retry
    // authority from either existing owner.
    let thread = "conflicting-revocation-retains-relay";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_test_resident_session_environment(thread, environment);
    let (relay, mcp_generation) = install_active_worker_relay(&host, thread).await;
    let route_before = relay.route_url(&mcp_generation).unwrap();
    let terminal = host
        .retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal-effect".into(),
            },
            RetirementSelection::Current,
        )
        .unwrap()
        .unwrap();

    let error = host
        .revoke_all_session_realizations()
        .await
        .expect_err("R1 conflicting revocation must fail before MCP drain");
    assert!(error.to_string().contains("lower-priority"), "R1/E1");
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(current))
                if current.exact_matches(&terminal.owner)
        ),
        "R1/E1 exact Terminal owner retained"
    );
    assert!(
        host.mcp_projection(&mcp_generation)
            .is_some_and(|projection| projection.state
                == crate::session_slot::McpProjectionState::Active
                && projection.server.is_some()),
        "R1/E2 Active MCP owner unchanged"
    );
    assert_eq!(
        relay.route_url(&mcp_generation).as_deref(),
        Some(route_before.as_str()),
        "R1/E2 exact Route retained"
    );
    let still_open = relay
        .route_call_fence(&mcp_generation)
        .unwrap()
        .try_enter()
        .expect("R1/E2 route fence remains open");
    drop(still_open);

    let proof = host
        .drain_mcp_projections(thread, std::slice::from_ref(&mcp_generation))
        .await
        .expect("R2 compatible exact MCP drain");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&mcp_generation),
        "R2/E3"
    );
    assert!(relay.route_url(&mcp_generation).is_none(), "R2/E3");
    assert_eq!(
        host.mcp_projection(&mcp_generation).unwrap().state,
        crate::session_slot::McpProjectionState::Removed,
        "R2/E3"
    );
}

#[tokio::test]
async fn failed_revocation_retains_provider_mount_and_pending_owner_for_retry() {
    // Cause/effect table: C1 durable pending binding has no local Arc; C2 exact
    // frozen provider and SandboxSpec mount inputs exist; C3 provider adoption
    // cannot be completed; C4 revocation is attempted before ordinary runtime
    // authority has been released;
    // C5 an exact WorkerRelay Route and Active MCP projection already exist.
    // Effects: E1 return Err; E2 retain pending binding; E3 retain the exact
    // provider publication and mount/environment projection needed by the next
    // terminal/revocation retry; E4 retain the open Route/fence and Active MCP
    // owner without an early drain. Rule R19/R20: C1+C2+C3+C4+C5=>E1+E2+E3+E4.
    let thread = "revocation-retains-retry-authority";
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("revocation-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "provider", "model", "backend",
        ))
        .build();
    host.session_slots.update(thread, |slot| {
        slot.published_snapshot = Some(snapshot.clone());
    });
    host.register_thread_resources(
        thread,
        crate::provisioning::StagedResources {
            mounts: vec![retry_input_mount()],
            ..Default::default()
        },
    );
    let environment_projection = awaken_session_contract::EnvironmentSnapshot {
        environment_id: thread.into(),
        revision: awaken_session_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
            "revocation-environment".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::None,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    host.install_environment_projection(thread, &environment_projection)
        .expect("C2 Environment projection");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: "not-a-sandbox-handle".into(),
            effect_id: Some("durable-effect".into()),
            generation: Some(generation(thread)),
            idle_since_unix_ms: None,
        },
    )
    .expect("C1 pending owner");
    let (relay, mcp_generation) = install_active_worker_relay(&host, thread).await;
    let route_before = relay.route_url(&mcp_generation).unwrap();
    let spec_before = host.sandbox_spec(thread);
    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    assert!(
        host.retire_session_environment_for_revocation(thread)
            .await
            .is_err(),
        "R19/E1"
    );
    assert_eq!(
        host.durable_session_environment_binding(thread).as_deref(),
        Some("not-a-sandbox-handle"),
        "R20/E2"
    );
    assert_eq!(
        host.sandbox_spec(thread),
        spec_before,
        "R20/E3 mounts/projection"
    );
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten()
            .map(|snapshot| snapshot.id),
        Some(snapshot.id),
        "R20/E3 provider publication"
    );
    assert!(
        host.mcp_projection(&mcp_generation)
            .is_some_and(|projection| projection.state
                == crate::session_slot::McpProjectionState::Active
                && projection.server.is_some()),
        "R20/E4 Active MCP owner retained"
    );
    assert_eq!(
        relay.route_url(&mcp_generation).as_deref(),
        Some(route_before.as_str()),
        "R20/E4 exact Route retained"
    );
    let still_open = relay
        .route_call_fence(&mcp_generation)
        .unwrap()
        .try_enter()
        .expect("R20/E4 route fence remains open");
    drop(still_open);
}

#[tokio::test]
async fn live_revocation_retains_retiring_owner_and_its_retry_inputs() {
    // Cause/effect rule: C1 revocation selects one exact Resident Arc; C2 stop
    // succeeds but status remains live; C3 ordinary runtime projections are
    // revoked; C4 Environment quiescence already closed MCP admission. Effects:
    // E1 retain the exact hidden Revocation Retiring owner; E2 retain its frozen
    // provider publication and SandboxSpec mount inputs for terminal/revocation
    // retry; E3 preserve the exact closed fence so revocation cannot masquerade
    // as an authoritative restore/source-disposal reopen. Only an exact
    // Terminated observation may clear C1, so C2 is not a successful physical
    // cleanup; only the canonical durable projection proof may clear C4.
    // C5 a live realization lease exists before stop => E4 retained owner reset
    // preserves Closed(lease), never Uninstalled/Live or direct-assertion fallback.
    let thread = "revocation-retiring-retry-authority";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("revocation-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "provider", "model", "backend",
        ))
        .build();
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker/1".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    host.install_session_realization_lease(thread, lease.clone())
        .unwrap();
    host.session_slots.update(thread, |slot| {
        slot.published_snapshot = Some(snapshot.clone());
    });
    host.register_thread_resources(
        thread,
        crate::provisioning::StagedResources {
            mounts: vec![retry_input_mount()],
            ..Default::default()
        },
    );
    host.install_test_resident_session_environment(thread, environment.clone());
    let generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        1,
        u64::MAX,
        "environment",
        "image",
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let fence = crate::session_slot::McpQuiescenceAdmissionFence::new(
        &operation,
        "source-effect",
        &serde_json::to_string(&environment.handle()).unwrap(),
        &generation,
    );
    host.session_slots
        .close_mcp_realization_admission(thread, fence.clone())
        .unwrap();
    let spec_before = host.sandbox_spec(thread);
    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    host.retire_session_environment_for_revocation(thread)
        .await
        .expect("C2 live revocation");
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.realization_authority.clone()),
        Some(crate::session_slot::SessionRealizationAuthority::Closed(
            Some(lease.clone())
        )),
        "E4"
    );
    assert!(
        host.install_session_realization_lease(thread, lease)
            .is_err()
    );
    assert!(
        host.session_slots.realization_leases().is_empty(),
        "E4 replay stays closed"
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Bound(owned),
            })) if Arc::ptr_eq(&owned.environment, &environment)
        ),
        "E1"
    );
    assert_eq!(host.sandbox_spec(thread), spec_before, "E2 SandboxSpec");
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten()
            .map(|current| current.id),
        Some(snapshot.id),
        "E2 provider publication"
    );
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.mcp_quiescence_fence.clone()),
        Some(Some(fence)),
        "E3 revocation is not a quiescence-fence reopen authority"
    );
}

#[tokio::test]
async fn cancelled_adoption_retries_the_same_hidden_candidate_without_readopting() {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, BlockingBindingSink, eager_environment,
        empty_frozen_projection_for_snapshot, managed_test_host, test_activation,
    };
    use awaken_session_contract::SessionRuntime as _;

    // Cause/effect decision table: C1 a frozen provider adopts one durable
    // binding; C2 the Future is cancelled inside durable persistence after
    // provider return; C3 the authorized caller retries. Effects: E1 provider
    // return is captured immediately as one hidden Candidate Arc; E2 C2
    // retains that Arc and releases the lifecycle guard;
    // E3 C3 persists and publishes the same Arc without another provider
    // adoption or by-value owner. Rule R19/R20: C1+C2=>E1+E2;
    // C1+C2+C3=>E3. Fixture constraint: both process incarnations install the
    // production DispatchSessionRuntime before constructing executable context.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-adoption-retry";
    let activation = test_activation(thread, "run-adoption-retry-publication");
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-adoption-retry".into(),
        runtime_incarnation: "worker-adoption-retry:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    let mut projection = empty_frozen_projection_for_snapshot(
        "workspace",
        eager_environment(),
        &activation.snapshot,
    );
    projection.agent_publication = Some(activation.snapshot.clone());
    let first =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let first_managed = managed_test_host(first.clone());
    let first_sink = Arc::new(BlockingBindingSink::blocked());
    first_sink.unblock();
    first_managed.install_environment_binding_sink(first_sink);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        awaken_session_contract::SessionRuntime::install_session_projection(
            &first_managed,
            thread,
            projection.clone(),
            awaken_session_contract::SessionProjectionInstallMode::Realization {
                lease: crate::session_slot::test_observed_lease(lease.clone()),
                prepare_session: true,
            },
        ),
    )
    .await
    .expect("first Realization projection must not block")
    .expect("install first complete Realization projection");
    let first_ctx = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        first.ctx_for_snapshot(thread, Some("agent-a"), Some(activation.snapshot.clone())),
    )
    .await
    .expect("first Session creation must not block")
    .expect("first Session");
    let handle = first_ctx.env.as_ref().expect("first Environment").handle();
    let binding = serde_json::to_string(&handle).unwrap();
    drop(first_ctx);
    drop(first_managed);
    drop(first);

    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let managed = managed_test_host(host.clone());
    let sink = Arc::new(BlockingBindingSink::blocked());
    managed.install_environment_binding_sink(sink.clone());
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        awaken_session_contract::SessionRuntime::install_session_projection(
            &managed,
            thread,
            projection,
            awaken_session_contract::SessionProjectionInstallMode::Realization {
                lease: crate::session_slot::test_observed_lease(lease),
                prepare_session: true,
            },
        ),
    )
    .await
    .expect("retry Realization projection must not block")
    .expect("install retry complete Realization projection");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: Some("prior-effect".into()),
            generation: Some(awaken_session_contract::SandboxGeneration::new(
                thread,
                1,
                u64::MAX,
                "environment",
                "image",
            )),
            idle_since_unix_ms: None,
        },
    )
    .expect("durable pending adoption");

    let adoption = {
        let host = host.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            host.adopt_bound_session_environment(
                thread,
                Some(&binding),
                &host.session_provider,
                None,
                false,
            )
            .await
        })
    };
    if tokio::time::timeout(std::time::Duration::from_secs(2), sink.entered.notified())
        .await
        .is_err()
    {
        if adoption.is_finished() {
            panic!(
                "R19/C1 adoption failed before durable persistence: {:?}",
                adoption.await
            );
        }
        panic!("R19/C1 adoption did not reach durable persistence");
    }
    let candidate = host
        .prepared_session_environment(thread)
        .expect("R19/E1 hidden Candidate before cancellation");
    assert!(host.session_environment(thread).await.is_none(), "R19/E1");
    adoption.abort();
    let cancellation = match adoption.await {
        Err(error) => error,
        Ok(_) => panic!("R19/C2 adoption unexpectedly completed"),
    };
    assert!(cancellation.is_cancelled(), "R19/C2");
    let retained = host
        .prepared_session_environment(thread)
        .expect("R19/E2 hidden Candidate retained");
    assert!(candidate.exact_matches(&retained), "R19/E2");

    sink.unblock();
    let disposition = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        host.adopt_bound_session_environment(
            thread,
            Some(&binding),
            &host.session_provider,
            None,
            false,
        ),
    )
    .await
    .expect("R20/E3 retry must not re-adopt or block on provider ownership")
    .expect("R20/E3 retry");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::Ready,
        "R20/E3 no by-value duplicate or rebuild path"
    );
    let resident = host
        .session_environment(thread)
        .await
        .expect("R20/E3 published Resident");
    assert!(Arc::ptr_eq(&candidate.environment, &resident), "R20/E3");
    assert_eq!(sink.calls(), 2, "R19/R20 sink retries");
}

#[tokio::test]
async fn newer_projection_replaces_only_a_descriptor_and_preserves_every_physical_owner() {
    // C1 Vacant/pending/Candidate/Resident/Retiring; C2 old/equal vs admitted
    // newer root; C3 foreign physical binding B. R1 pending+newer accepts B;
    // R2 pending without newer proof rejects; R3 every physical phase rejects B
    // even with newer root. Revocation, discard, terminal and checkpoint each
    // retain their exact owner: root ordering never substitutes for closure.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let env = environment(&provider, "owner-A").await;
    let bound = BoundSessionEnvironment {
        identity: identity("owner-A"),
        binding: serde_json::to_string(&env.handle()).unwrap(),
        environment: env.clone(),
    };
    for newer in [false, true] {
        let mut pending =
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                identity: bound.identity.clone(),
                binding: bound.binding.clone(),
            });
        assert_eq!(
            pending
                .install_projection(
                    ProjectedEnvironmentOwner::AwaitingAdoption {
                        identity: identity("owner-B"),
                        binding: "B".into()
                    },
                    newer
                )
                .is_ok(),
            newer,
            "R1/R2"
        );
        let mut owners = vec![
            SessionEnvironmentOwner::Resident(bound.clone()),
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                UnboundSessionEnvironment {
                    origin: UnboundSessionEnvironmentOrigin::Adoption,
                    binding: bound.binding.clone(),
                    environment: env.clone(),
                },
            )),
        ];
        for cause in [
            SessionEnvironmentRetirementCause::RealizationRevocation,
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal-A".into(),
            },
            SessionEnvironmentRetirementCause::CheckpointSource {
                operation: awaken_session_contract::SessionEnvironmentOperation::new(
                    "workspace",
                    "owner-A",
                    "suspend",
                    &generation("owner-A"),
                    1,
                    None,
                    None,
                ),
                generation: generation("owner-A"),
            },
        ] {
            owners.push(SessionEnvironmentOwner::Retiring(
                RetiringSessionEnvironment {
                    cause,
                    owned: RetiringEnvironmentOwner::Bound(bound.clone()),
                },
            ));
        }
        for mut owner in owners {
            let phase = std::mem::discriminant(&owner);
            assert!(
                owner
                    .install_projection(
                        ProjectedEnvironmentOwner::AwaitingAdoption {
                            identity: identity("owner-B"),
                            binding: "B".into()
                        },
                        newer
                    )
                    .is_err(),
                "R3"
            );
            assert_eq!(std::mem::discriminant(&owner), phase);
            let retained = match &owner {
                SessionEnvironmentOwner::Resident(o) => &o.environment,
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(o)) => {
                    &o.environment
                }
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    owned: RetiringEnvironmentOwner::Bound(o),
                    ..
                }) => &o.environment,
                _ => panic!("R3 unexpected phase"),
            };
            assert!(Arc::ptr_eq(retained, &env), "R3 exact Arc retained");
        }
    }
}
