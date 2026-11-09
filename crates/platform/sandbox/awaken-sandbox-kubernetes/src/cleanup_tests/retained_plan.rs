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
    // R5 before preparations are durable, Actor retirement must observe the
    // same retained original source and independent exact live claim. R5-L is
    // a Running original: Actor exit preserves this independent live data source.
    // R5-T is terminal. Removing the guard, a known phase or GC independence
    // denies retirement; terminal cleanup still requires its terminal guard.
    // R5-I running sources also require the same original Pod/PVC identities
    // and both participants present. R2-L Running never authorizes terminal
    // preparation. Cross these through the same HTTP observer and assert the
    // complete before/after Pod/PVC snapshot and empty write log for every read.
    // observing this read never mutates or prepares/disposes the source.
    // P1 cause/effect graph: C1 claim UID exact/mismatched; C2 Pod effect epoch
    // valid/malformed. All other original-source/guard/GC facts remain fixed.
    // | Rule | claim C1 | epoch C2 | observation | effect |
    // | P1a | exact | valid | RetainedOriginal | unchanged snapshots/no writes |
    // | P1b | mismatch | valid | Incompatible | same zero effects |
    // | P1c | mismatch | bad | Incompatible before Pod decoding | same |
    // | P1d | exact | bad | original Backend codec error | same |
    // P1c preserves the established known-negative precedence; P1d prevents
    // masking genuine evidence errors. Existing 403/503 rows own API failures.
    // Image-history table H: H1 captured original rootfs + changed deployment
    // image -> original prepare/dispose/replay; H2 historical complete V3
    // namespace/UID/participants + double-matching image candidate -> H1.
    // H3 UID, namespace, immutable spec or actual image drift -> zero writes.
    // H4 exact total absence -> no owner/no repeated delete; H5 absent Pod +
    // live claim remains refused. H6 403/503 remains an error, never absence.
    for row in [
        "empty",
        "control",
        "nonempty-plan",
        "declaration",
        "malformed-results",
        "foreign-pod",
        "foreign-claim",
        "configuration",
        "captured-image-upgrade",
        "historical-image",
        "historical-running-image",
        "historical-provisioning-image",
        "historical-foreign-pod",
        "historical-foreign-claim",
        "historical-namespace",
        "historical-spec-drift",
        "historical-image-tamper",
        "historical-absent",
        "historical-pod-absent-live-claim",
        "historical-api-403",
        "historical-api-503",
        "legacy-v1",
        "revoked",
        "already-absent",
        "deleting-pod",
        "running",
        "absent-pod-live-claim",
        "retirement-held",
        "retirement-no-guard",
        "retirement-running",
        "retirement-running-foreign-pod",
        "retirement-running-foreign-claim",
        "retirement-running-foreign-claim-bad-epoch",
        "retirement-running-bad-epoch",
        "retirement-running-missing-pod",
        "retirement-running-missing-claim",
        "retirement-running-no-guard",
        "retirement-running-isolated",
        "retirement-running-pod-owner",
        "retirement-running-claim-owner",
        "retirement-running-claim-deleting",
        "retirement-running-api-403",
        "retirement-running-api-503",
        "retirement-pending",
        "retirement-unknown",
        "retirement-missing-phase",
        "retirement-deleting",
        "retirement-pod-owner",
        "retirement-claim-owner",
        "retirement-prepared-running",
        "retirement-prepared-absent",
        "retirement-prepared-disposing",
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
        pod.metadata.finalizers = Some(vec![
            awaken_container_contract::ORIGINAL_GUARD_FINALIZER.into(),
        ]);
        pod.status = Some(serde_json::from_value(json!({"phase":"Failed"})).unwrap());
        let mut original = awaken_sandbox_container::ContainerSandboxHandleV1 {
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
        if row.starts_with("historical-") {
            original.runtime_handle = Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    pod_uid: "pod-uid".into(),
                    claim_uid: Some("claim-uid".into()),
                    participants: Box::new(
                        awaken_sandbox_container::KubernetesProjectedParticipants::new(
                            if row == "historical-namespace" {
                                "foreign".into()
                            } else {
                                runtime.namespace.clone()
                            },
                            "pod-uid".into(),
                            vec![],
                            Some(attempt.as_str().to_owned()),
                            None,
                        )
                        .unwrap(),
                    ),
                },
            );
        }
        let handle = if row == "legacy-v1" {
            pc::SandboxHandle::container(&spec.scope, original.clone())
        } else {
            pc::SandboxHandle::container_v2(
                &spec.scope,
                awaken_sandbox_container::ContainerSandboxHandleV2 {
                    creation_rootfs: (row == "captured-image-upgrade").then(|| plan.rootfs.clone()),
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
                "historical-foreign-pod" => {
                    state.pod.as_mut().unwrap().metadata.uid = Some("foreign-pod".into())
                }
                "historical-foreign-claim" => {
                    state.claim.as_mut().unwrap().metadata.uid = Some("foreign-claim".into())
                }
                "historical-image-tamper" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .spec
                        .as_mut()
                        .unwrap()
                        .containers[0]
                        .image = Some("foreign-image".into())
                }
                "historical-absent" => {
                    state.pod = None;
                    state.claim = None;
                }
                "historical-pod-absent-live-claim" => state.pod = None,
                "historical-api-403" => state.read_failure = Some(403),
                "historical-api-503" => state.read_failure = Some(503),
                "historical-running-image" => {
                    let pod = state.pod.as_mut().unwrap();
                    pod.status = Some(serde_json::from_value(json!({
                        "phase":"Running", "containerStatuses":pod.spec.as_ref().unwrap().containers.iter().map(|container| json!({
                            "name":container.name,"ready":true,"image":"image@test","imageID":"image@test",
                            "restartCount":0,"started":true,"state":{"running":{}}
                        })).collect::<Vec<_>>()
                    })).unwrap());
                }
                "historical-provisioning-image" => {
                    state.pod.as_mut().unwrap().status =
                        Some(serde_json::from_value(json!({"phase":"Pending"})).unwrap())
                }
                _ => {}
            }
            // R5-L negative controls each remove one live-source premise;
            // the same real HTTP observer must refuse without any writes.
            if row.starts_with("retirement-running")
                || matches!(row, "running" | "retirement-prepared-running")
            {
                state.pod.as_mut().unwrap().status =
                    Some(serde_json::from_value(json!({"phase":"Running"})).unwrap());
            }
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
                "foreign-pod" | "retirement-running-foreign-pod" => {
                    state.pod.as_mut().unwrap().metadata.uid = Some("replacement-pod".into())
                }
                "foreign-claim"
                | "retirement-running-foreign-claim"
                | "retirement-running-foreign-claim-bad-epoch" => {
                    state.claim.as_mut().unwrap().metadata.uid = Some("replacement-claim".into())
                }
                "already-absent" => {
                    state.pod = None;
                    state.claim = None;
                }
                "absent-pod-live-claim" | "retirement-running-missing-pod" => state.pod = None,
                "retirement-running-missing-claim" => state.claim = None,
                "deleting-pod" => {
                    state.pod.as_mut().unwrap().metadata.deletion_timestamp =
                        Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap())
                }
                "retirement-no-guard" | "retirement-running-no-guard" => {
                    state.pod.as_mut().unwrap().metadata.finalizers = None
                }
                "retirement-running-isolated" => {
                    state
                        .pod
                        .as_mut()
                        .unwrap()
                        .metadata
                        .labels
                        .get_or_insert_with(Default::default)
                        .insert(
                            awaken_container_contract::ISOLATION_LABEL.into(),
                            "true".into(),
                        );
                }
                "retirement-running-claim-deleting" => {
                    state.claim.as_mut().unwrap().metadata.deletion_timestamp =
                        Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap())
                }
                "retirement-running-api-403" => state.read_failure = Some(403),
                "retirement-running-api-503" => state.read_failure = Some(503),
                "retirement-pending" => {
                    state.pod.as_mut().unwrap().status =
                        Some(serde_json::from_value(json!({"phase":"Pending"})).unwrap())
                }
                "retirement-unknown" => {
                    state.pod.as_mut().unwrap().status =
                        Some(serde_json::from_value(json!({"phase":"Unknown"})).unwrap())
                }
                "retirement-missing-phase" => state.pod.as_mut().unwrap().status = None,
                "retirement-deleting" => {
                    state.pod.as_mut().unwrap().metadata.deletion_timestamp =
                        Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap())
                }
                "retirement-pod-owner"
                | "retirement-claim-owner"
                | "retirement-running-pod-owner"
                | "retirement-running-claim-owner" => {
                    let owner = serde_json::from_value(json!({"apiVersion":"v1","kind":"Pod",
                        "name":"retiring-worker","uid":"worker-pod-uid"}))
                    .unwrap();
                    if row.ends_with("pod-owner") {
                        state.pod.as_mut().unwrap().metadata.owner_references = Some(vec![owner]);
                    } else {
                        state.claim.as_mut().unwrap().metadata.owner_references = Some(vec![owner]);
                    }
                }
                "retirement-prepared-absent" => {
                    state.pod = None;
                    state.claim = None;
                }
                "retirement-prepared-disposing" => {
                    state.pod.as_mut().unwrap().metadata.deletion_timestamp =
                        Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap());
                    state.claim.as_mut().unwrap().metadata.deletion_timestamp =
                        Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap());
                }
                _ => {}
            }
            if matches!(
                row,
                "retirement-running-foreign-claim-bad-epoch" | "retirement-running-bad-epoch"
            ) {
                state
                    .pod
                    .as_mut()
                    .unwrap()
                    .metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .insert(
                        crate::pod_evidence::SANDBOX_EFFECT_EPOCH_ANNOTATION.into(),
                        "bad".into(),
                    );
            }
        }
        let before = {
            let state = state.lock().unwrap();
            (json!(state.pod), json!(state.claim))
        };
        let replacement_image = if row == "captured-image-upgrade" || row.starts_with("historical-")
        {
            "replacement-deployment-image"
        } else {
            image
        };
        let provider =
            awaken_sandbox_container::ContainerProvider::new(Arc::new(runtime), replacement_image)
                .with_clock(clock);
        let authority = EffectAuthority::new(&terminal);
        if row == "revoked" {
            *authority
                .current
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}")) = None;
        }
        if matches!(row, "configuration" | "historical-spec-drift") {
            spec.outputs_path = "/different-outputs".into();
        }
        if row.starts_with("retirement-") {
            let source_prepared = row.starts_with("retirement-prepared-");
            let mut adoption = SandboxAdoption::new(&spec, &handle);
            adoption.observation_purpose =
                awaken_provisioning_contract::SandboxObservationPurpose::WorkerRetirement {
                    source_prepared,
                };
            let observed = provider
                .observe(adoption, adoption.effect_basis(&create))
                .await;
            // R5-E real API authority/mechanism failures remain typed errors;
            // known failed retention premises are ordinary negative facts.
            match row {
                "retirement-running-api-403" => {
                    assert!(matches!(&observed, Err(pc::SandboxError::Unauthorized(_))))
                }
                "retirement-running-api-503" => {
                    assert!(matches!(&observed, Err(pc::SandboxError::Backend { .. })))
                }
                "retirement-running-bad-epoch" => {
                    assert!(
                        matches!(&observed,
                            Err(pc::SandboxError::Backend { detail, .. })
                                if detail == "invalid Sandbox effect epoch"),
                        "P1d genuine codec failure remains an error: {observed:?}"
                    );
                }
                _ if !source_prepared
                    && !matches!(
                        row,
                        "retirement-held" | "retirement-running" | "retirement-deleting"
                    ) =>
                {
                    assert!(
                        matches!(&observed, Ok(pc::SandboxObservation::Incompatible { .. })),
                        "R5 negative fact {row}: {observed:?}"
                    );
                }
                _ => {}
            }
            if row == "retirement-running" {
                // LR2: exact guarded Running source + independent live claim
                // may qualify retention, but its physical lifecycle has not
                // ended. Never replace that fact with Terminal, Disposing or
                // absence; the positive-retention assertion below is separate.
                assert!(
                    !matches!(
                        &observed,
                        Ok(pc::SandboxObservation::Terminal { .. }
                            | pc::SandboxObservation::Disposing { .. }
                            | pc::SandboxObservation::DefinitivelyUnavailable { .. })
                    ),
                    "LR2 Running source retention cannot claim physical completion: {observed:?}"
                );
                // LR3 Recovery retains the physical Ready/Provisioning fact.
                // Direct TerminalCleanup of this legacy V2 carrier lacks the
                // original projected-member witness: keep that exact uncertainty
                // until canonical terminal preparation derives its empty plan.
                // Neither read may manufacture retention or physical completion.
                let physical = SandboxAdoption::new(&spec, &handle);
                let recovered = provider
                    .observe(physical, physical.effect_basis(&create))
                    .await
                    .unwrap();
                assert!(
                    matches!(
                        recovered,
                        pc::SandboxObservation::Ready | pc::SandboxObservation::Provisioning { .. }
                    ),
                    "LR3 Recovery: {recovered:?}"
                );
                let mut physical = SandboxAdoption::new(&spec, &handle);
                physical.observation_purpose = pc::SandboxObservationPurpose::TerminalCleanup;
                let terminal = provider
                    .observe(physical, physical.effect_basis(&create))
                    .await;
                assert!(
                    matches!(&terminal, Err(pc::SandboxError::Backend { operation: "container runtime", detail }) if detail == "terminal Kubernetes projected-object membership is unknown"),
                    "LR3 legacy terminal membership: {terminal:?}"
                );
            }
            // R6 durable preparation belongs to the aggregate. Once supplied,
            // a running substrate, disposing pair or exact absence retains
            // the canonical physical replay path without new source I/O.
            let recoverable = matches!(
                observed,
                Ok(pc::SandboxObservation::RetainedOriginal { .. })
            ) || (source_prepared
                && matches!(
                    observed,
                    Ok(pc::SandboxObservation::Ready
                        | pc::SandboxObservation::Terminal { .. }
                        | pc::SandboxObservation::Provisioning { .. }
                        | pc::SandboxObservation::Disposing { .. }
                        | pc::SandboxObservation::DefinitivelyUnavailable { .. })
                ));
            assert_eq!(
                recoverable,
                source_prepared
                    || matches!(
                        row,
                        "retirement-held" | "retirement-running" | "retirement-deleting"
                    ),
                "R5/R6 {row}: {observed:?}"
            );
            {
                let snapshot = state.lock().unwrap();
                assert!(snapshot.writes.is_empty(), "R5 observation is read-only");
                assert_eq!((json!(snapshot.pod), json!(snapshot.claim)), before);
            }
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            continue;
        }
        let prepared = provider
            .prepare_terminal_for_effect(&spec, Some(&handle), None, &terminal, &authority)
            .await;
        assert!(
            state.lock().unwrap().writes.is_empty(),
            "{row}: preparation cannot mutate Kubernetes"
        );
        if matches!(row, "already-absent" | "historical-absent") {
            assert!(prepared.unwrap().is_none(), "R3");
        } else if matches!(
            row,
            "empty"
                | "control"
                | "deleting-pod"
                | "captured-image-upgrade"
                | "historical-image"
                | "historical-running-image"
        ) {
            let environment = prepared
                .unwrap_or_else(|e| panic!("R1 {row}: {e}"))
                .unwrap();
            let normalized = environment.handle();
            let mut actual = normalized.container_payload().unwrap();
            if !row.starts_with("historical-") {
                assert!(actual.runtime_handle.as_ref().unwrap().terminal_only());
            }
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
            if matches!(row, "historical-image" | "historical-running-image") {
                // H2 proves exact original source reconstruction without live
                // Control or writes. Unlike FrozenPlanEmpty's physical-only
                // target, this source needs real process quiescence before
                // preparation can be durable. This HTTP fixture has no exec
                // transport; controlled process and ACK tests own that effect.
                let snapshot = state.lock().unwrap();
                assert_eq!((json!(snapshot.pod), json!(snapshot.claim)), before);
                assert!(snapshot.writes.is_empty());
                drop(snapshot);
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
                continue;
            }
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
                .unwrap_or_else(|error| panic!("H1/H2 {row} physical disposal: {error}"));
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
            match row {
                "historical-api-403" => assert!(
                    matches!(&prepared, Err(pc::SandboxError::Unauthorized(_))),
                    "H6 permission remains an error: {:?}",
                    prepared.as_ref().err()
                ),
                "historical-api-503" => assert!(
                    matches!(&prepared, Err(pc::SandboxError::Backend { .. })),
                    "H6 backend failure is not absence: {:?}",
                    prepared.as_ref().err()
                ),
                "historical-provisioning-image" => assert!(
                    matches!(&prepared, Err(pc::SandboxError::NotReady(_))),
                    "H2 pending original is not a terminal absence: {:?}",
                    prepared.as_ref().err()
                ),
                _ => {}
            }
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
        creation_rootfs: None,
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
