// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Real kube HTTP boundaries, with one deterministic API response-loss scheduler
//! shared by cleanup and checkpoint participant tests.
use super::*;
use awaken_container_contract::ContainerEffect;
use awaken_provisioning_contract::test_support::FenceTamper as _;
use pc::test_support::EffectAuthority;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};

use super::participants::tests::ClosedProjectedRequests;

mod retained_plan;
mod retained_source;

#[tokio::test]
async fn original_joined_empty_runs_the_provider_terminal_protocol_but_never_adopts_for_execution()
{
    // JE1: old immutable Pod has no new handoff, original broker group is
    // sealed/Complete with exactly its Pod UID, and frozen plan is narrow empty.
    // Actual provider preparation -> caller-authorized preparation receipt ->
    // disposal must remove that exact Pod. JE2: the same terminal-only carrier
    // cannot adopt for ordinary work or start an exec, while UID queries remain
    // available to the cleanup protocol. The fixture mocks the trusted original
    // Complete group; it does not qualify a real Registry exit or cluster.
    use awaken_container_contract::{ContainerEnvironmentAdoption, ContainerEnvironmentProvider};
    let (runtime, state, _, _, server) = fixture(0, false).await;
    let mut spec = crate::tests::spec("joined-provider");
    spec.mounts.clear();
    spec.env.clear();
    spec.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
    spec.control_services.clear();
    // This original fixture is a Kubernetes-supported demand. PID isolation
    // refusal remains independently covered by the runtime admission table.
    spec.limits.pids = None;
    let clock = awaken_time::system_clock();
    let image = "joined-provider-image";
    let plan = crate::container_plan_with_allowlist(
        &spec,
        image,
        &crate::environment_keepalive_command(),
        None,
        None,
        Some(&clock),
    )
    .unwrap();
    let original_name = pod_name(&runtime.realization_runtime_id(&spec.scope).unwrap());
    let ticket = CreateTicket {
        request: CreateTicketRequest::new(
            "old-group",
            "pods",
            "default",
            &original_name,
            "a".repeat(64),
        )
        .unwrap(),
        status: TicketStatus::Finished(TicketResult::Created {
            uid: "pod-uid".into(),
        }),
        origin: Default::default(),
        isolation_id: String::new(),
        isolation_pending: false,
    };
    let runtime = runtime
        .with_create_transport(Arc::new(ClosedProjectedRequests {
            tickets: Arc::new(Mutex::new(vec![ticket])),
            complete: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }))
        .unwrap();
    let fingerprint = crate::runtime::container_adoption_fingerprint(
        &spec,
        &plan,
        None,
        &runtime.realization_configuration().unwrap(),
    );
    let realization =
        crate::runtime::container_realization_fingerprint(&spec, &fingerprint, &plan.image);
    let create =
        pc::SandboxEffectFence::new("create", "old-owner", "old-boot", 1, u64::MAX - 1).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("cleanup", "old-owner", "old-boot", 2, u64::MAX - 1).unwrap();
    let attempt = ContainerCreateAttempt::new("original-attempt");
    let intent = ContainerRealizationIntent::Create;
    let context = ContainerRealizationContext::new(
        &spec.scope,
        &fingerprint,
        Some(&create),
        &intent,
        &attempt,
    );
    let id = runtime.realization_runtime_id(&spec.scope).unwrap();
    let mut original = runtime.pod_for_effect(&id, &plan, Some(&create));
    crate::k8s_package_realization::stamp_sandbox_release_annotations(
        &mut original,
        &spec.scope,
        &plan.image,
    );
    runtime.stamp_effect_evidence(&mut original, &context, &realization);
    stamp_pod_realization(&mut original).unwrap(); // exact original historical producer shape
    original.metadata.uid = Some("pod-uid".into());
    original.metadata.resource_version = Some("1".into());
    original.metadata.namespace = Some("default".into());
    original.status = Some(serde_json::from_value(json!({"phase":"Succeeded"})).unwrap());
    assert_eq!(
        original.metadata.name.as_deref(),
        Some(original_name.as_str())
    );
    {
        let mut state = state.lock().unwrap();
        state.pod = Some(original);
        state.claim = None;
        state.projected.clear();
    }
    let provider = crate::ContainerProvider::new(Arc::new(runtime), image).with_clock(clock);
    let authority = EffectAuthority::new(&terminal);
    let environment = provider
        .prepare_terminal_environment_for_effect(&spec, None, Some(&create), &terminal, &authority)
        .await
        .unwrap()
        .expect("JE1 original joined Pod is retained for terminal cleanup");
    let handle = environment.handle();
    assert_eq!(
        provider.physical_incarnation(&handle).unwrap(),
        "pod-uid",
        "physical fact is not execution authority"
    );
    assert!(
        provider
            .adopt_unattached_environment(ContainerEnvironmentAdoption::new(&spec, &handle))
            .await
            .is_err(),
        "JE2 ordinary adoption"
    );
    assert!(
        environment.spawn(pc::Command::new(["true"])).await.is_err(),
        "JE2 no Session exec"
    );
    // JE4: a returned JoinedEmpty carrier is the same physical-only proof on
    // replay, even if its never-adopted original is still provisioning. The
    // new published-plan empty witness must not alter this existing role.
    state.lock().unwrap().pod.as_mut().unwrap().status =
        Some(serde_json::from_value(json!({"phase":"Pending"})).unwrap());
    let replay = provider
        .prepare_terminal_environment_for_effect(
            &spec,
            Some(&handle),
            Some(&create),
            &terminal,
            &authority,
        )
        .await
        .expect("JE4 JoinedEmpty replay keeps physical-only preparation")
        .unwrap();
    assert_eq!(replay.handle(), handle, "JE4 retains the exact witness");
    assert!(
        state.lock().unwrap().writes.is_empty(),
        "JE4 no physical preparation writes"
    );
    state.lock().unwrap().pod.as_mut().unwrap().status =
        Some(serde_json::from_value(json!({"phase":"Succeeded"})).unwrap());
    let prepared = environment
        .prepare_disposal_for_effect(&terminal, &authority)
        .await
        .unwrap();
    let prepared = pc::SandboxDisposalPreparation::new(
        prepared,
        handle.realization_fingerprint().unwrap().as_str(),
    )
    .unwrap();
    // JE3: the existing preparation law rejects the old operation id. The
    // caller derives the disposal identity from this same durable preparation,
    // preserving holder/epoch/expiry; no fresh unrelated receipt is invented.
    assert!(
        prepared.authorize(terminal.clone()).is_err(),
        "JE3 wrong operation cannot authorize disposal"
    );
    let source = prepared.prepared_effect_fence();
    let current = pc::SandboxEffectFence::new(
        prepared.operation_id().unwrap(),
        source.owner(),
        source.runtime_incarnation(),
        source.epoch(),
        source.expires_at_unix_ms(),
    )
    .unwrap();
    let authority = EffectAuthority::new(&current);
    let authorization = prepared.authorize(current).unwrap();
    environment
        .dispose_for_effect(&authorization, &authority)
        .await
        .unwrap();
    assert!(
        state.lock().unwrap().pod.is_none(),
        "JE1 actual exact Pod absence"
    );
    assert!(
        state
            .lock()
            .unwrap()
            .writes
            .iter()
            .any(|call| call == "DELETE pods")
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn complete_original_inline_handoff_uses_the_provider_disposal_protocol() {
    // Cause/effect table: C1 real producer retained complete declaration/results
    // and original POST UID tickets; C2 original RO text+binary frozen inputs,
    // Pod UID/spec/fence and original member UIDs; C3 current terminal authority;
    // C4 DELETE accepted but its reply lost; C5 Pod gone before disposal while
    // original GC members survive; C6 replacement UID or missing ticket/results.
    // RP1=C1+C2+C3 => provider prepare -> same durable preparation -> authorized
    // exact-UID disposal -> all original objects absent. Preparation has zero
    // physical deletes and retains both owned/excluded paths. RP2=RP1+C4 =>
    // retry the same handle/receipt, never new Create or sampled replacement UID.
    // RP3=RP1+C5 => pending until original GC absence, with zero orphan deletes.
    // RP4=C6 => Unknown before preparation/delete; no partial/empty set.
    // These real kube HTTP adapter calls mock the trusted closed group. They
    // do not qualify Registry process exit, resource-domain access or a cluster.
    // Control extension table: CC1 supported frozen RepositoryGitCredential
    // sidecar plus original complete Created V3 set and Terminal -> same Pod
    // Control UID/topology, closed publication slot, zero New/Adopt/channel,
    // then the same authorized UID disposal. CC2 missing original handoff or
    // altered pinned forwarder projection -> Unknown; no JoinedEmpty borrow.
    // CC3 Ready/Provisioning -> NotReady before owner transfer/live binding.
    // This adds no Memory/Secret source permission or old-execution closure.
    use crate::ContainerHandleCodec as _;
    use awaken_container_contract::ContainerEnvironmentProvider as _;
    for row in [
        "complete",
        "delete-response-loss",
        "gc-after-pod-loss",
        "foreign-member",
        "missing-ticket",
        "missing-results",
        "control-terminal",
        "control-empty",
        "control-missing-results",
        "control-empty-no-results",
        "control-no-original-ticket",
        "control-ready",
        "control-provisioning",
        "control-image-drift",
        "control-port-drift",
    ] {
        let (mut source, state, reached, release, server) = fixture(0, false).await;
        let control = row.starts_with("control-");
        source.continuation_volume = None;
        let scope = format!("rich-provider-{row}");
        let mut spec = crate::tests::spec(&scope);
        spec.mounts.clear();
        spec.env.clear();
        spec.control_services.clear();
        spec.limits.pids = None;
        spec.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
        for (name, mount_source) in [
            (
                "text",
                pc::MountSource::Inline {
                    contents: "original text".into(),
                },
            ),
            (
                "binary",
                pc::MountSource::InlineBytes {
                    contents: vec![0xff, 0, 1],
                    content_hash: None,
                },
            ),
        ] {
            spec.mounts.push(pc::MountRequirement {
                mount_id: name.into(),
                source: mount_source,
                mount_path: format!("/mnt/{name}"),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            });
        }
        if control {
            spec.control_services
                .insert(SandboxControlServiceKind::RepositoryGitCredential);
            source = source.with_sandbox_control_forwarder(
                K8sSandboxControlForwarder::new(
                    format!("registry.test/control@sha256:{}", "a".repeat(64)),
                    "/usr/local/bin/awaken-sandbox",
                    7778,
                )
                .unwrap(),
            );
        }
        if matches!(row, "control-empty" | "control-empty-no-results") {
            spec.mounts.clear();
        }
        let clock = awaken_time::system_clock();
        let image = "rich-provider-image";
        let plan = crate::container_plan_with_allowlist(
            &spec,
            image,
            &crate::environment_keepalive_command(),
            None,
            None,
            Some(&clock),
        )
        .unwrap();
        let original_tickets = Arc::new(Mutex::new(Vec::new()));
        let mut recovery = K8sRuntime::for_test(state.lock().unwrap().server_addr);
        recovery.clients.control = source.clients.control.clone();
        if control {
            recovery = recovery.with_sandbox_control_forwarder(
                K8sSandboxControlForwarder::new(
                    format!("registry.test/control@sha256:{}", "a".repeat(64)),
                    "/usr/local/bin/awaken-sandbox",
                    7778,
                )
                .unwrap(),
            );
        }
        let recovery = recovery
            .with_create_transport(Arc::new(ClosedProjectedRequests {
                tickets: original_tickets.clone(),
                complete: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            }))
            .unwrap();
        let adoption = crate::runtime::container_adoption_fingerprint(
            &spec,
            &plan,
            None,
            &recovery.realization_configuration().unwrap(),
        );
        let realization =
            crate::runtime::container_realization_fingerprint(&spec, &adoption, &plan.image);
        let create =
            pc::SandboxEffectFence::new("rich-create", "old-owner", "old-boot", 1, u64::MAX - 1)
                .unwrap();
        let terminal =
            pc::SandboxEffectFence::new("rich-cleanup", "old-owner", "old-boot", 2, u64::MAX - 1)
                .unwrap();
        let original_authority = EffectAuthority::new(&create);
        let attempt = ContainerCreateAttempt::new("original-rich-attempt");
        let intent = ContainerRealizationIntent::Create;
        let context = ContainerRealizationContext::new(
            &spec.scope,
            &adoption,
            Some(&create),
            &intent,
            &attempt,
        )
        .with_effect(ContainerEffect::Fenced {
            fence: &create,
            authority: &original_authority,
        });
        let id = source.realization_runtime_id(&spec.scope).unwrap();
        let mut pod = source.pod_for_effect(&id, &plan, Some(&create));
        crate::k8s_package_realization::stamp_sandbox_release_annotations(
            &mut pod,
            &spec.scope,
            &plan.image,
        );
        source.stamp_effect_evidence(&mut pod, &context, &realization);
        participants::declare(&mut pod, &id, &plan).unwrap();
        stamp_pod_realization(&mut pod).unwrap();
        pod.metadata.uid = Some("pod-uid".into());
        pod.metadata.resource_version = Some("1".into());
        pod.metadata.namespace = Some("default".into());
        pod.status = Some(serde_json::from_value(json!({"phase":"Succeeded"})).unwrap());
        let nonce = "0123456789abcdef0123456789abcdef";
        pod.metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(ISOLATION_ID_ANNOTATION.into(), nonce.into());
        pod.metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(REQUEST_DIGEST_ANNOTATION.into(), "a".repeat(64));
        let pod_name = pod.metadata.name.clone().unwrap();
        original_tickets.lock().unwrap().push(CreateTicket {
            request: CreateTicketRequest::new(
                "old-group",
                "pods",
                "default",
                &pod_name,
                "a".repeat(64),
            )
            .unwrap(),
            status: TicketStatus::Finished(TicketResult::Created {
                uid: "pod-uid".into(),
            }),
            origin: Default::default(),
            isolation_id: nonce.into(),
            isolation_pending: false,
        });
        {
            let mut state = state.lock().unwrap();
            state.pod = Some(pod.clone());
            state.claim = None;
            state.projected.clear();
            state.accept_projected_creates = true;
        }
        // Invoke the canonical producer: both member POST results and Pod UID/RV
        // CAS publication happen through the same HTTP fixture as disposal.
        let original_set = participants::realize(
            &source,
            pod,
            &id,
            &plan,
            Some((&context, &realization)),
            None,
        )
        .await
        .unwrap();
        let source_writes = {
            let mut state = state.lock().unwrap();
            state.accept_projected_creates = false;
            original_tickets
                .lock()
                .unwrap()
                .extend(state.created_tickets.clone());
            state.writes.len()
        };
        assert_eq!(original_set.objects.len(), spec.mounts.len());
        let first = original_set.objects.first();
        match row {
            "foreign-member" => {
                state.lock().unwrap().projected_behavior.insert(
                    first
                        .expect("inline row has an original member")
                        .name
                        .clone(),
                    "replacement",
                );
            }
            "missing-ticket" => {
                original_tickets.lock().unwrap().retain(|t| {
                    t.request.name != first.expect("inline row has an original member").name
                });
            }
            "control-no-original-ticket" => {
                original_tickets.lock().unwrap().clear();
            }
            "control-ready" | "control-provisioning" => {
                let mut state = state.lock().unwrap();
                let pod = state.pod.as_mut().unwrap();
                pod.status = Some(serde_json::from_value(if row == "control-ready" {
                    json!({"phase":"Running", "containerStatuses":pod.spec.as_ref().unwrap().containers.iter().map(|v| json!({
                        "name":v.name,"ready":true,"image":"image@test","imageID":"image@test",
                        "restartCount":0,"started":true,"state":{"running":{}}
                    })).collect::<Vec<_>>()})
                } else { json!({"phase":"Pending"}) }).unwrap());
            }
            "control-image-drift" | "control-port-drift" => {
                let mut state = state.lock().unwrap();
                let pod = state.pod.as_mut().unwrap();
                let forwarder = pod
                    .spec
                    .as_mut()
                    .unwrap()
                    .containers
                    .iter_mut()
                    .find(|v| v.name == "sandbox-control")
                    .unwrap();
                if row == "control-image-drift" {
                    forwarder.image =
                        Some(format!("registry.test/control@sha256:{}", "b".repeat(64)));
                } else {
                    forwarder.ports.as_mut().unwrap()[0].container_port = 7779;
                }
            }
            "missing-results" | "control-missing-results" | "control-empty-no-results" => {
                state
                    .lock()
                    .unwrap()
                    .pod
                    .as_mut()
                    .unwrap()
                    .metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .remove("awaken.dev/projected-object-results");
            }
            _ => {}
        }
        let provider = crate::ContainerProvider::new(Arc::new(recovery), image).with_clock(clock);
        let terminal_authority = EffectAuthority::new(&terminal);
        let result = provider
            .prepare_terminal_environment_for_effect(
                &spec,
                None,
                Some(&create),
                &terminal,
                &terminal_authority,
            )
            .await;
        if matches!(
            row,
            "foreign-member"
                | "missing-ticket"
                | "missing-results"
                | "control-missing-results"
                | "control-empty-no-results"
                | "control-no-original-ticket"
                | "control-image-drift"
                | "control-port-drift"
        ) {
            match result {
                Err(error) => assert!(error.to_string().contains("container provider cannot dispose an in-flight restore without a durable handle"), "RP4/{row}: {error}"),
                Ok(_) => panic!("RP4 incomplete or foreign original set cannot prepare/{row}"),
            }
            assert_eq!(
                state.lock().unwrap().writes.len(),
                source_writes,
                "RP4 no mutation/{row}"
            );
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            continue;
        }
        if matches!(row, "control-ready" | "control-provisioning") {
            match result {
                Err(error) => assert!(
                    error.to_string().contains(
                        "cold terminal Control reconstruction requires a closed original source"
                    ),
                    "CC3/{row}: {error}"
                ),
                Ok(_) => panic!(
                    "CC3 live/unready Control cannot rebind during cold terminal reconstruction"
                ),
            }
            assert_eq!(
                state.lock().unwrap().writes.len(),
                source_writes,
                "CC3 zero owner-transfer/republication"
            );
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            continue;
        }
        let environment = Arc::new(result.unwrap().expect("RP1 exact original rich source"));
        let handle = environment.handle();
        assert!(
            matches!(handle.container_payload().unwrap().runtime_handle.as_ref(),
            Some(crate::ContainerContinuationHandle::KubernetesContinuationV3 { pod_uid, participants, .. })
            if pod_uid == "pod-uid" && participants.as_ref() == &original_set && !participants.terminal_only())
        );
        if control {
            let payload = handle.container_payload().unwrap();
            assert_eq!(
                payload.control_services, spec.control_services,
                "CC1 frozen topology"
            );
            assert_eq!(
                payload
                    .sandbox_control_incarnation
                    .as_ref()
                    .and_then(crate::SandboxControlIncarnation::kubernetes_pod_uid),
                Some("pod-uid"),
                "CC1 same original UID"
            );
        }
        let expected_paths = spec
            .mounts
            .iter()
            .map(|m| m.mount_path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            handle.owned_paths().unwrap(),
            expected_paths,
            "RP1 original owned paths"
        );
        assert_eq!(
            handle
                .container_payload()
                .unwrap()
                .continuation_excluded_paths,
            expected_paths,
            "RP1 original excluded paths"
        );
        let prepared = environment
            .prepare_disposal_for_effect(&terminal, &terminal_authority)
            .await
            .unwrap();
        let prepared = pc::SandboxDisposalPreparation::new(
            prepared,
            handle.realization_fingerprint().unwrap().as_str(),
        )
        .unwrap();
        assert!(
            prepared.authorize(terminal.clone()).is_err(),
            "RP1 unrelated operation remains rejected"
        );
        assert_eq!(
            state.lock().unwrap().writes.len(),
            source_writes,
            "RP1 preparation does not delete"
        );
        let source_fence = prepared.prepared_effect_fence();
        let current = pc::SandboxEffectFence::new(
            prepared.operation_id().unwrap(),
            source_fence.owner(),
            source_fence.runtime_incarnation(),
            source_fence.epoch(),
            source_fence.expires_at_unix_ms(),
        )
        .unwrap();
        let current_authority = Arc::new(EffectAuthority::new(&current));
        let authorization = prepared.authorize(current).unwrap();
        if row == "delete-response-loss" {
            state.lock().unwrap().cut_after_call = Some(format!(
                "DELETE /api/v1/namespaces/default/configmaps/{}",
                first.expect("inline row has an original member").name
            ));
            let task = tokio::spawn({
                let (environment, authorization, authority) = (
                    environment.clone(),
                    authorization.clone(),
                    current_authority.clone(),
                );
                async move {
                    environment
                        .dispose_for_effect(&authorization, authority.as_ref())
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), reached.notified())
                .await
                .unwrap();
            assert!(
                !state
                    .lock()
                    .unwrap()
                    .projected
                    .contains(&first.expect("inline row has an original member").name),
                "RP2 DELETE accepted before lost response"
            );
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            release.add_permits(1);
        }
        if row == "gc-after-pod-loss" {
            let calls_before = {
                let mut state = state.lock().unwrap();
                state.pod = None;
                state.calls.len()
            };
            let mut task = tokio::spawn({
                let (environment, authorization, authority) = (
                    environment.clone(),
                    authorization.clone(),
                    current_authority.clone(),
                );
                async move {
                    environment
                        .dispose_for_effect(&authorization, authority.as_ref())
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.lock().unwrap().calls.len() < calls_before + 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                !task.is_finished(),
                "RP3 surviving original members prevent completion"
            );
            assert_eq!(
                state.lock().unwrap().writes.len(),
                source_writes,
                "RP3 no orphan deletes"
            );
            state.lock().unwrap().projected.clear(); // actual observed GC absence in this API fixture
            tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            environment
                .dispose_for_effect(&authorization, current_authority.as_ref())
                .await
                .unwrap();
        }
        // Lost-return replay uses this same carrier and preparation, after all
        // UIDs are gone. It neither republishes RESULTS nor reissues Create.
        let completed_writes = state.lock().unwrap().writes.len();
        environment
            .dispose_for_effect(&authorization, current_authority.as_ref())
            .await
            .unwrap();
        assert_eq!(environment.handle(), handle, "RP1/RP2 exact same handle");
        {
            let state = state.lock().unwrap();
            assert!(
                state.pod.is_none() && state.claim.is_none() && state.projected.is_empty(),
                "RP1/RP3 complete physical set/{row}"
            );
            assert_eq!(
                state.writes.len(),
                completed_writes,
                "RP2 replay has no writes"
            );
            assert_eq!(state.projected_lists, 0, "no sampled LIST membership");
            if row != "gc-after-pod-loss" {
                assert_eq!(
                    state
                        .writes
                        .iter()
                        .filter(|call| call.as_str() == "DELETE configmaps")
                        .count(),
                    original_set.objects.len(),
                    "RP1/RP2 each original UID is deleted once"
                );
            }
            assert_eq!(
                state
                    .writes
                    .iter()
                    .filter(|call| call.starts_with("POST "))
                    .count(),
                original_set.objects.len(),
                "no new recovery Create"
            );
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn terminal_total_absence_uses_the_same_complete_original_projected_set() {
    // Causes: C1 complete original carrier / legacy V2 missing set; C2 Pod and
    // PVC absent; C3 original CM/Secret UID present, gone, replacement, missing/
    // blank UID, or API failure. Effects: T1 complete+gone/replacement -> physical
    // absence; T2 complete+original present -> Disposing, never AlreadyAbsent;
    // T3 legacy/bad UID/API -> Unknown/error. All rows are read-only. The fixture
    // LIST advertises continuation: no row may borrow any sampled listing as
    // complete membership. Its original set is retained before Pod loss.
    for row in [
        "present",
        "gone",
        "replacement",
        "missing-uid",
        "blank-uid",
        "api-error",
        "legacy",
    ] {
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let mut handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
            .unwrap()
            .unwrap();
        {
            let mut state = state.lock().unwrap();
            state.pod = None;
            state.claim = None;
            if row == "gone" {
                state.projected.clear();
            }
            if row == "replacement" {
                for name in ["configmaps-0", "configmaps-1", "secrets-0", "secrets-1"] {
                    state.projected_behavior.insert(name.into(), "replacement");
                }
            }
            if matches!(row, "missing-uid" | "blank-uid" | "api-error") {
                state.projected_behavior.insert("configmaps-0".into(), row);
            }
        }
        if row == "legacy" {
            handle = crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "pod-uid".into(),
                claim_uid: Some("claim-uid".into()),
            };
        }
        let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
        let fence =
            pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let result = runtime
            .observe(ContainerObservationExpectation {
                purpose: ContainerObservationPurpose::TerminalCleanup,
                container_id: "awaken-cleanup",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: Some(&handle),
                effect_fence: Some(&fence),
                retained_attempt: None,
            })
            .await;
        match row {
            "present" => assert!(
                matches!(result.unwrap(), pc::SandboxObservation::Disposing { .. }),
                "T2"
            ),
            "gone" | "replacement" => assert!(
                matches!(
                    result.unwrap(),
                    pc::SandboxObservation::DefinitivelyUnavailable { .. }
                ),
                "T1/{row}"
            ),
            _ => assert!(result.is_err(), "T3/{row}"),
        }
        {
            let state = state.lock().unwrap();
            assert!(state.writes.is_empty());
            assert_eq!(state.projected_lists, 0);
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn accepted_projected_delete_waits_for_the_original_uid_behind_a_finalizer() {
    // F1/F2 causes: complete original set, exact Pod/PVC and live grant; each
    // kind's DELETE is accepted but original UID remains behind a finalizer.
    // Effects: no success/receipt or Pod release until controller removal of that
    // UID. Cleanup never removes arbitrary finalizers. F3 renew/revoke/cancel at
    // each actual response is covered by KCA, including the new projected GET,
    // DELETE and absence wait. This fixture qualifies wiring, not a real GC.
    for (name, kind) in [("configmaps-0", "configmaps"), ("secrets-0", "secrets")] {
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let handle =
            continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap()).unwrap();
        let accepted = state.lock().unwrap().projected_deleted.clone();
        state
            .lock()
            .unwrap()
            .projected_behavior
            .insert(name.into(), "finalizer");
        let authorization = crate::tests::disposal_authorization(
            &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        let authority = EffectAuthority::new(authorization.effect_fence());
        let mut task = tokio::spawn(async move {
            runtime
                .remove_exact_incarnation(
                    "awaken-cleanup",
                    handle.as_ref(),
                    &authorization,
                    &authority,
                )
                .await
        });
        while !state
            .lock()
            .unwrap()
            .writes
            .iter()
            .any(|call| call == &format!("DELETE {kind}"))
        {
            tokio::time::timeout(Duration::from_secs(5), accepted.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
        }
        assert!(!task.is_finished(), "F1/F2 accepted DELETE is not absence");
        assert!(
            state.lock().unwrap().pod.is_some(),
            "owner is not released before members close"
        );
        state.lock().unwrap().projected.remove(name); // observed controller absence
        tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        {
            let state = state.lock().unwrap();
            assert!(state.projected.is_empty());
            assert_eq!(state.projected_lists, 0);
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn absent_pod_cleanup_response_loss_replay_waits_for_original_gc_members() {
    // G1: Pod/PVC absent + retained original set + surviving original member ->
    // same disposal stays pending and sends zero DELETE while native GC owns
    // removal. G2 cancel/lost response -> retry same handle/UIDs; only actual
    // complete member absence permits completion. No sampled empty LIST or new
    // Create substitutes for the original result set.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    let handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap()).unwrap();
    {
        let mut state = state.lock().unwrap();
        state.pod = None;
        state.claim = None;
    }
    let authorization = crate::tests::disposal_authorization(
        &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
    );
    let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
    let runtime = Arc::new(runtime);
    let task = tokio::spawn({
        let (runtime, handle, authorization, authority) = (
            runtime.clone(),
            handle.clone(),
            authorization.clone(),
            authority.clone(),
        );
        async move {
            runtime
                .remove_exact_incarnation(
                    "awaken-cleanup",
                    handle.as_ref(),
                    &authorization,
                    authority.as_ref(),
                )
                .await
        }
    });
    while state.lock().unwrap().calls.len() < 3 {
        tokio::task::yield_now().await;
    }
    assert!(!task.is_finished(), "G1 original survivor cannot complete");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        state.lock().unwrap().writes.is_empty(),
        "G1/G2 no orphan/replacement delete"
    );
    state.lock().unwrap().projected.clear();
    runtime
        .remove_exact_incarnation(
            "awaken-cleanup",
            handle.as_ref(),
            &authorization,
            authority.as_ref(),
        )
        .await
        .unwrap();
    assert!(state.lock().unwrap().writes.is_empty());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn late_read_only_absence_cannot_acknowledge_a_revoked_disposal_grant() {
    // A1/A2 causes: complete original handle, all physical objects absent,
    // read-only observer reply delayed; actual current grant stable/renewed/
    // revoked/replaced/expired/caller-cancelled at that reply. Effects: only
    // stable/renewed returns success. Other rows return error/cancellation and
    // zero DELETE/receipt. Effect-free probe may finish remaining reads; the
    // shared disposition kernel must re-enter real authority before returning.
    for change in ['s', 'r', 'v', 'g', 'e', 'c'] {
        let (runtime, state, reached, release, server) = fixture(3, false).await;
        let handle =
            continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap()).unwrap();
        {
            let mut state = state.lock().unwrap();
            state.pod = None;
            state.claim = None;
            state.projected.clear();
        }
        let authorization = crate::tests::disposal_authorization(
            &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
        let current = authority.clone();
        let mut task = tokio::spawn(async move {
            let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
            let expectation = ContainerObservationExpectation {
                purpose: ContainerObservationPurpose::TerminalCleanup,
                container_id: "awaken-cleanup",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: handle.as_ref(),
                effect_fence: Some(authorization.effect_fence()),
                retained_attempt: None,
            };
            runtime
                .dispose_authorized(expectation, &authorization, current.as_ref())
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        if change == 'c' {
            task.abort();
        } else {
            let mut grant = authority.current.lock().unwrap();
            match change {
                's' => {}
                'r' => grant.as_mut().unwrap().0.renew(),
                'v' => *grant = None,
                'g' => grant.as_mut().unwrap().0.reassign(),
                'e' => grant.as_mut().unwrap().1 = Duration::ZERO,
                _ => unreachable!(),
            }
        }
        release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap();
        if change == 'c' {
            assert!(result.unwrap_err().is_cancelled());
        } else {
            assert_eq!(
                result.unwrap().is_ok(),
                matches!(change, 's' | 'r'),
                "A1/A2/{change}"
            );
        }
        assert!(state.lock().unwrap().writes.is_empty());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

struct ApiState {
    server_addr: std::net::SocketAddr,
    pod: Option<Pod>,
    claim: Option<PersistentVolumeClaim>,
    pod_posted: Arc<Semaphore>,
    calls: Vec<String>,
    writes: Vec<String>,
    projected: std::collections::HashSet<String>,
    projected_behavior: std::collections::HashMap<String, &'static str>,
    // Original POST responses for rich producer/terminal protocol cases.
    // Old cleanup fixtures retain their existing typed skeleton fallback.
    projected_objects: std::collections::HashMap<String, Value>,
    accept_projected_creates: bool,
    created_tickets: Vec<CreateTicket>,
    cut_after_call: Option<String>,
    projected_lists: usize,
    projected_deleted: Arc<Semaphore>,
    release_conflict: bool,
    read_failure: Option<u16>,
}

impl ApiState {
    fn response(&mut self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let resource = path.split('/').nth(5).unwrap();
        let call = format!("{method} {resource}");
        self.calls.push(call.clone());
        if method == "GET"
            && matches!(resource, "pods" | "persistentvolumeclaims")
            && let Some(code) = self.read_failure
        {
            return (
                code,
                json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"InjectedReadFailure", "message":"read unavailable", "code":code}),
            );
        }
        let missing = || {
            (
                404,
                json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"NotFound", "message":"gone", "code":404}),
            )
        };
        match (method, resource) {
            ("POST", "pods") => {
                if self.pod.is_some() {
                    return (
                        409,
                        json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"AlreadyExists", "message":"occupied", "code":409}),
                    );
                }
                let mut pod: Pod = serde_json::from_value(body).unwrap();
                pod.metadata.uid = Some("delayed-create-uid".into());
                pod.metadata.resource_version = Some("1".into());
                self.pod = Some(pod.clone());
                self.writes.push(call);
                self.pod_posted.add_permits(1);
                (201, json!(pod))
            }
            ("GET", "pods") => self
                .pod
                .as_ref()
                .map_or_else(missing, |pod| (200, json!(pod))),
            ("GET", "persistentvolumeclaims") => self
                .claim
                .as_ref()
                .map_or_else(missing, |claim| (200, json!(claim))),
            ("PUT", "pods") => {
                let mut replacement: Pod = serde_json::from_value(body).unwrap();
                let old = self.pod.as_ref().unwrap();
                if replacement.metadata.uid != old.metadata.uid
                    || replacement.metadata.resource_version != old.metadata.resource_version
                {
                    return (
                        409,
                        json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"Conflict", "message":"stale UID or resourceVersion", "code":409}),
                    );
                }
                let releasing = continuation::pod_cleanup_gate_held(old)
                    && !continuation::pod_cleanup_gate_held(&replacement);
                let guarded = |pod: &Pod| {
                    pod.metadata.finalizers.as_ref().is_some_and(|values| {
                        values
                            .iter()
                            .any(|value| value == create_ledger::ORIGINAL_GUARD_FINALIZER)
                    })
                };
                let releasing_original = guarded(old) && !guarded(&replacement);
                if (releasing || releasing_original) && self.release_conflict {
                    self.release_conflict = false;
                    return (
                        409,
                        json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"Conflict", "message":"retry CAS", "code":409}),
                    );
                }
                self.writes.push(call);
                let version: u64 = old
                    .metadata
                    .resource_version
                    .as_ref()
                    .unwrap()
                    .parse()
                    .unwrap();
                replacement.metadata.resource_version = Some((version + 1).to_string());
                let finalizers_empty = replacement
                    .metadata
                    .finalizers
                    .as_ref()
                    .is_none_or(Vec::is_empty);
                self.pod = if old.metadata.deletion_timestamp.is_some() && finalizers_empty {
                    None
                } else {
                    Some(replacement.clone())
                };
                if releasing {
                    self.claim = None;
                }
                (200, json!(replacement))
            }
            ("DELETE", "pods" | "persistentvolumeclaims") => {
                let metadata = if resource == "pods" {
                    &mut self.pod.as_mut().unwrap().metadata
                } else {
                    &mut self.claim.as_mut().unwrap().metadata
                };
                assert_eq!(body["preconditions"]["uid"], json!(metadata.uid));
                assert_eq!(
                    body["preconditions"]["resourceVersion"],
                    json!(metadata.resource_version)
                );
                metadata.deletion_timestamp =
                    Some(serde_json::from_value(json!("2026-09-07T00:00:00Z")).unwrap());
                self.writes.push(call);
                if resource == "pods" && metadata.finalizers.as_ref().is_none_or(Vec::is_empty) {
                    self.pod = None;
                }
                (
                    200,
                    json!({"kind":"Status", "apiVersion":"v1", "status":"Success"}),
                )
            }
            ("POST", "configmaps" | "secrets") => {
                assert!(
                    self.accept_projected_creates,
                    "cleanup cannot issue projected Create"
                );
                let mut object = body;
                let name = object["metadata"]["name"].as_str().unwrap().to_owned();
                assert!(!self.projected.contains(&name), "original POST occurs once");
                let uid = format!("{name}-uid");
                let nonce = "0123456789abcdef0123456789abcdef";
                let digest = "b".repeat(64);
                object["metadata"]["uid"] = uid.clone().into();
                object["metadata"]["resourceVersion"] = "1".into();
                object["metadata"]["namespace"] = "default".into();
                object["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = nonce.into();
                object["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION] =
                    digest.clone().into();
                self.created_tickets.push(CreateTicket {
                    request: CreateTicketRequest::new(
                        "old-group",
                        resource,
                        "default",
                        &name,
                        digest,
                    )
                    .unwrap(),
                    status: TicketStatus::Finished(TicketResult::Created { uid }),
                    origin: Default::default(),
                    isolation_id: nonce.into(),
                    isolation_pending: false,
                });
                self.projected.insert(name.clone());
                self.projected_objects.insert(name, object.clone());
                self.writes.push(call);
                (201, object)
            }
            ("GET", "configmaps" | "secrets") => {
                let kind = if resource == "configmaps" {
                    "ConfigMap"
                } else {
                    "Secret"
                };
                let object = |name: &str| {
                    let mut object = self.projected_objects.get(name).cloned().unwrap_or_else(|| json!({"kind":kind,"apiVersion":"v1","metadata":{
                        "name":name,"uid":format!("{name}-uid"),"resourceVersion":"1",
                        "annotations":{SANDBOX_ATTEMPT_ANNOTATION:"attempt"},
                        "labels":{crate::SANDBOX_ATTEMPT_LABEL:k8s_attempt_label("attempt")},
                        "ownerReferences":[{"apiVersion":"v1","kind":"Pod","name":"awaken-cleanup","uid":"pod-uid","controller":true}]
                    }}));
                    match self.projected_behavior.get(name).copied() {
                        Some("replacement") => object["metadata"]["uid"] = "replacement-uid".into(),
                        Some("missing-uid") => {
                            object["metadata"].as_object_mut().unwrap().remove("uid");
                        }
                        Some("blank-uid") => object["metadata"]["uid"] = " ".into(),
                        Some("finalizer") => {
                            object["metadata"]["finalizers"] = json!(["fixture/finalizer"]);
                            object["metadata"]["deletionTimestamp"] = "2026-10-12T00:00:00Z".into();
                        }
                        _ => {}
                    }
                    object
                };
                if let Some(name) = path.split('/').nth(6) {
                    if !self.projected.contains(name) {
                        return missing();
                    }
                    if self.projected_behavior.get(name) == Some(&"api-error") {
                        return (
                            500,
                            json!({"kind":"Status","apiVersion":"v1","code":500,"reason":"Unavailable"}),
                        );
                    }
                    (200, object(name))
                } else {
                    let items: Vec<_> = (0..2)
                        .filter(|index| self.projected.contains(&format!("{resource}-{index}")))
                        .map(|index| object(&format!("{resource}-{index}")))
                        .collect();
                    self.projected_lists += 1;
                    (
                        200,
                        json!({"kind":format!("{kind}List"),"apiVersion":"v1","metadata":{"continue":"unread-next-page"},"items":items}),
                    )
                }
            }
            ("DELETE", "configmaps" | "secrets") => {
                let name = path.split('/').nth(6).unwrap();
                let metadata = self.projected_objects.get(name).map(|v| &v["metadata"]);
                assert_eq!(
                    body["preconditions"]["uid"],
                    metadata.map_or_else(|| json!(format!("{name}-uid")), |v| v["uid"].clone())
                );
                assert_eq!(
                    body["preconditions"]["resourceVersion"],
                    metadata.map_or_else(|| json!("1"), |v| v["resourceVersion"].clone())
                );
                assert!(self.projected.contains(name));
                if self.projected_behavior.get(name) != Some(&"finalizer") {
                    self.projected.remove(name);
                }
                self.writes.push(call);
                self.projected_deleted.add_permits(1);
                (
                    200,
                    json!({"kind":"Status", "apiVersion":"v1", "status":"Success"}),
                )
            }
            _ => panic!("unexpected cleanup request {method} {path}: {body}"),
        }
    }
}

#[tokio::test]
async fn unscheduled_pod_cleanup_retains_current_authority_at_every_response() {
    /* UCA cause/effect graph: the actual converge path observes Unschedulable
     * at a zero configured bound -> exact Pod DELETE/projected cleanup/original
     * Create guard release/UID absence. Cross every HTTP reply from that capacity
     * observation onward with stable/renew/revoke/reassign/expire/cancel, plus
     * the finalizer's 409 retry. UCA1 stable/renew returns CapacityUnavailable
     * ONLY after exact Pod absence; UCA2 authority loss returns another error and
     * emits no later request/write/guard release; UCA3 cancellation keeps the
     * already accepted prefix and emits no later request. All writes use the
     * original UID/resourceVersion and projected attempt; the PVC is untouched.
     * with_attempt must retain the original admission checkpoint. This tests the
     * real adapter HTTP wiring, not physical process/cluster qualification.
     */
    for conflict in [false, true] {
        let mut boundaries = 0;
        for cut in 0..=16 {
            if cut == 1 || (cut > 0 && cut > boundaries) {
                continue;
            }
            for change in if cut == 0 {
                &['s'][..]
            } else {
                &['s', 'r', 'v', 'g', 'e', 'c'][..]
            } {
                let fence = pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1)
                    .unwrap();
                let authority = Arc::new(EffectAuthority::new(&fence));
                let (mut runtime, state, reached, release, server) = fixture(cut, conflict).await;
                runtime.unschedulable_wait = Duration::ZERO;
                {
                    let mut state = state.lock().unwrap();
                    state.projected.clear();
                    let pod = state.pod.as_mut().unwrap();
                    fixture_original_participants(pod, &runtime.namespace, std::iter::empty());
                    pod.metadata.finalizers =
                        Some(vec![create_ledger::ORIGINAL_GUARD_FINALIZER.into()]);
                    pod.status = Some(serde_json::from_value(json!({
                        "phase":"Pending", "conditions":[{
                            "type":"PodScheduled", "status":"False", "reason":"Unschedulable", "message":"no capacity"
                        }]
                    })).unwrap());
                }
                let current = authority.clone();
                let task = tokio::spawn(async move {
                    let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
                    let attempt = ContainerCreateAttempt::new("initial-attempt");
                    let recovered = ContainerCreateAttempt::new("attempt");
                    let intent = ContainerRealizationIntent::Create;
                    let context = ContainerRealizationContext::new(
                        "scope",
                        &fingerprint,
                        Some(&fence),
                        &intent,
                        &attempt,
                    )
                    .with_effect(ContainerEffect::Fenced {
                        fence: &fence,
                        authority: current.as_ref(),
                    });
                    creation::converge(
                        &runtime,
                        "cleanup",
                        "awaken-cleanup",
                        "pod-uid",
                        &context.with_attempt(&recovered),
                        &ephemeral_plan(),
                        &fingerprint,
                    )
                    .await
                });
                let mut task = task;
                let mut writes_at_cut = vec![];
                if cut > 0 {
                    tokio::time::timeout(Duration::from_secs(5), reached.notified())
                        .await
                        .expect("UCA selected reply must be reached");
                    writes_at_cut = state.lock().unwrap().writes.clone();
                    if *change == 'c' {
                        task.abort();
                    } else {
                        let mut current = authority.current.lock().unwrap();
                        match change {
                            's' => {}
                            'r' => current.as_mut().unwrap().0.renew(),
                            'v' => *current = None,
                            'g' => current.as_mut().unwrap().0.reassign(),
                            'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                            _ => unreachable!(),
                        }
                    }
                    release.add_permits(1);
                }
                let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .expect("UCA cleanup bounded");
                {
                    let state = state.lock().unwrap();
                    if cut == 0 || matches!(change, 's' | 'r') {
                        assert!(
                            matches!(result.unwrap(), Err(RuntimeError::CapacityUnavailable(_))),
                            "UCA1"
                        );
                        assert!(
                            state.pod.is_none(),
                            "UCA1 exact absence before capacity return"
                        );
                        assert!(state.projected.is_empty());
                        assert_eq!(state.writes, ["DELETE pods", "PUT pods"]);
                        boundaries = state.calls.len();
                        assert_eq!(boundaries, if conflict { 9 } else { 7 });
                    } else {
                        if *change == 'c' {
                            assert!(result.unwrap_err().is_cancelled(), "UCA3");
                        } else {
                            let error = result.unwrap().unwrap_err();
                            assert!(
                                !matches!(error, RuntimeError::CapacityUnavailable(_)),
                                "UCA2 not a safe capacity return"
                            );
                        }
                        assert_eq!(
                            state.calls.len(),
                            cut,
                            "UCA2/UCA3 no next request {cut}/{change}"
                        );
                        assert_eq!(state.writes, writes_at_cut, "UCA2/UCA3 preserve prefix");
                    }
                    assert!(state.claim.is_some(), "UCA no PVC disposal authority");
                }
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
            }
        }
    }
}

#[tokio::test]
async fn unscheduled_pod_absence_and_replacement_require_current_response_authority() {
    // UA cause/effect table: C1 initial exact lookup observes 404/a replacement
    // UID; C2 authority stable/renew/revoke/reassign/expire/cancel at its reply.
    // UA1 stable/renew -> original UID is absent, no writes; UA2 lost -> error,
    // no success or later request; UA3 cancel -> preserve the accepted read.
    // The foreign Pod, its guard, projected resources and PVC remain untouched.
    for absent in [false, true] {
        for change in ['s', 'r', 'v', 'g', 'e', 'c'] {
            let fence =
                pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
            let authority = Arc::new(EffectAuthority::new(&fence));
            let (runtime, state, reached, release, server) = fixture(1, false).await;
            {
                let mut state = state.lock().unwrap();
                if absent {
                    state.pod = None;
                } else {
                    state.pod.as_mut().unwrap().metadata.uid = Some("foreign-uid".into());
                }
            }
            let current = authority.clone();
            let mut task = tokio::spawn(async move {
                let admit = || {
                    pc::with_current_sandbox_effect(&fence, current.as_ref(), |_| Ok(()))
                        .map_err(backend)
                };
                creation::dispose_unscheduled_pod(
                    &runtime,
                    "awaken-cleanup",
                    "pod-uid",
                    "attempt",
                    &admit,
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(5), reached.notified())
                .await
                .unwrap();
            if change == 'c' {
                task.abort();
            } else {
                let mut current = authority.current.lock().unwrap();
                match change {
                    's' => {}
                    'r' => current.as_mut().unwrap().0.renew(),
                    'v' => *current = None,
                    'g' => current.as_mut().unwrap().0.reassign(),
                    'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                    _ => unreachable!(),
                }
            }
            release.add_permits(1);
            let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .unwrap();
            if change == 'c' {
                assert!(result.unwrap_err().is_cancelled(), "UA3");
            } else {
                assert_eq!(
                    result.unwrap().is_ok(),
                    matches!(change, 's' | 'r'),
                    "UA1/UA2"
                );
            }
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            let state = state.lock().unwrap();
            assert_eq!(state.calls.len(), 1);
            assert!(state.writes.is_empty());
            assert_eq!(state.projected.len(), 4);
            assert!(state.claim.is_some());
            assert_eq!(
                state
                    .pod
                    .as_ref()
                    .and_then(|pod| pod.metadata.uid.as_deref()),
                if absent { None } else { Some("foreign-uid") }
            );
        }
    }
}

async fn request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
    let mut bytes = Vec::new();
    let (header, start) = loop {
        let mut buf = [0; 4096];
        let read = stream.read(&mut buf).await.unwrap();
        // A cancelled kube request may close after TCP connect but before the
        // API receives a complete HTTP request. It has no cleanup effect.
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..read]);
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break (String::from_utf8(bytes[..end].to_vec()).unwrap(), end + 4);
        }
    };
    let length: usize = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.trim().parse().unwrap());
    while bytes.len() < start + length {
        let mut buf = [0; 4096];
        let read = stream.read(&mut buf).await.unwrap();
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..read]);
    }
    let mut parts = header.lines().next().unwrap().split_whitespace();
    let method = parts.next().unwrap().to_owned();
    let path = parts.next().unwrap().split('?').next().unwrap().to_owned();
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[start..start + length]).unwrap()
    };
    Some((method, path, body))
}

fn fixture_original_participants<'a>(
    pod: &mut Pod,
    namespace: &str,
    names: impl Iterator<Item = &'a str>,
) {
    // Fixture only: these names/UIDs are its original accepted API results, not
    // discovery from a current LIST. Production must use creation's same plan.
    use crate::kubernetes_handle::{
        KubernetesProjectedKind as Kind, KubernetesProjectedObject, KubernetesProjectedParticipants,
    };
    let objects = names
        .map(|name| KubernetesProjectedObject {
            kind: if name.starts_with("configmaps") {
                Kind::ConfigMap
            } else {
                Kind::Secret
            },
            name: name.into(),
            uid: format!("{name}-uid"),
            owner_references_fingerprint: participants::owner_fingerprint(&ObjectMeta {
                owner_references: Some(vec![OwnerReference {
                    api_version: "v1".into(),
                    kind: "Pod".into(),
                    name: "awaken-cleanup".into(),
                    uid: "pod-uid".into(),
                    controller: Some(true),
                    block_owner_deletion: None,
                }]),
                ..Default::default()
            })
            .unwrap(),
        })
        .collect();
    let set = KubernetesProjectedParticipants::new(
        namespace.into(),
        pod.metadata.uid.clone().unwrap(),
        objects,
        Some("attempt".into()),
        None,
    )
    .unwrap();
    let declaration = set
        .objects
        .iter()
        .map(|object| (object.kind, object.name.clone()))
        .collect::<Vec<_>>();
    let annotations = pod
        .metadata
        .annotations
        .get_or_insert_with(Default::default);
    annotations.insert(
        participants::DECLARATION.into(),
        serde_json::to_string(&declaration).unwrap(),
    );
    annotations.insert(
        "awaken.dev/projected-object-results".into(),
        serde_json::to_string(&set).unwrap(),
    );
    annotations.insert(SANDBOX_ADOPTION_ANNOTATION.into(), "frozen-spec".into());
    annotations.insert(SANDBOX_REALIZATION_ANNOTATION.into(), "frozen-spec".into());
}

async fn fixture(
    cut: usize,
    conflict: bool,
) -> (
    K8sRuntime,
    Arc<Mutex<ApiState>>,
    Arc<Notify>,
    Arc<Semaphore>,
    tokio::task::JoinHandle<()>,
) {
    fixture_with_cut(cut, conflict, false).await
}

async fn fixture_with_cut(
    cut: usize,
    conflict: bool,
    pause_before_effect: bool,
) -> (
    K8sRuntime,
    Arc<Mutex<ApiState>>,
    Arc<Notify>,
    Arc<Semaphore>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut runtime = K8sRuntime::for_test(addr);
    runtime.clients.control =
        Client::try_from(kube::Config::new(format!("http://{addr}").parse().unwrap())).unwrap();
    let volume = crate::K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    };
    let mut claim = continuation::build_claim("cleanup", &volume).unwrap();
    claim.metadata.uid = Some("claim-uid".into());
    claim.metadata.resource_version = Some("1".into());
    let mut pod: Pod = serde_json::from_value(json!({"metadata": {
        "name":"awaken-cleanup", "uid":"pod-uid", "resourceVersion":"1", "annotations": {SANDBOX_ATTEMPT_ANNOTATION:"attempt"}
    }, "spec":{"containers":[], "volumes":[{"name":pod_projection::CONTINUATION_VOLUME, "persistentVolumeClaim":{"claimName":"awc-cleanup"}}]}})).unwrap();
    continuation::bind_claim_uid(&mut pod, "claim-uid");
    fixture_original_participants(
        &mut pod,
        &runtime.namespace,
        ["configmaps-0", "configmaps-1", "secrets-0", "secrets-1"].into_iter(),
    );
    runtime.continuation_volume = Some(volume);
    let state = Arc::new(Mutex::new(ApiState {
        server_addr: addr,
        pod: Some(pod),
        claim: Some(claim),
        pod_posted: Arc::new(Semaphore::new(0)),
        calls: vec![],
        writes: vec![],
        projected: ["configmaps-0", "configmaps-1", "secrets-0", "secrets-1"]
            .map(str::to_owned)
            .into_iter()
            .collect(),
        projected_behavior: Default::default(),
        projected_objects: Default::default(),
        accept_projected_creates: false,
        created_tickets: Vec::new(),
        cut_after_call: None,
        projected_lists: 0,
        projected_deleted: Arc::new(Semaphore::new(0)),
        release_conflict: conflict,
        read_failure: None,
    }));
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let (serving, notify, resume) = (state.clone(), reached.clone(), release.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let Some((method, path, body)) = request(&mut stream).await else {
                continue;
            };
            if pause_before_effect && serving.lock().unwrap().calls.len() + 1 == cut {
                notify.notify_one();
                let _permit = resume.acquire().await.unwrap();
            }
            let (status, response, count, selected_cut) = {
                let mut state = serving.lock().unwrap();
                let (status, response) = state.response(&method, &path, body);
                let selected_cut =
                    state.cut_after_call.as_deref() == Some(&format!("{method} {path}"));
                if selected_cut {
                    state.cut_after_call = None;
                }
                (
                    status,
                    response.to_string(),
                    state.calls.len(),
                    selected_cut,
                )
            };
            if !pause_before_effect && (count == cut || selected_cut) {
                notify.notify_one();
                let _permit = resume.acquire().await.unwrap();
            }
            let reply = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            // Cancellation may close the response socket after API acceptance.
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    (runtime, state, reached, release, server)
}

async fn fixture_pending_claim(
    runtime: &K8sRuntime,
    state: &Mutex<ApiState>,
    claim: bool,
) -> Option<continuation::PendingClaimDeletion> {
    if !claim {
        return None;
    }
    state
        .lock()
        .unwrap()
        .claim
        .as_mut()
        .unwrap()
        .metadata
        .deletion_timestamp = Some(serde_json::from_value(json!("2026-09-07T00:00:00Z")).unwrap());
    continuation::absent_pod_claim_deletion(
        &runtime.persistent_volume_claims(),
        "awaken-cleanup",
        "claim-uid",
    )
    .await
    .unwrap()
}

async fn wait_fixture_incarnation(
    runtime: &K8sRuntime,
    pending: Option<&continuation::PendingClaimDeletion>,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    match pending {
        Some(pending) => {
            continuation::await_claim_deletion(&runtime.persistent_volume_claims(), pending, admit)
                .await
        }
        None => {
            runtime
                .await_exact_pod_deleted("awaken-cleanup", "pod-uid", admit)
                .await
        }
    }
}

#[tokio::test]
async fn exact_deletion_wait_requires_absence_or_a_valid_replacement_uid() {
    /* EW cause/effect graph: the original typed Pod/PVC wait -> actual GET ->
     * 404 / present UID / API error -> success / uncertainty. UID validity and
     * equality are independent causes: missing/empty/whitespace-only is never
     * a replacement. Decision rules EW1=404=>Ok; EW2=valid different UID=>Ok
     * without a write; EW3=missing|empty|blank=>Err; EW4=403|500=>Err.
     * Cross every rule with BOTH callers using the existing HTTP fixture;
     * the PVC's preceding read only establishes its already-terminating exact
     * identity. Every wait sends exactly one GET and zero writes. Original
     * same-UID continuation and authority cuts are covered by EW5/EW7 below.
     * EW9 is owned by the existing test-waits scanner/CLI: removing the duplicate
     * continuation-local loop reduces its held count 1->0. Tighten must lower
     * only that recorded entry; no old loop or budget increase preserves growth. */
    for claim in [false, true] {
        for row in [
            "absent",
            "replacement",
            "missing",
            "empty",
            "blank",
            "403",
            "500",
        ] {
            let (runtime, state, _, _, server) = fixture(0, false).await;
            let pending = fixture_pending_claim(&runtime, &state, claim).await;
            let before = state.lock().unwrap().calls.len();
            {
                let mut state = state.lock().unwrap();
                if row == "absent" {
                    if claim {
                        state.claim = None;
                    } else {
                        state.pod = None;
                    }
                } else if let Ok(code) = row.parse::<u16>() {
                    state.read_failure = Some(code);
                } else {
                    let metadata = if claim {
                        &mut state.claim.as_mut().unwrap().metadata
                    } else {
                        &mut state.pod.as_mut().unwrap().metadata
                    };
                    metadata.uid = match row {
                        "replacement" => Some("replacement-uid".into()),
                        "missing" => None,
                        "empty" => Some(String::new()),
                        "blank" => Some(" \t\n".into()),
                        _ => unreachable!(),
                    };
                }
            }
            let result = wait_fixture_incarnation(&runtime, pending.as_ref(), &|| Ok(())).await;
            assert_eq!(
                result.is_ok(),
                matches!(row, "absent" | "replacement"),
                "EW1-4 {claim}/{row}"
            );
            if matches!(row, "missing" | "empty" | "blank") {
                assert!(
                    result.unwrap_err().to_string().contains("no valid UID"),
                    "EW3"
                );
            }
            {
                let state = state.lock().unwrap();
                assert_eq!(state.calls.len(), before + 1, "one actual wait GET");
                assert!(state.writes.is_empty(), "no replacement mutation");
                assert_eq!(state.projected.len(), 4, "no participant policy change");
            }
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    }
}

#[tokio::test]
async fn exact_deletion_wait_keeps_polling_the_same_uid_and_preserves_each_deadline() {
    // EW5: same valid UID at the first real GET reply, then 404 -> no early Ok,
    // next GET observes absence, no writes. EW6: hold that same-UID reply while
    // advancing the existing monotonic clock by Pod60s/PVC30s -> each original
    // caller returns its own timeout, not success or a shared replacement limit.
    // The server's response cut separates API observation from delivery. No
    // sleep is used as an absence witness, and no physical controller is tested.
    for claim in [false, true] {
        for expires in [false, true] {
            let (runtime, state, reached, release, server) =
                fixture(if claim { 2 } else { 1 }, false).await;
            let pending = fixture_pending_claim(&runtime, &state, claim).await;
            let mut task = tokio::spawn(async move {
                wait_fixture_incarnation(&runtime, pending.as_ref(), &|| Ok(())).await
            });
            tokio::time::timeout(Duration::from_secs(5), reached.notified())
                .await
                .unwrap();
            assert!(!task.is_finished(), "EW5 same UID cannot complete");
            let timeout = if claim { 30 } else { 60 };
            if expires {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(timeout)).await;
                tokio::time::resume();
            } else {
                let mut state = state.lock().unwrap();
                if claim {
                    state.claim = None;
                } else {
                    state.pod = None;
                }
            }
            release.add_permits(1);
            let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .unwrap()
                .unwrap();
            if expires {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains(&format!("within {timeout}s")),
                    "EW6"
                );
            } else {
                result.unwrap();
            }
            {
                let state = state.lock().unwrap();
                let first_wait = if claim { 2 } else { 1 };
                assert_eq!(
                    state.calls.len(),
                    first_wait + usize::from(!expires),
                    "EW5/6 actual GETs"
                );
                assert!(state.writes.is_empty());
            }
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    }
}

#[tokio::test]
async fn exact_deletion_wait_rechecks_current_authority_before_accepting_absence() {
    /* EW7 decision table: C1 Pod/PVC; C2 reply=404/valid replacement UID;
     * C3 authority stable/renewed/revoked/reassigned/expired or cancellation.
     * Stable/renewed+C2=>Ok; revoked/reassigned/expired=>Err; cancelled=>cancel.
     * Each row preserves zero writes and sends no request after the held reply.
     * This crosses the actual two typed callers and both success exits, so a
     * generic helper's unit result cannot hide a missing post-await admission. */
    for claim in [false, true] {
        for absent in [false, true] {
            for change in ['s', 'r', 'v', 'g', 'e', 'c'] {
                let fence =
                    pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1)
                        .unwrap();
                let authority = Arc::new(EffectAuthority::new(&fence));
                let (runtime, state, reached, release, server) =
                    fixture(if claim { 2 } else { 1 }, false).await;
                let pending = fixture_pending_claim(&runtime, &state, claim).await;
                {
                    let mut state = state.lock().unwrap();
                    if absent {
                        if claim {
                            state.claim = None;
                        } else {
                            state.pod = None;
                        }
                    } else if claim {
                        state.claim.as_mut().unwrap().metadata.uid = Some("replacement-uid".into());
                    } else {
                        state.pod.as_mut().unwrap().metadata.uid = Some("replacement-uid".into());
                    }
                }
                let current = authority.clone();
                let mut task = tokio::spawn(async move {
                    let admit = || {
                        pc::with_current_sandbox_effect(&fence, current.as_ref(), |_| Ok(()))
                            .map_err(backend)
                    };
                    wait_fixture_incarnation(&runtime, pending.as_ref(), &admit).await
                });
                tokio::time::timeout(Duration::from_secs(5), reached.notified())
                    .await
                    .unwrap();
                if change == 'c' {
                    task.abort();
                } else {
                    let mut current = authority.current.lock().unwrap();
                    match change {
                        's' => {}
                        'r' => current.as_mut().unwrap().0.renew(),
                        'v' => *current = None,
                        'g' => current.as_mut().unwrap().0.reassign(),
                        'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                        _ => unreachable!(),
                    }
                }
                release.add_permits(1);
                let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .unwrap();
                if change == 'c' {
                    assert!(result.unwrap_err().is_cancelled());
                } else {
                    assert_eq!(
                        result.unwrap().is_ok(),
                        matches!(change, 's' | 'r'),
                        "EW7 {claim}/{absent}/{change}"
                    );
                }
                assert_eq!(state.lock().unwrap().calls.len(), if claim { 2 } else { 1 });
                assert!(state.lock().unwrap().writes.is_empty());
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
            }
        }
    }
}

#[tokio::test]
async fn exact_deletion_wait_rechecks_current_authority_around_polling_sleep() {
    // EW8 crosses both typed callers with loss at initial admission (1), before
    // the polling sleep (3), and after it (4). Initial loss sends zero wait GETs;
    // later loss preserves the one same-UID GET but sends no second GET/write.
    // The original EffectAuthority is actually revoked at the selected callback,
    // so these are current-authority effects rather than injected API outcomes.
    use std::sync::atomic::{AtomicUsize, Ordering};

    for claim in [false, true] {
        for deny_at in [1, 3, 4] {
            let fence =
                pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap();
            let authority = EffectAuthority::new(&fence);
            let (runtime, state, _, _, server) = fixture(0, false).await;
            let pending = fixture_pending_claim(&runtime, &state, claim).await;
            let before = state.lock().unwrap().calls.len();
            let admissions = AtomicUsize::new(0);
            let admit = || {
                if admissions.fetch_add(1, Ordering::SeqCst) + 1 == deny_at {
                    *authority.current.lock().unwrap() = None;
                }
                pc::with_current_sandbox_effect(&fence, &authority, |_| Ok(())).map_err(backend)
            };
            assert!(
                wait_fixture_incarnation(&runtime, pending.as_ref(), &admit)
                    .await
                    .is_err(),
                "EW8"
            );
            assert_eq!(admissions.load(Ordering::SeqCst), deny_at);
            assert_eq!(
                state.lock().unwrap().calls.len(),
                before + usize::from(deny_at > 1)
            );
            assert!(state.lock().unwrap().writes.is_empty());
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    }
}

#[tokio::test]
async fn cancelled_connections_before_http_acceptance_leave_cleanup_available() {
    // Cause/effect table: C1 TCP closes before headers, during headers, or
    // during a declared body; C2 a later exact cleanup has live authority.
    // R1=C1 => no API call or mutation is accepted; R2=C1+C2 => the same
    // fixture serves the complete Pod/PVC/projected-resource cleanup. The
    // cancelled transport is not a Kubernetes response or an effect receipt.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    let address = state.lock().unwrap().server_addr;
    for partial in [
        &b""[..],
        &b"GET /api HTTP/1.1\r\n"[..],
        &b"POST /api HTTP/1.1\r\nContent-Length: 2\r\n\r\n{"[..],
    ] {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(partial).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    let authorization = crate::tests::disposal_authorization(
        &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
    );
    let authority = EffectAuthority::new(authorization.effect_fence());
    let handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap()).unwrap();
    runtime
        .remove_exact_incarnation(
            "awaken-cleanup",
            handle.as_ref(),
            &authorization,
            &authority,
        )
        .await
        .unwrap();
    {
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls.len(),
            27,
            "R1/R2 only complete HTTP requests count"
        );
        assert_eq!(state.writes.len(), 8, "R2 exact cleanup effects");
        assert!(state.pod.is_none() && state.claim.is_none() && state.projected.is_empty());
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn broker_original_guard_releases_after_exact_terminal_cleanup() {
    // Causes: C1 a brokered Pod has the result guard and a retained exact
    // Pod/PVC handle; C2 removal is unreceipted or carries current terminal
    // authority; C3 UID stays exact through Pod delete and two finalizer CASes.
    // Effects: E1 unreceipted removal makes no write and keeps the guard;
    // E2 C1+C2(current)+C3 deletes the claim/projected participants, starts
    // exact Pod deletion, removes the continuation and result guards, and
    // observes original UID absence. Rules BG1=C1+C2(unreceipted)->E1,
    // BG2=C1+C2(current)+C3->E2. A disappearing/foreign UID remains covered
    // by the existing KCE/KCA failure rows.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    state
        .lock()
        .unwrap()
        .pod
        .as_mut()
        .unwrap()
        .metadata
        .finalizers = Some(vec![create_ledger::ORIGINAL_GUARD_FINALIZER.into()]);
    let authorization = crate::tests::disposal_authorization(
        &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
    );
    let authority = EffectAuthority::new(authorization.effect_fence());
    let handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap()).unwrap();
    assert!(runtime.remove("awaken-cleanup").await.is_err(), "BG1");
    assert!(state.lock().unwrap().writes.is_empty(), "BG1 no mutation");
    runtime
        .remove_exact_incarnation(
            "awaken-cleanup",
            handle.as_ref(),
            &authorization,
            &authority,
        )
        .await
        .unwrap();
    {
        let state = state.lock().unwrap();
        assert!(
            state.pod.is_none() && state.claim.is_none() && state.projected.is_empty(),
            "BG2"
        );
        assert!(
            state
                .writes
                .iter()
                .filter(|write| write.as_str() == "PUT pods")
                .count()
                >= 3,
            "BG2 both finalizers were CASed"
        );
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn delayed_pod_create_cannot_be_closed_by_a_replacement_runtime() {
    // Cause/effect table for the real kube HTTP edge. C1 the original runtime
    // sends Pod POST under its exact attempt gate; C2 the API pauses before
    // accepting POST or after accepting it but before returning the response;
    // C3 the Worker process dies without persisting a handle; C4 a replacement
    // runtime observes the same scope/fence. E1 C1+C2+C4 cannot certify
    // absence; E2 C1+C2+C3 later permits the old POST to materialize despite
    // the replacement's earlier observation; E3 no cleanup write is forged.
    // The existing durable-handle and rich-topology tests own their separate
    // completion rules. The child executes the production K8sRuntime request
    // in another OS process against this test's controlled kube HTTP edge.
    // This establishes the fail-closed cut, not a durable closure witness.
    if let Ok(addr) = std::env::var("AWAKEN_K8S_CREATE_CHILD_ADDR") {
        let addr: std::net::SocketAddr = addr.parse().unwrap();
        let mut runtime = K8sRuntime::for_test(addr);
        runtime.clients.control =
            Client::try_from(kube::Config::new(format!("http://{addr}").parse().unwrap())).unwrap();
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(json!("delayed-create-fingerprint")).unwrap();
        let fence = pc::SandboxEffectFence::new(
            "create-operation",
            "worker-owner",
            "worker-boot",
            7,
            u64::MAX,
        )
        .unwrap();
        let intent = ContainerRealizationIntent::Create;
        let attempt = crate::fresh_create_attempt();
        let context = ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            Some(&fence),
            &intent,
            &attempt,
        );
        runtime
            .prepare_for_effect(&context, &ephemeral_plan(), &fingerprint)
            .await
            .expect("child API request");
        panic!("the child unexpectedly completed before its forced process death");
    }
    for pause_before_effect in [true, false] {
        let (runtime, state, reached, release, server) =
            fixture_with_cut(2, false, pause_before_effect).await;
        let (posted, server_addr) = {
            let mut state = state.lock().unwrap();
            state.pod = None;
            state.claim = None;
            state.projected.clear();
            (state.pod_posted.clone(), state.server_addr)
        };
        let mut replacement = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        replacement.clients.control = runtime.clients.control.clone();
        drop(runtime);
        let fence = pc::SandboxEffectFence::new(
            "create-operation",
            "worker-owner",
            "worker-boot",
            7,
            u64::MAX,
        )
        .unwrap();
        let mut creating = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "k8s::cleanup_tests::delayed_pod_create_cannot_be_closed_by_a_replacement_runtime",
                "--nocapture",
            ])
            .env("AWAKEN_K8S_CREATE_CHILD_ADDR", server_addr.to_string())
            .spawn()
            .expect("C3 start one disposable Worker process");
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("C1 actual Pod POST reached the selected API cut");
        assert_eq!(
            state.lock().unwrap().pod.is_some(),
            !pause_before_effect,
            "C2 API acceptance and response delivery are separate cuts"
        );
        assert_eq!(
            replacement
                .missing_handle_terminal_evidence("scope", &ephemeral_plan(), Some(&fence))
                .await
                .unwrap(),
            crate::MissingHandleTerminalEvidence::Unknown,
            "E1 replacement has no observed-attempt closure witness"
        );
        creating.kill().expect("C3 force-stop Worker process");
        assert!(
            !creating.wait().unwrap().success(),
            "C3 Worker process died"
        );
        release.add_permits(1);
        let permit = tokio::time::timeout(Duration::from_secs(5), posted.acquire())
            .await
            .expect("E2 delayed API POST eventually applies")
            .unwrap();
        drop(permit);
        {
            let state = state.lock().unwrap();
            assert!(state.pod.is_some(), "E2 late Pod exists after Worker loss");
            assert_eq!(state.writes, vec!["POST pods"], "E3 no cleanup was forged");
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[derive(Clone)]
struct CheckpointEvidence {
    source: pc::SandboxEffectFence,
    fingerprint: pc::SandboxRealizationFingerprint,
    handle: crate::ContainerContinuationHandle,
}

impl CheckpointEvidence {
    fn initialize(state: &Mutex<ApiState>) -> Self {
        let source =
            pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, u64::MAX - 1).unwrap();
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(json!("checkpoint-fixture-realization")).unwrap();
        let mut state = state.lock().unwrap();
        let pod = state.pod.as_mut().unwrap();
        let annotations = pod.metadata.annotations.as_mut().unwrap();
        annotations.insert(SANDBOX_ADOPTION_ANNOTATION.into(), fingerprint.to_string());
        annotations.insert(
            SANDBOX_REALIZATION_ANNOTATION.into(),
            fingerprint.to_string(),
        );
        stamp_effect_fence(
            pod,
            &pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        pod.status = Some(serde_json::from_value(json!({"phase":"Running", "containerStatuses":[{
            "name":"agent", "ready":true, "restartCount":0, "image":"fixture", "imageID":"fixture", "state":{"running":{}}
        }]})).unwrap());
        let handle = continuation::handle_extra(pod).unwrap().unwrap();
        Self {
            source,
            fingerprint,
            handle,
        }
    }

    async fn apply(
        &self,
        runtime: &K8sRuntime,
        active: &pc::SandboxEffectFence,
        authority: &EffectAuthority,
        step: usize,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
        let pending =
            pc::SandboxCheckpointUpload::pending("suspend".into(), "request", "digest", 4);
        let reference = crate::checkpoint::tests::receipt();
        let mutation = match step {
            0 => crate::ContainerCheckpointMutation::Acquire,
            1 => crate::ContainerCheckpointMutation::Begin(&pending),
            2 => crate::ContainerCheckpointMutation::Complete(&reference),
            _ => unreachable!(),
        };
        runtime
            .checkpoint_upload(
                ContainerObservationExpectation {
                    purpose: ContainerObservationPurpose::Recovery,
                    container_id: "awaken-cleanup",
                    adoption_fingerprint: Some(&self.fingerprint),
                    realization_fingerprint: Some(&self.fingerprint),
                    runtime_handle: Some(&self.handle),
                    effect_fence: Some(active),
                    retained_attempt: None,
                },
                crate::ContainerCheckpointAccess {
                    source_effect_fence: &self.source,
                    effect_fence: active,
                    request_fingerprint: "request",
                    authority,
                },
                mutation,
            )
            .await
    }
}

#[tokio::test]
async fn checkpoint_http_replay_and_terminal_takeover_share_one_pod_record() {
    // KH1 acquire/begin/complete -> three UID+RV writes to the SAME Pod;
    // KH2 fresh runtime replays each exact mutation -> reads only, same receipt;
    // KH3 terminal takeover at SAME epoch -> one fence+participant CAS;
    // KH4 old Suspend (even with a locally live grant) -> no write or success.
    // Neither path acquires a cleanup finalizer, writes PVC, creates resources,
    // uploads bytes, deletes objects, or claims Session completion.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    let evidence = CheckpointEvidence::initialize(&state);
    let authority = EffectAuthority::new(&evidence.source);
    for step in 0..3 {
        evidence
            .apply(&runtime, &evidence.source, &authority, step)
            .await
            .unwrap();
    }
    assert_eq!(state.lock().unwrap().writes, vec!["PUT pods"; 3]);
    let mut restarted = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    restarted.clients.control = runtime.clients.control.clone();
    drop(runtime);
    let expected = crate::checkpoint::tests::receipt();
    for step in 0..3 {
        let replay = evidence
            .apply(&restarted, &evidence.source, &authority, step)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.reference(), Some(&expected), "KH2");
    }
    assert_eq!(
        state.lock().unwrap().writes.len(),
        3,
        "KH2 no duplicate writes"
    );
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "boot", 2, u64::MAX - 1).unwrap();
    let terminal_authority = EffectAuthority::new(&terminal);
    for step in 0..3 {
        evidence
            .apply(&restarted, &terminal, &terminal_authority, step)
            .await
            .unwrap();
    }
    assert_eq!(state.lock().unwrap().writes.len(), 4, "KH3");
    for step in 0..3 {
        assert!(
            evidence
                .apply(&restarted, &evidence.source, &authority, step)
                .await
                .is_err(),
            "KH4"
        );
    }
    {
        let state = state.lock().unwrap();
        assert_eq!(state.writes.len(), 4);
        assert!(state.claim.is_some() && state.projected.len() == 4);
        assert!(!continuation::pod_cleanup_gate_held(
            state.pod.as_ref().unwrap()
        ));
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn checkpoint_current_authority_is_reentered_after_each_http_response() {
    // KA graph: acquire/begin/complete -> GET Pod -> GET PVC -> Pod CAS;
    // C1 pause after each actual response; C2 renew/revoke/replace/expire/cancel.
    // KA1 renewal preserves the admitted effect and completes; KA2 all other
    // rows return no success and send no later request. A CAS already accepted
    // before the pause remains durable. KA3 a new successor grant replays the
    // original input over that prefix, never allocates a second participant.
    for step in 0..3 {
        for cut in 1..=3 {
            for change in ['r', 'v', 'g', 'e', 'c'] {
                let (runtime, state, reached, release, server) =
                    fixture(step * 3 + cut, false).await;
                let evidence = CheckpointEvidence::initialize(&state);
                let authority = Arc::new(EffectAuthority::new(&evidence.source));
                for seed in 0..step {
                    evidence
                        .apply(&runtime, &evidence.source, &authority, seed)
                        .await
                        .unwrap();
                }
                let runtime = Arc::new(runtime);
                let (executing, proof, inputs) =
                    (runtime.clone(), authority.clone(), evidence.clone());
                let mut task = tokio::spawn(async move {
                    inputs.apply(&executing, &inputs.source, &proof, step).await
                });
                tokio::time::timeout(Duration::from_secs(5), reached.notified())
                    .await
                    .expect("KA selected response reached");
                let (calls, writes) = {
                    let state = state.lock().unwrap();
                    (state.calls.len(), state.writes.clone())
                };
                if change == 'c' {
                    task.abort();
                } else {
                    let mut current = authority.current.lock().unwrap();
                    match change {
                        'r' => current.as_mut().unwrap().0.renew(),
                        'v' => *current = None,
                        'g' => current.as_mut().unwrap().0.reassign(),
                        'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                        _ => unreachable!(),
                    }
                }
                release.add_permits(1);
                let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .expect("KA bounded result");
                if change == 'r' {
                    result.unwrap().unwrap();
                } else {
                    if change == 'c' {
                        assert!(result.unwrap_err().is_cancelled());
                    } else {
                        assert!(result.unwrap().is_err());
                    }
                    let state = state.lock().unwrap();
                    assert_eq!(state.calls.len(), calls, "KA2/{step}/{cut}/{change}");
                    assert_eq!(state.writes, writes, "KA2 retains committed prefix");
                }
                let mut successor = evidence.source.clone();
                successor.reassign();
                let fresh = EffectAuthority::new(&successor);
                evidence
                    .apply(&runtime, &successor, &fresh, step)
                    .await
                    .unwrap();
                {
                    let state = state.lock().unwrap();
                    assert!(state.pod.is_some() && state.claim.is_some());
                    assert!(state.writes.iter().all(|write| write == "PUT pods"));
                }
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
            }
        }
    }
}

#[tokio::test]
async fn checkpoint_cas_cannot_overwrite_a_concurrent_pod_revision_or_incarnation() {
    // KC1 read exact Pod then concurrent metadata change/replacement -> stale
    // CAS rejects; KC2 no participant is published over either newer object.
    // This uses the real kube request serializer and HTTP 409, not a pure mock
    // result. The shared fixture checks BOTH UID and resourceVersion.
    for replaced in [false, true] {
        let (runtime, state, reached, release, server) = fixture(1, false).await;
        let evidence = CheckpointEvidence::initialize(&state);
        let task = tokio::spawn(async move {
            evidence
                .apply(
                    &runtime,
                    &evidence.source,
                    &EffectAuthority::new(&evidence.source),
                    0,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        let expected = {
            let mut state = state.lock().unwrap();
            let pod = state.pod.as_mut().unwrap();
            if replaced {
                pod.metadata.uid = Some("replacement".into());
            } else {
                pod.metadata.resource_version = Some("2".into());
            }
            pod.clone()
        };
        release.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(state.lock().unwrap().pod.as_ref(), Some(&expected), "KC2");
        assert!(state.lock().unwrap().writes.is_empty());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn checkpoint_identity_and_source_state_denials_have_zero_writes() {
    // KI graph: current grant AND complete matching physical evidence AND
    // ungated live source AND valid participant => admission. KH is the positive
    // row; KI1 negates each independent prerequisite -> no write. KI2 denied
    // current authority -> no HTTP at all. KI3 a vanished Pod/PVC is an error,
    // never invented upload completion. Corrupt record bytes are not authority.
    for row in [
        "denied",
        "legacy",
        "pod-missing",
        "pod-uid",
        "pod-version",
        "adoption",
        "realization",
        "fence-missing",
        "fence-newer",
        "pod-deleting",
        "cleanup-gate",
        "claim-missing",
        "claim-uid",
        "claim-deleting",
        "corrupt",
    ] {
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let mut evidence = CheckpointEvidence::initialize(&state);
        let authority = if row == "denied" {
            EffectAuthority::denied()
        } else {
            EffectAuthority::new(&evidence.source)
        };
        {
            let mut state = state.lock().unwrap();
            let deleting = || serde_json::from_value(json!("2026-09-08T00:00:00Z")).unwrap();
            match row {
                "legacy" => {
                    evidence.handle = crate::ContainerContinuationHandle::KubernetesContinuation {
                        claim_uid: "claim-uid".into(),
                    }
                }
                "pod-missing" => state.pod = None,
                "pod-uid" => state.pod.as_mut().unwrap().metadata.uid = Some("foreign".into()),
                "pod-version" => state.pod.as_mut().unwrap().metadata.resource_version = None,
                "adoption" | "realization" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .remove(if row == "adoption" {
                            SANDBOX_ADOPTION_ANNOTATION
                        } else {
                            SANDBOX_REALIZATION_ANNOTATION
                        });
                }
                "fence-missing" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .remove(SANDBOX_EFFECT_ANNOTATION);
                }
                "fence-newer" => {
                    let mut newer = evidence.source.clone();
                    newer.reassign();
                    stamp_effect_fence(state.pod.as_mut().unwrap(), &newer);
                }
                "pod-deleting" => {
                    state.pod.as_mut().unwrap().metadata.deletion_timestamp = Some(deleting())
                }
                "cleanup-gate" => {
                    // Inconsistent gate evidence must also reject; the canonical
                    // continuation validator owns both annotation/finalizer bits.
                    state.pod.as_mut().unwrap().metadata.finalizers =
                        Some(vec![continuation::CONTINUATION_CLEANUP_FINALIZER.into()]);
                }
                "claim-missing" => state.claim = None,
                "claim-uid" => state.claim.as_mut().unwrap().metadata.uid = Some("foreign".into()),
                "claim-deleting" => {
                    state.claim.as_mut().unwrap().metadata.deletion_timestamp = Some(deleting())
                }
                "corrupt" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert("awaken.dev/checkpoint-upload".into(), "{}".into());
                }
                "denied" => {}
                _ => unreachable!(),
            }
        }
        let before = {
            let state = state.lock().unwrap();
            (state.pod.clone(), state.claim.clone())
        };
        assert!(
            evidence
                .apply(&runtime, &evidence.source, &authority, 0)
                .await
                .is_err(),
            "KI/{row}"
        );
        {
            let state = state.lock().unwrap();
            assert_eq!(
                (state.pod.clone(), state.claim.clone()),
                before,
                "KI1/{row}"
            );
            assert!(state.writes.is_empty());
            if row == "denied" {
                assert!(state.calls.is_empty(), "KI2");
            }
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn cleanup_requires_both_current_authority_and_exact_incarnation() {
    // KCE causes: C1 current/denied authority; C2 exact/missing/foreign Pod UID;
    // C3 exact/foreign PVC UID. KCE1 denied -> zero HTTP even for exact identity;
    // KCE2 missing original carrier -> zero reads; a complete original carrier
    // with foreign Pod/PVC identity -> one actual read, no writes. Positive
    // C1+C2+C3 is KCA1. Identity never substitutes for current authority, nor
    // may an authority grant bypass the existing immutable-incarnation gates.
    for row in ["denied", "missing", "foreign-pod", "foreign-claim"] {
        let authorization = crate::tests::disposal_authorization(
            &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let authority = if row == "denied" {
            EffectAuthority::denied()
        } else {
            EffectAuthority::new(authorization.effect_fence())
        };
        let handle = if row == "missing" {
            None
        } else {
            let mut handle =
                continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
                    .unwrap()
                    .unwrap();
            if let crate::ContainerContinuationHandle::KubernetesContinuationV3 {
                pod_uid,
                claim_uid,
                participants,
            } = &mut handle
            {
                if row == "foreign-pod" {
                    *pod_uid = "foreign".into();
                    participants.pod_uid = "foreign".into();
                }
                if row == "foreign-claim" {
                    *claim_uid = Some("foreign".into());
                }
            } else {
                panic!("KCE fixture requires its original complete carrier");
            }
            Some(handle)
        };
        assert!(
            runtime
                .remove_exact_incarnation(
                    "awaken-cleanup",
                    handle.as_ref(),
                    &authorization,
                    &authority
                )
                .await
                .is_err(),
            "KCE {row}"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls.len(),
            usize::from(matches!(row, "foreign-pod" | "foreign-claim"))
        );
        assert!(state.writes.is_empty());
        assert!(!continuation::pod_cleanup_gate_held(
            state.pod.as_ref().unwrap()
        ));
        assert!(
            state
                .claim
                .as_ref()
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_none()
        );
        assert_eq!(state.projected.len(), 4);
    }
}

#[tokio::test]
async fn current_authority_is_required_after_every_kubernetes_cleanup_response() {
    /* KCA cause/effect graph: exact Pod+PVC+projected resources -> GET/CAS/
     * DELETE/wait -> delayed response -> authority stable/renew/revoke/replace/
     * expire or caller cancellation -> continue or preserve the committed prefix.
     * Cross every actual HTTP response with all changes, and cross the finalizer
     * release 409 retry. KCA1 stable/renew reaches exact total absence; KCA2 lost
     * authority returns error and emits NO subsequent request or success; KCA3
     * cancellation emits no subsequent request. A mutation accepted before the
     * cut remains committed; neither cancellation nor revocation may be claimed
     * to undo it. UID+resourceVersion preconditions are asserted at every write.
     * The successful path must include both projected resource kinds, both
     * entries of each deletion loop, Pod/PVC deletion and finalizer acquire/release.
     * KCA4 retry uses a NEW successor grant over the original preparation and
     * persisted handle, never revives the rejected proof or recreates resources.
     * This is adapter wiring proof, not a real cluster/controller qualification.
     */
    for conflict in [false, true] {
        let mut boundaries = 1;
        for cut in 0..40 {
            if cut > boundaries {
                break;
            }
            for change in if cut == 0 {
                &['s'][..]
            } else {
                &['r', 'v', 'g', 'e', 'c'][..]
            } {
                let authorization = crate::tests::disposal_authorization(
                    &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1)
                        .unwrap(),
                );
                let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
                let (runtime, state, reached, release, server) = fixture(cut, conflict).await;
                let runtime = Arc::new(runtime);
                let handle =
                    continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
                        .unwrap();
                let current = authority.clone();
                let (executing, authorized, bound) =
                    (runtime.clone(), authorization.clone(), handle.clone());
                let task = tokio::spawn(async move {
                    executing
                        .remove_exact_incarnation(
                            "awaken-cleanup",
                            bound.as_ref(),
                            &authorized,
                            current.as_ref(),
                        )
                        .await
                });
                let mut task = task;
                let mut writes_at_cut = vec![];
                if cut > 0 {
                    tokio::time::timeout(Duration::from_secs(5), reached.notified())
                        .await
                        .expect("KCA selected HTTP cut must be reached");
                    writes_at_cut = state.lock().unwrap().writes.clone();
                    if *change == 'c' {
                        task.abort();
                    } else {
                        let mut current = authority.current.lock().unwrap();
                        match change {
                            'r' => current.as_mut().unwrap().0.renew(),
                            'v' => *current = None,
                            'g' => current.as_mut().unwrap().0.reassign(),
                            'e' => current.as_mut().unwrap().1 = Duration::ZERO,
                            _ => unreachable!(),
                        }
                    }
                    release.add_permits(1);
                }
                let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .expect("KCA cleanup bounded");
                {
                    let state = state.lock().unwrap();
                    if cut == 0 || *change == 'r' {
                        result.unwrap().unwrap();
                        assert!(state.pod.is_none() && state.claim.is_none());
                        assert!(state.projected.is_empty());
                        assert_eq!(
                            state.writes,
                            [
                                "PUT pods",
                                "DELETE persistentvolumeclaims",
                                "DELETE configmaps",
                                "DELETE configmaps",
                                "DELETE secrets",
                                "DELETE secrets",
                                "DELETE pods",
                                "PUT pods"
                            ]
                        );
                        boundaries = state.calls.len();
                        assert_eq!(boundaries, if conflict { 29 } else { 27 });
                    } else {
                        if *change == 'c' {
                            assert!(result.unwrap_err().is_cancelled());
                        } else {
                            assert!(result.unwrap().is_err(), "KCA2 {cut}/{change}");
                        }
                        assert_eq!(
                            state.calls.len(),
                            cut,
                            "KCA no next HTTP request: {cut}/{change}"
                        );
                        assert_eq!(state.writes, writes_at_cut, "KCA no next mutation");
                    }
                }
                if cut > 0 && *change != 'r' {
                    let mut successor = authorization.effect_fence().clone();
                    successor.reassign();
                    let retry = authorization.preparation().authorize(successor).unwrap();
                    let fresh = EffectAuthority::new(retry.effect_fence());
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        runtime.remove_exact_incarnation(
                            "awaken-cleanup",
                            handle.as_ref(),
                            &retry,
                            &fresh,
                        ),
                    )
                    .await
                    .expect("KCA4 exact retry bounded")
                    .unwrap();
                    let state = state.lock().unwrap();
                    assert!(
                        state.pod.is_none() && state.claim.is_none() && state.projected.is_empty(),
                        "KCA4 terminal outcome {cut}/{change}"
                    );
                }
                server.abort();
                assert!(
                    server.await.unwrap_err().is_cancelled(),
                    "fixture server must not panic"
                );
            }
        }
    }
}
