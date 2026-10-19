// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: API witnesses cover published-handle plan evidence, refusal and the existing UID
// @formal reason: disposal/replay owner. They do not prove Actor retirement or a live cluster.
//! Published legacy handles retain their metadata while deriving only proven empty API membership.

use super::*;
use awaken_provisioning_contract::SandboxAdoption;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::ContainerHandleCodec as _;

#[tokio::test]
async fn published_legacy_empty_plan_preserves_metadata_and_exact_disposal_replay() {
    // Cause/effect graph: published original UID + both matching effective
    // fingerprints + canonical zero-member plan + current authority => the
    // existing terminal prepare/dispose owner. Any missing/conflicting cause
    // => refusal before writes. Empty current LIST/RESULTS is never a cause.
    // R1 empty/control: preserve ALL original metadata, terminal-only carrier,
    // no preparation writes; separately authorized exact Pod/PVC disposal.
    // R2 nonempty plan/declaration, malformed results, UID/configuration drift,
    // V1, revoked authority => no writes/receipt and retain every original.
    // R3 both original objects absent => deterministic cold replay returns
    // absence without re-creating or deleting anything. R4 the exact deleting
    // original keeps source preparation through its retained Create guard;
    // an absent Pod + live claim remains denied. Neither
    // empty membership nor source preparation is a physical cleanup receipt.
    for row in [
        "empty",
        "control",
        "nonempty-plan",
        "declaration",
        "malformed-results",
        "foreign-pod",
        "foreign-claim",
        "configuration",
        "legacy-v1",
        "revoked",
        "already-absent",
        "deleting-pod",
        "absent-pod-live-claim",
    ] {
        let (mut runtime, state, _, _, server) = fixture(0, false).await;
        let control = row != "empty";
        let mut spec = awaken_sandbox_container::test_support::spec("published-legacy-empty");
        spec.mounts.clear();
        spec.env.clear();
        spec.limits.pids = None;
        if control {
            spec.control_services
                .insert(SandboxControlServiceKind::RepositoryGitCredential);
            runtime = runtime.with_sandbox_control_forwarder(
                K8sSandboxControlForwarder::new(
                    format!("registry.test/control@sha256:{}", "a".repeat(64)),
                    "/usr/local/bin/awaken-sandbox",
                    7778,
                )
                .unwrap(),
            );
        }
        if row == "nonempty-plan" {
            spec.mounts.push(pc::MountRequirement {
                mount_id: "original-config".into(),
                source: pc::MountSource::Inline {
                    contents: "original".into(),
                },
                mount_path: "/mnt/config".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::Session,
                required: true,
            });
        }
        let clock: Arc<dyn awaken_time::WallClock> = Arc::new(
            awaken_lease_testkit::FixedClock::at(awaken_time::UtcTime::from_epoch_ms(1_000)),
        );
        let image = "published-legacy-image";
        let plan = awaken_sandbox_container::container_plan_with_allowlist(
            &spec,
            image,
            &awaken_sandbox_container::engine::environment_keepalive_command(),
            None,
            None,
            Some(&clock),
        )
        .unwrap();
        let adoption = awaken_sandbox_container::engine::container_adoption_fingerprint(
            &spec,
            &plan,
            None,
            &runtime.realization_configuration().unwrap(),
        );
        let realization = awaken_sandbox_container::engine::container_realization_fingerprint(
            &spec, &adoption, image,
        );
        let create =
            pc::SandboxEffectFence::new("create", "old-owner", "old-boot", 1, u64::MAX - 1)
                .unwrap();
        let terminal =
            pc::SandboxEffectFence::new("cleanup", "new-owner", "new-boot", 2, u64::MAX - 1)
                .unwrap();
        let attempt = SandboxCreateAttempt::new("published-original-attempt");
        let intent = ContainerRealizationIntent::Create;
        let context = ContainerRealizationContext::new(
            &spec.scope,
            &adoption,
            Some(&create),
            &intent,
            &attempt,
        );
        let id = runtime.realization_runtime_id(&spec.scope).unwrap();
        let mut pod = runtime.pod_for_effect(&id, &plan, Some(&create));
        runtime.stamp_effect_evidence(&mut pod, &context, &realization);
        continuation::bind_claim_uid(&mut pod, "claim-uid");
        pod.metadata.uid = Some("pod-uid".into());
        pod.metadata.resource_version = Some("1".into());
        pod.metadata.namespace = Some(runtime.namespace.clone());
        pod.metadata.finalizers = Some(vec![create_ledger::ORIGINAL_GUARD_FINALIZER.into()]);
        pod.status = Some(serde_json::from_value(json!({"phase":"Failed"})).unwrap());
        let original = awaken_sandbox_container::ContainerSandboxHandleV1 {
            container_id: pod.metadata.name.clone().unwrap(),
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            live_input_projection: runtime.supports_live_input_projection(),
            continuation_excluded_paths: vec!["/mnt/excluded-original".into()],
            runtime_handle: Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
                    pod_uid: "pod-uid".into(),
                    claim_uid: Some("claim-uid".into()),
                },
            ),
            sandbox_control_incarnation: control.then(|| {
                awaken_sandbox_container::SandboxControlIncarnation::kubernetes_pod("pod-uid")
                    .unwrap()
            }),
            control_services: spec.control_services.clone(),
        };
        let handle = if row == "legacy-v1" {
            pc::SandboxHandle::container(&spec.scope, original.clone())
        } else {
            pc::SandboxHandle::container_v2(
                &spec.scope,
                awaken_sandbox_container::ContainerSandboxHandleV2 {
                    previous: original.clone(),
                    adoption_fingerprint: adoption.clone(),
                    realization_fingerprint: realization.clone(),
                    owned_paths: awaken_sandbox_container::engine::source_mount_paths(&spec),
                },
            )
        };
        {
            let mut state = state.lock().unwrap();
            state.claim.as_mut().unwrap().metadata.name =
                Some(continuation_claim_for_pod(&original.container_id).unwrap());
            state.pod = Some(pod);
            state.projected.clear();
            match row {
                "declaration" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert(
                            participants::DECLARATION.into(),
                            json!([["config_map", "original-config"]]).to_string(),
                        );
                }
                "malformed-results" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert("awaken.dev/projected-object-results".into(), "{".into());
                }
                "foreign-pod" => {
                    state.pod.as_mut().unwrap().metadata.uid = Some("replacement-pod".into())
                }
                "foreign-claim" => {
                    state.claim.as_mut().unwrap().metadata.uid = Some("replacement-claim".into())
                }
                "already-absent" => {
                    state.pod = None;
                    state.claim = None;
                }
                "absent-pod-live-claim" => state.pod = None,
                "deleting-pod" => {
                    state.pod.as_mut().unwrap().metadata.deletion_timestamp =
                        Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap())
                }
                _ => {}
            }
        }
        let before = {
            let state = state.lock().unwrap();
            (json!(state.pod), json!(state.claim))
        };
        let provider = awaken_sandbox_container::ContainerProvider::new(Arc::new(runtime), image)
            .with_clock(clock);
        let authority = EffectAuthority::new(&terminal);
        if row == "revoked" {
            *authority.current.lock().unwrap() = None;
        }
        if row == "configuration" {
            spec.outputs_path = "/different-outputs".into();
        }
        let prepared = provider
            .prepare_terminal_for_effect(&spec, Some(&handle), None, &terminal, &authority)
            .await;
        assert!(
            state.lock().unwrap().writes.is_empty(),
            "{row}: preparation cannot mutate Kubernetes"
        );
        if row == "already-absent" {
            assert!(prepared.unwrap().is_none(), "R3");
        } else if matches!(row, "empty" | "control" | "deleting-pod") {
            let environment = prepared
                .unwrap_or_else(|e| panic!("R1 {row}: {e}"))
                .unwrap();
            let normalized = environment.handle();
            let mut actual = normalized.container_payload().unwrap();
            assert!(actual.runtime_handle.as_ref().unwrap().terminal_only());
            actual.runtime_handle = original.runtime_handle.clone();
            assert_eq!(actual, original, "R1 preserve complete original metadata");
            assert_eq!(
                normalized.realization_fingerprint(),
                handle.realization_fingerprint()
            );
            assert_eq!(normalized.owned_paths(), handle.owned_paths());
            assert!(
                provider
                    .adopt_for_effect(
                        SandboxAdoption::new(&spec, &normalized),
                        awaken_provisioning_contract::SandboxEffect::Unattached
                    )
                    .await
                    .is_err()
            );
            assert!(environment.spawn(pc::Command::new(["true"])).await.is_err());
            let source = environment
                .prepare_disposal_for_effect(&terminal, &authority)
                .await
                .unwrap();
            let preparation =
                pc::SandboxDisposalPreparation::new(source, realization.as_str()).unwrap();
            let fence = pc::SandboxEffectFence::new(
                preparation.operation_id().unwrap(),
                "new-owner",
                "new-boot",
                2,
                u64::MAX - 1,
            )
            .unwrap();
            let authority = EffectAuthority::new(&fence);
            let authorization = preparation.authorize(fence).unwrap();
            environment
                .dispose_for_effect(&authorization, &authority)
                .await
                .unwrap();
            {
                let state = state.lock().unwrap();
                assert!(
                    state.pod.is_none() && state.claim.is_none(),
                    "R1 exact physical closure"
                );
                assert!(
                    state
                        .writes
                        .iter()
                        .any(|w| w == "DELETE persistentvolumeclaims")
                );
            }
            let writes = state.lock().unwrap().writes.len();
            assert!(
                provider
                    .prepare_terminal_for_effect(
                        &spec,
                        Some(&handle),
                        None,
                        &terminal,
                        &EffectAuthority::new(&terminal)
                    )
                    .await
                    .unwrap()
                    .is_none(),
                "R3 cold original V2 replay"
            );
            assert_eq!(
                state.lock().unwrap().writes.len(),
                writes,
                "R3 no repeated physical effect"
            );
        } else {
            assert!(prepared.is_err(), "R2/R4 {row}");
            let state = state.lock().unwrap();
            assert_eq!(json!(state.pod), before.0, "{row}");
            assert_eq!(json!(state.claim), before.1, "{row}");
        }
        assert_eq!(
            state.lock().unwrap().projected_lists,
            0,
            "never derive original membership from LIST"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn derived_empty_terminal_handle_revalidates_its_effective_plan() {
    use awaken_sandbox_container::ContainerRuntime as _;
    // Causes: C1 original legacy handle or a derived carrier; C2 canonical
    // plan is empty, a Managed File, or an API projection; C3 fingerprint
    // agrees or drifts. R1 empty/Managed File + exact => same original UIDs.
    // R2 API projection/configuration drift => legacy gets no supplement;
    // derived evidence is explicitly refused. This crosses both carrier
    // states with the plan/config and all seven mount-source variants through the runtime
    // port, and proves zero observation I/O rather than current absence.
    // R4 pre-materialization File/Resource/Secret, Memory and cache descriptors
    // are unresolved membership, even when the current content list is empty.
    // Cross every source partition with legacy/derived carriers: no early
    // empty witness, no member lookup, no release or source I/O.
    // R3 a foreign derived namespace is refused, never normalized into this
    // installation. Tokio supplies the kube client's reactor, not API I/O.
    let runtime = crate::K8sRuntime::for_test("127.0.0.1:9".parse().unwrap());
    let fp: pc::SandboxRealizationFingerprint =
        serde_json::from_value(serde_json::json!("original-fingerprint")).unwrap();
    let legacy = awaken_sandbox_container::DecodedContainerHandle {
        payload: awaken_sandbox_container::ContainerSandboxHandleV1 {
            container_id: "original-pod-name".into(),
            outputs_path: "/outputs".into(),
            base_env: vec![],
            live_input_projection: true,
            continuation_excluded_paths: vec![],
            runtime_handle: Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
                    pod_uid: "original-pod".into(),
                    claim_uid: Some("original-claim".into()),
                },
            ),
            sandbox_control_incarnation: None,
            control_services: Default::default(),
        },
        adoption_fingerprint: Some(fp.clone()),
        realization_fingerprint: Some(fp.clone()),
    };
    let mut spec = awaken_sandbox_container::test_support::spec("empty-revalidation");
    spec.mounts.clear();
    spec.env.clear();
    let plan = awaken_sandbox_container::container_plan_with_allowlist(
        &spec,
        "image",
        &awaken_sandbox_container::engine::environment_keepalive_command(),
        None,
        None,
        None,
    )
    .unwrap();
    let mut derived = legacy.clone();
    derived.payload.runtime_handle = runtime.terminal_continuation(&plan, &fp, &legacy).unwrap();
    assert!(
        derived
            .payload
            .runtime_handle
            .as_ref()
            .unwrap()
            .terminal_only()
    );
    let mut foreign = derived.clone();
    let Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
        participants,
        ..
    }) = foreign.payload.runtime_handle.as_mut()
    else {
        panic!("fixture must retain the derived carrier");
    };
    **participants = crate::tests::tampered_participants(participants, |wire| {
        wire["namespace"] = "foreign-namespace".into();
    });
    assert!(
        runtime.terminal_continuation(&plan, &fp, &foreign).is_err(),
        "R3"
    );
    for old in [false, true] {
        for row in [
            "empty",
            "managed-file",
            "managed-binary",
            "api-projection",
            "api-binary",
            "configuration",
            "unresolved-file",
            "unresolved-resource",
            "unresolved-secret",
            "memory",
            "cache",
        ] {
            let mut spec = spec.clone();
            let source = match row {
                "managed-file" | "api-projection" => Some(pc::MountSource::Inline {
                    contents: "original".into(),
                }),
                "managed-binary" | "api-binary" => Some(pc::MountSource::InlineBytes {
                    contents: vec![0, 255, 2],
                    content_hash: None,
                }),
                "unresolved-file" => Some(pc::MountSource::File {
                    file_id: "original-file".into(),
                    content_hash: None,
                }),
                "unresolved-resource" => Some(pc::MountSource::Resource {
                    resource_id: "original-resource".into(),
                    content_hash: None,
                }),
                "unresolved-secret" => Some(pc::MountSource::Secret {
                    reference: "broker://original".into(),
                    content_hash: None,
                }),
                "memory" => Some(pc::MountSource::MemoryStore {
                    store_id: "original-memory".into(),
                    materialization_reference: None,
                    write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
                }),
                "cache" => Some(pc::MountSource::CacheVolume {
                    key: "original-cache".into(),
                    location: pc::CacheVolumeLocation::PersistentVolumeClaim {
                        claim_name: "original-cache-pvc".into(),
                    },
                }),
                _ => None,
            };
            if let Some(source) = source {
                spec.mounts.push(pc::MountRequirement {
                    mount_id: "original-input".into(),
                    source,
                    mount_path: if matches!(row, "managed-file" | "managed-binary") {
                        format!("{}/original", awaken_container_contract::LIVE_INPUTS_ROOT)
                    } else {
                        "/mnt/config".into()
                    },
                    access: pc::MountAccess::ReadOnly,
                    lifetime: pc::MountLifetime::Session,
                    required: true,
                });
            }
            let plan = awaken_sandbox_container::container_plan_with_allowlist(
                &spec,
                "image",
                &awaken_sandbox_container::engine::environment_keepalive_command(),
                None,
                None,
                None,
            )
            .unwrap();
            let changed: pc::SandboxRealizationFingerprint =
                serde_json::from_value(serde_json::json!("changed-fingerprint")).unwrap();
            let result = runtime.terminal_continuation(
                &plan,
                if row == "configuration" {
                    &changed
                } else {
                    &fp
                },
                if old { &legacy } else { &derived },
            );
            if matches!(row, "empty" | "managed-file" | "managed-binary") {
                assert_eq!(
                    result.unwrap().unwrap().kubernetes_incarnation(),
                    Some(("original-pod", Some("original-claim"))),
                    "R1 {old}/{row}"
                );
            } else if old {
                assert!(result.unwrap().is_none(), "R2 legacy {row}");
            } else {
                assert!(result.is_err(), "R2 derived {row}");
            }
        }
    }
}
