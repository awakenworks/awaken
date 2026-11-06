// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Original-create capacity disposal keeps the exact UID/RV, current
// @formal reason: authority, original members and guard; fake HTTP is not physical closure.
//! Original creating attempt disposal after scheduler capacity refusal.

use super::*;

fn retain_unscheduled_original(state: &Mutex<ApiState>, namespace: &str) {
    // The original UCA fixture has an exact empty projected set and a retained
    // original-Create guard. Reuse that same source for the read/CAS races.
    let mut state = state.lock().unwrap();
    state.projected.clear();
    let pod = state.pod.as_mut().unwrap();
    fixture_original_participants(pod, namespace, std::iter::empty());
    mark_original_pending_unschedulable(pod);
}

fn mark_original_pending_unschedulable(pod: &mut Pod) {
    // Status/guard setup only; the existing accepted participants stay intact.
    pod.metadata.finalizers = Some(vec![
        awaken_container_contract::ORIGINAL_GUARD_FINALIZER.into(),
    ]);
    pod.status = Some(serde_json::from_value(json!({
        "phase":"Pending", "conditions":[{
            "type":"PodScheduled", "status":"False", "reason":"Unschedulable", "message":"no capacity"
        }]
    })).unwrap());
}

async fn dispose_unscheduled_original(
    runtime: K8sRuntime,
    fence: pc::SandboxEffectFence,
    authority: Arc<EffectAuthority>,
) -> Result<(), RuntimeError> {
    let admit =
        || pc::with_current_sandbox_effect(&fence, authority.as_ref(), |_| Ok(())).map_err(backend);
    creation::dispose_unscheduled_pod(&runtime, "awaken-cleanup", "pod-uid", "attempt", &admit)
        .await
}

async fn converge_unscheduled_original(
    runtime: K8sRuntime,
    fence: pc::SandboxEffectFence,
    authority: Arc<EffectAuthority>,
) -> Result<(), RuntimeError> {
    // Preserve the original UCA with_attempt admission checkpoint; no new grant,
    // test provider, physical cleanup owner, or alternate activation path.
    let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
    let attempt = SandboxCreateAttempt::new("initial-attempt");
    let recovered = SandboxCreateAttempt::new("attempt");
    let intent = ContainerRealizationIntent::Create;
    let context =
        ContainerRealizationContext::new("scope", &fingerprint, Some(&fence), &intent, &attempt)
            .with_effect(SandboxEffect::Fenced {
                fence: &fence,
                authority: authority.as_ref(),
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
                retain_unscheduled_original(&state, &runtime.namespace);
                let task = tokio::spawn(converge_unscheduled_original(
                    runtime,
                    fence,
                    authority.clone(),
                ));
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
                        let mut current = authority
                            .current
                            .lock()
                            .unwrap_or_else(|poisoned| panic!("{poisoned}"));
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
async fn unscheduled_cleanup_rechecks_the_current_scheduler_refusal_before_any_write() {
    /* Fresh-read cause/effect graph: original Create + exact UID + live grant
     * -> old Unschedulable verdict -> the same fixture pauses BEFORE disposal's
     * new GET. C1 its current scheduler refusal; C2 valid RV; C3 GET success.
     * D1 all remain true -> original UID disposal, then CapacityUnavailable.
     * D2 Ready / running agent unready / ready agent with starting sidecar /
     *    scheduled / gated / missing condition -> C1 false, zero writes.
     * D3 missing/blank RV -> C2 false, zero writes, never early Ok.
     * D4 GET error -> C3 false, zero writes, never inferred absence.
     * D2-D4 keep the original source through MayHaveCommitted/Transport; no
     * closed observation or Session cleanup receipt is manufactured. Running
     * rows keep the OLD False+Unschedulable condition to prove phase matters;
     * the unready agent really has running state and started=true. The original
     * UCA table still independently crosses every reply with authority loss,
     * renewal and cancellation; its before/after-GET admit dominates this gate.
     * This runs the actual kube HTTP client, not a physical cluster/process.
     */
    for row in [
        "stable",
        "ready",
        "running-agent-unready",
        "running-sidecar-waiting",
        "scheduled",
        "gated",
        "missing-condition",
        "missing-rv",
        "blank-rv",
        "read-error",
    ] {
        let fence =
            pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let (mut runtime, state, reached, release, server) = fixture_with_cut(3, false, true).await;
        runtime.unschedulable_wait = Duration::ZERO;
        retain_unscheduled_original(&state, &runtime.namespace);
        let mut task = tokio::spawn(converge_unscheduled_original(runtime, fence, authority));
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("D new disposal GET must be reached after the old capacity verdict");
        let (expected_pod, expected_claim) = {
            let mut state = state.lock().unwrap();
            assert_eq!(state.calls, ["GET pods", "GET pods"], "D old verdict first");
            assert!(state.writes.is_empty(), "D no effect before fresh GET");
            if row == "read-error" {
                state.read_failure = Some(500);
            }
            let expected_pod = {
                let pod = state.pod.as_mut().unwrap();
                let status = pod.status.as_mut().unwrap();
                match row {
                    "ready" | "running-agent-unready" | "running-sidecar-waiting" => {
                        status.phase = Some("Running".into());
                        let mut containers = vec![json!({
                            "name":"agent", "image":"sandbox:test", "imageID":"", "restartCount":0,
                            "ready":row != "running-agent-unready", "started":true, "state":{"running":{}}
                        })];
                        if row == "running-sidecar-waiting" {
                            containers.push(json!({
                                "name":"input-projector", "image":"sandbox:test", "imageID":"", "restartCount":0,
                                "ready":false, "started":false, "state":{"waiting":{"reason":"ContainerCreating"}}
                            }));
                        }
                        status.container_statuses =
                            Some(serde_json::from_value(json!(containers)).unwrap());
                    }
                    "scheduled" => {
                        let scheduled = &mut status.conditions.as_mut().unwrap()[0];
                        scheduled.status = "True".into();
                        scheduled.reason = None;
                    }
                    "gated" => {
                        status.conditions.as_mut().unwrap()[0].reason =
                            Some("SchedulingGated".into());
                    }
                    "missing-condition" => status.conditions = None,
                    "missing-rv" => pod.metadata.resource_version = None,
                    "blank-rv" => pod.metadata.resource_version = Some(" ".into()),
                    "stable" | "read-error" => {}
                    _ => unreachable!(),
                }
                if row == "ready" {
                    assert_eq!(
                        realization::pod_readiness(pod),
                        realization::PodReadiness::Ready
                    );
                } else if row.starts_with("running-") {
                    let agent = &pod
                        .status
                        .as_ref()
                        .unwrap()
                        .container_statuses
                        .as_ref()
                        .unwrap()[0];
                    assert!(agent.state.as_ref().unwrap().running.is_some());
                    assert_eq!(agent.started, Some(true));
                    assert!(matches!(
                        realization::pod_readiness(pod),
                        realization::PodReadiness::Waiting(_)
                    ));
                }
                pod.clone()
            };
            (expected_pod, state.claim.clone())
        };
        release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .expect("D re-read outcome is bounded")
            .expect("D original converge task completes");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        if row == "stable" {
            assert!(
                matches!(result, Err(RuntimeError::CapacityUnavailable(_))),
                "D1"
            );
            assert!(
                state.pod.is_none(),
                "D1 exact UID absence precedes capacity return"
            );
            assert_eq!(
                state.writes,
                ["DELETE pods", "PUT pods"],
                "D1 original guard"
            );
            assert_eq!(state.calls.len(), 7, "D1 original sequence");
        } else {
            let error = result.unwrap_err();
            let detail = error.to_string();
            assert!(
                matches!(&error, RuntimeError::MayHaveCommitted(_)),
                "D2-D4 {row}: {error}"
            );
            let expected = match row {
                "missing-rv" | "blank-rv" => "resourceVersion",
                "read-error" => "read unavailable",
                _ => "no longer Pending and Unschedulable",
            };
            assert!(detail.contains(expected), "D2-D4 {row}: {detail}");
            assert!(
                matches!(
                    pc::SandboxError::from(error),
                    pc::SandboxError::Transport(_)
                ),
                "D2-D4 retain original participants"
            );
            assert!(state.writes.is_empty(), "D2-D4 {row} zero writes");
            assert_eq!(state.calls.len(), 3, "D2-D4 {row} no later request");
            assert_eq!(
                state.pod.as_ref(),
                Some(&expected_pod),
                "D2-D4 original/changed source retained"
            );
        }
        assert_eq!(state.claim, expected_claim, "D1-D4 no PVC authority");
        assert!(
            state.projected.is_empty(),
            "D1-D4 same original empty projected set"
        );
    }
}

#[tokio::test]
async fn unscheduled_cleanup_keeps_exact_delete_preconditions_across_uid_and_version_races() {
    /* CAS table: C1 disposal's fresh GET is exact Pending+Unschedulable/RV=1;
     * C2 UID changes before API accepts DELETE; C3 RV changes at that boundary.
     * X1 !C2&&!C3 -> original positive UID/RV assertions + exact disposal.
     * X2 C2 or X3 C3 -> actual HTTP 409, no accepted write or next request;
     * the retained source and PVC survive. The fixture pauses BEFORE accepting
     * the fourth request (DELETE), not after deleting and merely hiding a reply.
     * The unique ApiState DELETE handler supplies both refusals and keeps its
     * old accepted-request assertions. No second fake/CAS oracle is introduced.
     * The converge caller preserves post-materialization uncertainty through
     * the existing MayHaveCommitted class instead of reporting safe capacity.
     */
    for row in ["stable", "uid", "rv"] {
        let fence =
            pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let (mut runtime, state, reached, release, server) = fixture_with_cut(4, false, true).await;
        runtime.unschedulable_wait = Duration::ZERO;
        retain_unscheduled_original(&state, &runtime.namespace);
        let mut task = tokio::spawn(converge_unscheduled_original(runtime, fence, authority));
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("X original DELETE boundary must be reached");
        let (expected_pod, expected_claim) = {
            let mut state = state.lock().unwrap();
            assert_eq!(state.calls, ["GET pods"; 3], "X exact GET precedes DELETE");
            assert!(
                state.writes.is_empty(),
                "X original empty set has no prefix effects"
            );
            let pod = state.pod.as_mut().unwrap();
            match row {
                "stable" => {}
                "uid" => pod.metadata.uid = Some("replacement-uid".into()),
                "rv" => pod.metadata.resource_version = Some("2".into()),
                _ => unreachable!(),
            }
            (pod.clone(), state.claim.clone())
        };
        release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .expect("X CAS outcome is bounded")
            .expect("X original converge task completes");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        if row == "stable" {
            assert!(
                matches!(result, Err(RuntimeError::CapacityUnavailable(_))),
                "X1"
            );
            assert!(state.pod.is_none(), "X1 exact UID absence");
            assert_eq!(
                state.writes,
                ["DELETE pods", "PUT pods"],
                "X1 original guard"
            );
            assert_eq!(state.calls.len(), 7, "X1 original sequence");
        } else {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, RuntimeError::MayHaveCommitted(_)),
                "X2/X3 {row}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("stale DELETE UID or resourceVersion")
            );
            assert!(
                state.writes.is_empty(),
                "X2/X3 CAS refused before any write"
            );
            assert_eq!(
                state.calls.len(),
                4,
                "X2/X3 no unfenced retry/guard release"
            );
            assert_eq!(
                state.pod.as_ref(),
                Some(&expected_pod),
                "X2/X3 newer source untouched"
            );
        }
        assert_eq!(state.claim, expected_claim, "X1-X3 PVC untouched");
        assert!(state.projected.is_empty(), "X1-X3 same original empty set");
    }
}

#[tokio::test]
async fn unscheduled_cleanup_fences_the_pod_before_deleting_nonempty_original_participants() {
    /* NP cause/effect table: C1 the fresh GET is original Pending + explicit
     * Unschedulable, with the actual original two ConfigMaps and two Secrets;
     * C2 the Pod UID/RV changes before the first DELETE is accepted. NP1 stable
     * -> first accepted write is Pod UID/RV DELETE, then all original members
     * close, then original guard release. NP2 foreign UID / NP3 new RV / NP4 a
     * Running-unready agent with new RV -> real HTTP409 before ANY member
     * deletion, no retry or guard release, exact original four members + PVC
     * untouched. Status alone is not a CAS coordinate: the API's scheduler or
     * status update also advances RV. The existing cut pauses before request2;
     * with the old unsafe ordering this is a member GET, so its later accepted
     * member DELETEs expose the regression instead of an empty-set false pass.
     * Producer MayHaveCommitted and Memory retention are covered by the actual
     * converge and activation tables; this layer tests the same raw disposer.
     */
    for row in ["stable", "uid", "rv", "running-unready"] {
        let fence =
            pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let (runtime, state, reached, release, server) = fixture_with_cut(2, false, true).await;
        mark_original_pending_unschedulable(state.lock().unwrap().pod.as_mut().unwrap());
        let original_members = state.lock().unwrap().projected.clone();
        assert_eq!(original_members.len(), 4, "NP actual nonempty original set");
        let mut task = tokio::spawn(dispose_unscheduled_original(runtime, fence, authority));
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        let (expected_pod, expected_claim) = {
            let mut state = state.lock().unwrap();
            assert_eq!(
                state.calls,
                ["GET pods"],
                "NP only fresh GET before first CAS"
            );
            assert!(state.writes.is_empty());
            let pod = state.pod.as_mut().unwrap();
            match row {
                "stable" => {}
                "uid" => pod.metadata.uid = Some("replacement-uid".into()),
                "rv" => pod.metadata.resource_version = Some("2".into()),
                "running-unready" => {
                    pod.metadata.resource_version = Some("2".into());
                    pod.status = Some(
                        serde_json::from_value(json!({
                            "phase":"Running", "conditions":[
                                {"type":"PodScheduled","status":"True"},
                                {"type":"Ready","status":"False"}
                            ], "containerStatuses":[{
                                "name":"agent", "image":"agent", "imageID":"agent-sha",
                                "restartCount":0, "ready":false, "started":true,
                                "state":{"running":{"startedAt":"2026-10-18T00:00:00Z"}}
                            }]
                        }))
                        .unwrap(),
                    );
                }
                _ => unreachable!(),
            }
            (pod.clone(), state.claim.clone())
        };
        release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        if row == "stable" {
            result.unwrap();
            assert!(state.pod.is_none(), "NP1 original UID absence");
            assert!(
                state.projected.is_empty(),
                "NP1 all original members closed"
            );
            assert_eq!(
                state.writes.first().map(String::as_str),
                Some("DELETE pods")
            );
            assert_eq!(state.writes.last().map(String::as_str), Some("PUT pods"));
            assert_eq!(
                state
                    .writes
                    .iter()
                    .filter(|call| *call == "DELETE configmaps")
                    .count(),
                2
            );
            assert_eq!(
                state
                    .writes
                    .iter()
                    .filter(|call| *call == "DELETE secrets")
                    .count(),
                2
            );
            assert_eq!(
                state.writes.len(),
                6,
                "NP1 one Pod CAS, four members, one guard CAS"
            );
        } else {
            let error = result.unwrap_err();
            assert!(
                matches!(&error, RuntimeError::Conflict(_)),
                "NP2-4 {row}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("stale DELETE UID or resourceVersion")
            );
            assert_eq!(
                state.calls,
                ["GET pods", "DELETE pods"],
                "NP2-4 no participant request or retry"
            );
            assert!(
                state.writes.is_empty(),
                "NP2-4 CAS failure has no accepted prefix"
            );
            assert_eq!(
                state.projected, original_members,
                "NP2-4 original members survive"
            );
            assert_eq!(
                state.pod.as_ref(),
                Some(&expected_pod),
                "NP2-4 source + original guard unchanged"
            );
        }
        assert_eq!(state.claim, expected_claim, "NP no PVC disposal authority");
        assert_eq!(
            state.projected_lists, 0,
            "NP original retained set, no discovery"
        );
    }
}

#[tokio::test]
async fn unscheduled_cleanup_keeps_nonempty_participants_when_delete_response_loses_authority() {
    /* NL causes: NP's nonempty original set, original Pod DELETE accepted, but
     * its response is held. Change current authority to revoked/reassigned/
     * expired, or cancel. Effect NL1: accepted prefix is ONLY Pod DELETE;
     * original guard remains on the terminating original UID, all original
     * ConfigMaps/Secrets and PVC remain, no next request or invented receipt.
     * Stable completion is NP1; UCA covers renewal at every reply. Reuse the
     * same scheduler/API state, never mint a replacement cleanup grant.
     */
    for change in ['v', 'g', 'e', 'c'] {
        let fence =
            pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let (runtime, state, reached, release, server) = fixture(2, false).await;
        mark_original_pending_unschedulable(state.lock().unwrap().pod.as_mut().unwrap());
        let original_members = state.lock().unwrap().projected.clone();
        let original_claim = state.lock().unwrap().claim.clone();
        let mut task = tokio::spawn(dispose_unscheduled_original(
            runtime,
            fence,
            authority.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.calls, ["GET pods", "DELETE pods"]);
            assert_eq!(state.writes, ["DELETE pods"]);
            assert_eq!(state.projected, original_members);
            let pod = state.pod.as_ref().unwrap();
            assert_eq!(pod.metadata.uid.as_deref(), Some("pod-uid"));
            assert!(pod.metadata.deletion_timestamp.is_some());
            assert!(
                awaken_worker_fleet_kubernetes::original_guard_release_required(Some(pod), true)
                    .unwrap()
            );
        }
        if change == 'c' {
            task.abort();
        } else {
            let mut grant = authority
                .current
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"));
            match change {
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
            assert!(result.unwrap_err().is_cancelled(), "NL1 cancel");
        } else {
            assert!(
                result.unwrap().is_err(),
                "NL1 {change} no completed disposal"
            );
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            ["GET pods", "DELETE pods"],
            "NL1 no request after failed current-authority reentry"
        );
        assert_eq!(
            state.writes,
            ["DELETE pods"],
            "NL1 preserve exact accepted prefix"
        );
        assert_eq!(state.projected, original_members, "NL1 members retained");
        assert_eq!(state.claim, original_claim, "NL1 PVC retained");
        let pod = state.pod.as_ref().unwrap();
        assert!(pod.metadata.deletion_timestamp.is_some());
        assert!(
            awaken_worker_fleet_kubernetes::original_guard_release_required(Some(pod), true)
                .unwrap()
        );
    }
}

#[tokio::test]
async fn unscheduled_cleanup_keeps_the_original_guard_while_a_projected_uid_has_a_finalizer() {
    /* NF table: exact Pending-unschedulable original Pod + nonempty original
     * participants; a ConfigMap (NF1) / Secret (NF2) accepts DELETE but its
     * original UID remains behind a foreign finalizer. First write must still
     * be Pod CAS, but no success or original-guard release is allowed until
     * that member UID is actually absent. The existing exact-member wait owns
     * this constraint; do not wait for guarded Pod absence first, remove foreign
     * finalizers, or count accepted DELETE as closure. Controller absence is
     * simulated by removing only the exact blocked fixture member.
     */
    for (name, kind) in [("configmaps-0", "configmaps"), ("secrets-0", "secrets")] {
        let fence =
            pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let (runtime, state, _, _, server) = fixture(0, false).await;
        mark_original_pending_unschedulable(state.lock().unwrap().pod.as_mut().unwrap());
        let original_claim = state.lock().unwrap().claim.clone();
        let accepted = state.lock().unwrap().projected_deleted.clone();
        state
            .lock()
            .unwrap()
            .projected_behavior
            .insert(name.into(), "finalizer");
        let mut task = tokio::spawn(dispose_unscheduled_original(runtime, fence, authority));
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
        {
            let state = state.lock().unwrap();
            assert!(
                !task.is_finished(),
                "NF1/NF2 accepted member DELETE is not UID absence"
            );
            assert_eq!(
                state.writes.first().map(String::as_str),
                Some("DELETE pods")
            );
            assert!(
                !state.writes.iter().any(|call| call == "PUT pods"),
                "NF1/NF2 guard not released"
            );
            assert!(
                state.projected.contains(name),
                "NF1/NF2 original member still exists"
            );
            let pod = state.pod.as_ref().unwrap();
            assert!(
                pod.metadata.deletion_timestamp.is_some(),
                "NF1/NF2 exact Pod deletion intent accepted first"
            );
            assert!(
                awaken_worker_fleet_kubernetes::original_guard_release_required(Some(pod), true)
                    .unwrap()
            );
            assert_eq!(state.claim, original_claim);
        }
        state.lock().unwrap().projected.remove(name);
        tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let state = state.lock().unwrap();
        assert!(state.projected.is_empty(), "NF original set fully closed");
        assert!(
            state.pod.is_none(),
            "NF original UID absent only after original guard release"
        );
        assert_eq!(state.writes.last().map(String::as_str), Some("PUT pods"));
        assert_eq!(state.claim, original_claim, "NF no PVC disposal authority");
        assert_eq!(state.projected_lists, 0);
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
            let mut task = tokio::spawn(dispose_unscheduled_original(
                runtime,
                fence,
                authority.clone(),
            ));
            tokio::time::timeout(Duration::from_secs(5), reached.notified())
                .await
                .unwrap();
            if change == 'c' {
                task.abort();
            } else {
                let mut current = authority
                    .current
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"));
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
