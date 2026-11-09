// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Original result/response-loss adapter decisions; these mocks do not qualify physical closure.
use super::super::ephemeral_plan;
use super::*;
use awaken_container_contract::RuntimeError;
use awaken_container_contract::{CreateTicket, CreateTicketRequest, TicketResult, TicketStatus};
use awaken_provisioning_contract::SandboxCreateAttempt;
use awaken_sandbox_container::ContainerRealizationIntent;
use http::{Request, Response};
use kube::client::Body;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn original_projected_create_results_never_borrow_a_later_get_uid() {
    // Causes: C1 original successful POST or bare409/currentGET; C2 original
    // UID, missing/blank UID, changed owner, or backend failure. Effects: R1
    // original201+exactowner+UID -> retained identity; R2 bare409+same digest
    // -> Unknown, irrespective of currentGET UID; R3 each bad C2 -> no retained
    // result. R2 is the direct-mode response-loss boundary: its local join gate
    // has no durable original UID. Broker replay's original ticket+nonce+UID
    // positive case is tested at the same Coordinator create route.
    let runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some("awaken-subject".into()),
            uid: Some("pod-uid".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let owner = Some(pod_owner_reference(&pod, "pod-uid").unwrap());
    for row in [
        "original",
        "bare409",
        "missing",
        "blank",
        "owner",
        "api-error",
    ] {
        let posts = Arc::new(AtomicUsize::new(0));
        let mut desired = build_configmap("subject", 0, Some("private-input"), None, &owner);
        let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
        let attempt = SandboxCreateAttempt::new("attempt");
        let intent = ContainerRealizationIntent::Create;
        let context =
            ContainerRealizationContext::new("scope", &fingerprint, None, &intent, &attempt);
        runtime.stamp_effect_evidence(&mut desired, &context, &fingerprint);
        stamp_realization(&mut desired).unwrap();
        let encoded = serde_json::to_value(&desired).unwrap();
        let service = tower::service_fn({
            let posts = posts.clone();
            move |request: Request<Body>| {
                let posts = posts.clone();
                let mut object = encoded.clone();
                async move {
                    let post = request.method() == http::Method::POST;
                    if post {
                        posts.fetch_add(1, Ordering::SeqCst);
                    }
                    let (code, value) = if row == "api-error" {
                        (
                            500,
                            json!({"kind":"Status","apiVersion":"v1","code":500,"reason":"Unavailable"}),
                        )
                    } else if row == "bare409" && post {
                        (
                            409,
                            json!({"kind":"Status","apiVersion":"v1","code":409,"reason":"AlreadyExists"}),
                        )
                    } else {
                        object["metadata"]["uid"] = "original-uid".into();
                        match row {
                            "missing" => {
                                object["metadata"].as_object_mut().unwrap().remove("uid");
                            }
                            "blank" => object["metadata"]["uid"] = " ".into(),
                            "owner" => {
                                object["metadata"]["ownerReferences"][0]["uid"] = "foreign".into()
                            }
                            _ => {}
                        };
                        (if post { 201 } else { 200 }, object)
                    };
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(code)
                            .body(Body::from(serde_json::to_vec(&value).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        let api: Api<ConfigMap> = Api::namespaced(kube::Client::new(service, "default"), "default");
        let result = original_result(&api, &desired).await.and_then(|object| {
            KubernetesProjectedParticipants::new(
                "default".into(),
                "pod-uid".into(),
                vec![object],
                Some("attempt".into()),
                None,
            )
            .map_err(backend)
        });
        assert_eq!(result.is_ok(), row == "original", "R1-R3/{row}");
        assert_eq!(posts.load(Ordering::SeqCst), 1, "no duplicate POST/{row}");
    }
}

#[tokio::test]
async fn direct_complete_original_projection_replays_without_new_creates() {
    // R4: original complete empty result handoff + same original
    // attempt -> reuse exactly those UIDs, no new POST/GET/PUT. R5 missing
    // original declaration -> Unknown rather than borrowing today's empty plan.
    // R6 another Pod UID/attempt -> reject copied handoff; R7 authority denied
    // -> no replay publication. This is an original-result replay, not a GET
    // that repairs direct response loss before original UID persistence.
    let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    runtime.clients.control = kube::Client::new(
        tower::service_fn(|_request: Request<Body>| async {
            panic!("complete replay must not send a new Kubernetes request");
            #[allow(unreachable_code)]
            Ok::<_, std::io::Error>(Response::new(Body::empty()))
        }),
        "default",
    );
    let plan = ephemeral_plan();
    let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
    let attempt = SandboxCreateAttempt::new("attempt");
    let intent = ContainerRealizationIntent::Create;
    let context = ContainerRealizationContext::new("scope", &fingerprint, None, &intent, &attempt);
    let mut pod = Pod {
        metadata: ObjectMeta {
            name: Some("awaken-subject".into()),
            uid: Some("pod-uid".into()),
            resource_version: Some("1".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    runtime.stamp_effect_evidence(&mut pod, &context, &fingerprint);
    declare(&mut pod, "subject", &plan).unwrap();
    let original = KubernetesProjectedParticipants::new(
        "default".into(),
        "pod-uid".into(),
        Vec::new(),
        Some("attempt".into()),
        None,
    )
    .unwrap();
    pod.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(RESULTS.into(), serde_json::to_string(&original).unwrap());
    assert_eq!(
        realize(
            &runtime,
            pod.clone(),
            "subject",
            &plan,
            Some((&context, &fingerprint)),
            None
        )
        .await
        .unwrap(),
        original,
        "R4"
    );
    for row in ["missing-declaration", "foreign-pod", "foreign-attempt"] {
        let mut changed = pod.clone();
        match row {
            "missing-declaration" => {
                changed
                    .metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .remove(DECLARATION);
            }
            "foreign-pod" => changed.metadata.uid = Some("new-pod".into()),
            "foreign-attempt" => {
                changed
                    .metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .insert(SANDBOX_ATTEMPT_ANNOTATION.into(), "new-attempt".into());
            }
            _ => unreachable!(),
        }
        assert!(
            realize(
                &runtime,
                changed,
                "subject",
                &plan,
                Some((&context, &fingerprint)),
                None
            )
            .await
            .is_err(),
            "R5/R6/{row}"
        );
    }
    let fence = pc::SandboxEffectFence::new("create", "worker", "boot", 1, u64::MAX - 1).unwrap();
    let authority = pc::test_support::EffectAuthority::denied();
    let denied =
        ContainerRealizationContext::new("scope", &fingerprint, Some(&fence), &intent, &attempt)
            .with_effect(awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &fence,
                authority: &authority,
            });
    assert!(
        realize(
            &runtime,
            pod,
            "subject",
            &plan,
            Some((&denied, &fingerprint)),
            None
        )
        .await
        .is_err(),
        "R7"
    );
}

// Same tests-owned transport for the decision matrix and the provider protocol
// cases. Complete is an injected closure fact, never a Registry qualification.
pub(crate) struct ClosedProjectedRequests {
    pub(crate) tickets: Arc<std::sync::Mutex<Vec<awaken_container_contract::CreateTicket>>>,
    pub(crate) complete: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl super::super::K8sCreateTransport for ClosedProjectedRequests {
    async fn create(
        &self,
        _request: awaken_container_contract::K8sCreateRequest,
    ) -> Result<awaken_container_contract::K8sCreateResponse, RuntimeError> {
        panic!("terminal reconstruction must not issue Create")
    }
    async fn seal_predecessor(
        &self,
        _scope: &str,
        _predecessor: &awaken_provisioning_contract::SandboxEffectFence,
        _pod: &str,
    ) -> Result<awaken_container_contract::RequestJoin, RuntimeError> {
        Ok(if self.complete.load(Ordering::SeqCst) {
            awaken_container_contract::RequestJoin::Complete(self.tickets.lock().unwrap().clone())
        } else {
            awaken_container_contract::RequestJoin::Unknown
        })
    }
}

#[tokio::test]
async fn complete_original_inline_handoff_recovers_only_its_ticket_bound_set() {
    // Cause/effect decision table: C1 original group Complete; C2 actual
    // producer declaration/results retained on the original UID-fenced Pod;
    // C3 each self member has original Created UID/nonce/digest/owner; C4 the
    // same frozen read-only inline bytes/full Pod projection; C5 original
    // member already absent; C6 unrelated closed request in the same Worker.
    // R1=C1+C2+C3+C4 => same V3 set and zero new Create/handoff writes.
    // R2=R1+C5 => retain its original UID (absence is not a sampled new UID).
    // R3=R1+C6 => same self set; no lookup/delete of the other Session.
    // R4=missing C1/C2/C3 or copied metadata+altered C4 => Unknown, never
    // complete empty. Pod gone/replaced cannot borrow the retained fixture.
    // R5 same original effect with renewed expiry uses original stamps, not
    // the caller expiry. R6 writable/unresolved inputs stay unsupported before
    // participant reads. Duplicate tickets and malformed/missing membership
    // never become a partial/empty set; metadata fields are independent causes.
    // R7 Complete with no original tickets => Unknown and zero physical reads.
    // R8 malformed 404 lacks a Kubernetes ErrorResponse: it is not true absent.
    // A bad member reply stays Unknown; a bad Pod reply retains its parse error,
    // unlike real member absence (R2) or real Pod absence (Unknown).
    // The fixture RESULTS are produced through realize/publish, not hand-made.
    use awaken_sandbox_container::ContainerRuntime as _;

    use std::collections::BTreeMap;
    use std::sync::Mutex;
    let mut source = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    let scope = "rich-inline-original";
    let id = source.realization_runtime_id(scope).unwrap();
    let mut plan = ephemeral_plan();
    plan.binds = vec![
        awaken_sandbox_container::BindPlan {
            source_ref: "inline-original".into(),
            mount_path: "/mnt/input.txt".into(),
            read_only: true,
            content: Some("original bytes".into()),
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
        awaken_sandbox_container::BindPlan {
            source_ref: "inline-original-binary".into(),
            mount_path: "/mnt/input.bin".into(),
            read_only: true,
            content: None,
            content_bytes: Some(vec![0xff, 0, 1]),
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
    ];
    let fence = awaken_provisioning_contract::SandboxEffectFence::new(
        "old-rich-effect",
        "old-worker",
        "old-boot",
        1,
        u64::MAX,
    )
    .unwrap();
    let authority = pc::test_support::EffectAuthority::new(&fence);
    let fingerprint = serde_json::from_value(json!("original-effective-spec")).unwrap();
    let attempt = SandboxCreateAttempt::new("original-rich-attempt");
    let intent = ContainerRealizationIntent::Create;
    let context =
        ContainerRealizationContext::new(scope, &fingerprint, Some(&fence), &intent, &attempt)
            .with_effect(awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &fence,
                authority: &authority,
            });
    let mut pod = source.pod_for_effect(&id, &plan, Some(&fence));
    awaken_package_image::package_realization::stamp_sandbox_release_annotations(
        &mut pod,
        scope,
        &plan.image,
    );
    source.stamp_effect_evidence(&mut pod, &context, &fingerprint);
    declare(&mut pod, &id, &plan).unwrap();
    super::super::stamp_pod_realization(&mut pod).unwrap();
    pod.metadata.namespace = Some("default".into());
    pod.metadata.uid = Some("original-rich-pod-uid".into());
    pod.metadata.resource_version = Some("7".into());
    let nonce = "0123456789abcdef0123456789abcdef";
    pod.metadata.annotations.as_mut().unwrap().insert(
        awaken_container_contract::ISOLATION_ID_ANNOTATION.into(),
        nonce.into(),
    );
    pod.metadata.annotations.as_mut().unwrap().insert(
        awaken_container_contract::REQUEST_DIGEST_ANNOTATION.into(),
        "a".repeat(64),
    );
    let pod_name = pod.metadata.name.clone().unwrap();
    let pod_path = format!("/api/v1/namespaces/default/pods/{pod_name}");
    let objects = Arc::new(Mutex::new(BTreeMap::from([(
        pod_path.clone(),
        serde_json::to_value(&pod).unwrap(),
    )])));
    let tickets = Arc::new(Mutex::new(vec![CreateTicket {
        request: CreateTicketRequest::new(
            "original-worker-group",
            "pods",
            "default",
            &pod_name,
            "a".repeat(64),
        )
        .unwrap(),
        status: TicketStatus::Finished(TicketResult::Created {
            uid: "original-rich-pod-uid".into(),
        }),
        origin: Default::default(),
        isolation_id: nonce.into(),
        isolation_pending: false,
    }]));
    let writes = Arc::new(AtomicUsize::new(0));
    let read_paths = Arc::new(Mutex::new(Vec::new()));
    let malformed_404_path = Arc::new(Mutex::new(None::<String>));
    let faults = Arc::new(Mutex::new(std::collections::VecDeque::from([
        PublicationFault::Status,
    ])));
    let put_bodies = Arc::new(Mutex::new(Vec::new()));
    let client = awaken_outbound_http::kubernetes_client_with_verified_absence(projected_client(
        objects.clone(),
        tickets.clone(),
        writes.clone(),
        read_paths.clone(),
        malformed_404_path.clone(),
        faults.clone(),
        put_bodies.clone(),
    ));
    source.clients.control = client.clone();
    let original_set = realize(
        &source,
        pod,
        &id,
        &plan,
        Some((&context, &fingerprint)),
        None,
    )
    .await
    .expect("C1 same-UID status/RV-only409 must publish the original projected result set");
    let stored = objects.lock().unwrap().clone();
    let original_tickets = tickets.lock().unwrap().clone();
    let complete = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut recovery = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    recovery.clients.control = client;
    let recovery = recovery
        .with_create_transport(Arc::new(ClosedProjectedRequests {
            tickets: tickets.clone(),
            complete: complete.clone(),
        }))
        .unwrap();
    let created_writes = writes.load(Ordering::SeqCst);
    for row in [
        "complete",
        "member-absent",
        "unrelated",
        "renewed-expiry",
        "missing-results",
        "missing-declaration",
        "malformed-results",
        "missing-ticket",
        "duplicate-ticket",
        "duplicate-pod",
        "extra-member",
        "ticket-group",
        "ticket-digest",
        "ticket-uid",
        "foreign-uid",
        "pod-uid",
        "pod-fence",
        "pod-spec",
        "owner",
        "namespace",
        "nonce",
        "deleting",
        "content",
        "pod-gone",
        "not-closed",
        "writable",
        "unresolved",
        "no-original-tickets",
        "malformed-member-404",
        "malformed-pod-404",
    ] {
        *objects.lock().unwrap() = stored.clone();
        *tickets.lock().unwrap() = original_tickets.clone();
        complete.store(true, Ordering::SeqCst);
        read_paths.lock().unwrap().clear();
        *malformed_404_path.lock().unwrap() = None;
        let member = &original_set.objects()[0];
        let mut frozen = plan.clone();
        let mut requested_fence = fence.clone();
        let member_path = format!("/api/v1/namespaces/default/configmaps/{}", member.name);
        match row {
            "member-absent" => {
                objects.lock().unwrap().remove(&member_path);
            }
            "unrelated" => {
                let mut other = original_tickets[1].clone();
                other.request.resource = "jobs".into();
                other.request.name = "another-session-job".into();
                tickets.lock().unwrap().push(other);
            }
            "renewed-expiry" => {
                requested_fence = awaken_provisioning_contract::SandboxEffectFence::new(
                    fence.operation_id(),
                    fence.owner(),
                    fence.runtime_incarnation(),
                    fence.epoch(),
                    u64::MAX - 1,
                )
                .unwrap();
            }
            "missing-declaration" => {
                objects.lock().unwrap().get_mut(&pod_path).unwrap()["metadata"]["annotations"]
                    .as_object_mut()
                    .unwrap()
                    .remove(DECLARATION);
            }
            "malformed-results" => {
                objects.lock().unwrap().get_mut(&pod_path).unwrap()["metadata"]["annotations"]
                    [RESULTS] = "not-json".into();
            }
            "missing-results" => {
                objects.lock().unwrap().get_mut(&pod_path).unwrap()["metadata"]["annotations"]
                    .as_object_mut()
                    .unwrap()
                    .remove(RESULTS);
            }
            "missing-ticket" => {
                tickets
                    .lock()
                    .unwrap()
                    .retain(|t| t.request.name != member.name);
            }
            "duplicate-pod" => {
                tickets.lock().unwrap().push(original_tickets[0].clone());
            }
            "extra-member" => {
                // Matching altered DECLARATION+RESULTS cannot borrow a copied
                // Pod digest to expand this Session's frozen member set.
                let mut extra = member.clone();
                extra.name = "foreign-configmap".into();
                extra.uid = "foreign-configmap-uid".into();
                let expanded = crate::tests::tampered_participants(&original_set, |wire| {
                    wire["objects"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::to_value(&extra).unwrap());
                });
                let declaration = expanded
                    .objects()
                    .iter()
                    .map(|v| (v.kind, v.name.clone()))
                    .collect::<Vec<_>>();
                let mut values = objects.lock().unwrap();
                let annotations =
                    &mut values.get_mut(&pod_path).unwrap()["metadata"]["annotations"];
                annotations[RESULTS] = serde_json::to_string(&expanded).unwrap().into();
                annotations[DECLARATION] = serde_json::to_string(&declaration).unwrap().into();
            }
            "duplicate-ticket" => {
                let duplicate = original_tickets
                    .iter()
                    .find(|t| t.request.name == member.name)
                    .unwrap()
                    .clone();
                tickets.lock().unwrap().push(duplicate);
            }
            "ticket-group" | "ticket-digest" | "ticket-uid" => {
                let mut tickets = tickets.lock().unwrap();
                let ticket = tickets
                    .iter_mut()
                    .find(|t| t.request.name == member.name)
                    .unwrap();
                match row {
                    "ticket-group" => ticket.request.group = "foreign-group".into(),
                    "ticket-digest" => ticket.request.digest = "c".repeat(64),
                    _ => {
                        ticket.status = TicketStatus::Finished(TicketResult::Created {
                            uid: "foreign-original-uid".into(),
                        })
                    }
                }
            }
            "pod-uid" => {
                objects.lock().unwrap().get_mut(&pod_path).unwrap()["metadata"]["uid"] =
                    "replacement-pod".into();
            }
            "pod-fence" => {
                objects.lock().unwrap().get_mut(&pod_path).unwrap()["metadata"]["annotations"]
                    [super::super::SANDBOX_EFFECT_OWNER_ANNOTATION] = "foreign-worker".into();
            }
            "pod-spec" => {
                objects.lock().unwrap().get_mut(&pod_path).unwrap()["spec"]["containers"][0]["image"] =
                    "foreign-image".into();
            }
            "owner" => {
                objects.lock().unwrap().get_mut(&member_path).unwrap()["metadata"]["ownerReferences"]
                    [0]["uid"] = "foreign-owner-pod".into();
            }
            "namespace" => {
                objects.lock().unwrap().get_mut(&member_path).unwrap()["metadata"]["namespace"] =
                    "foreign-namespace".into();
            }
            "deleting" => {
                objects.lock().unwrap().get_mut(&member_path).unwrap()["metadata"]["deletionTimestamp"] =
                    "2026-10-12T00:00:00Z".into();
            }
            "foreign-uid" => {
                objects.lock().unwrap().get_mut(&member_path).unwrap()["metadata"]["uid"] =
                    "replacement-uid".into();
            }
            "nonce" => {
                tickets
                    .lock()
                    .unwrap()
                    .iter_mut()
                    .find(|t| t.request.name == member.name)
                    .unwrap()
                    .isolation_id = "different-nonce".into();
            }
            "content" => {
                objects.lock().unwrap().get_mut(&member_path).unwrap()["data"]["content"] =
                    "tampered bytes".into();
            }
            "pod-gone" => {
                objects.lock().unwrap().remove(&pod_path);
            }
            "not-closed" => complete.store(false, Ordering::SeqCst),
            "no-original-tickets" => tickets.lock().unwrap().clear(),
            "malformed-member-404" => {
                *malformed_404_path.lock().unwrap() = Some(member_path.clone())
            }
            "malformed-pod-404" => *malformed_404_path.lock().unwrap() = Some(pod_path.clone()),
            "writable" => frozen.binds[0].read_only = false,
            "unresolved" => frozen.binds[0].content = None,
            _ => {}
        }
        let result = recovery
            .missing_handle_terminal_evidence(scope, &frozen, Some(&requested_fence))
            .await;
        if row == "malformed-pod-404" {
            // Reject a partial Status at the shared transport boundary before
            // an optional read can convert malformed evidence to absence.
            assert!(
                matches!(result, Err(RuntimeError::Backend(message)) if message.contains("invalid Kubernetes absence response")),
                "R8 bad Pod 404 is not an absence witness"
            );
            assert_eq!(
                writes.load(Ordering::SeqCst),
                created_writes,
                "R8 zero mutation"
            );
            continue;
        }
        let result = result.unwrap();
        if row == "no-original-tickets" {
            assert!(
                read_paths.lock().unwrap().is_empty(),
                "R7 no original origin means zero API"
            );
        }
        let accepted = matches!(
            row,
            "complete" | "member-absent" | "unrelated" | "renewed-expiry"
        );
        if accepted {
            assert_eq!(
                result,
                awaken_sandbox_container::MissingHandleTerminalEvidence::ExactIncarnation {
                    container_id: pod_name.clone(),
                    runtime_handle: awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                        pod_uid: "original-rich-pod-uid".into(),
                        claim_uid: None,
                        participants: Box::new(original_set.clone()),
                    },
                },
                "R1-R3/{row}"
            );
        } else {
            assert_eq!(
                result,
                awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
                "R4/{row}"
            );
        }
        assert_eq!(
            writes.load(Ordering::SeqCst),
            created_writes,
            "zero new Create/PUT/{row}"
        );
        assert!(
            !read_paths
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.contains("another-session-job")),
            "R3 no foreign participant I/O/{row}"
        );
    }
    // C1-C9 exercise the same producer results and HTTP fixture, not a second
    // broker or a GET-derived UID oracle. Reset only test fault state.
    *objects.lock().unwrap() = stored.clone();
    *tickets.lock().unwrap() = original_tickets.clone();
    *malformed_404_path.lock().unwrap() = None;
    let original_pod = serde_json::from_value(stored[&pod_path].clone()).unwrap();
    let original_ticket_count = tickets.lock().unwrap().len();
    metadata_publication_cas_matrix(
        &source,
        &original_pod,
        &original_set,
        &objects,
        (&writes, &read_paths),
        &faults,
        &put_bodies,
    )
    .await;
    metadata_owner_cas_matrix(
        &source,
        &original_pod,
        &objects,
        (&writes, &read_paths),
        &faults,
        &put_bodies,
    )
    .await;
    assert_eq!(
        tickets.lock().unwrap().len(),
        original_ticket_count,
        "C1-C9 no repeated member POST"
    );
}

type ProjectionObjects =
    Arc<std::sync::Mutex<std::collections::BTreeMap<String, serde_json::Value>>>;
type PublicationFaults = Arc<std::sync::Mutex<std::collections::VecDeque<PublicationFault>>>;
type PublicationPuts = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

#[derive(Clone)]
enum PublicationFault {
    Status,
    Uid,
    Gone,
    Attempt,
    Fence,
    Declaration,
    Members,
    Reject(u16),
    Transport,
    ResponseLost,
    ReplyUid,
    ReplyAttempt,
    ReplyResults,
    Revoke(Arc<std::sync::atomic::AtomicBool>),
    // A successful read response can change delivery authority or target
    // identity without inventing a metadata-conflict write.
    AfterGet(Arc<dyn Fn(&mut serde_json::Value) + Send + Sync>),
}

fn projected_client(
    objects: ProjectionObjects,
    tickets: Arc<std::sync::Mutex<Vec<CreateTicket>>>,
    writes: Arc<AtomicUsize>,
    read_paths: Arc<std::sync::Mutex<Vec<String>>>,
    malformed_404_path: Arc<std::sync::Mutex<Option<String>>>,
    faults: PublicationFaults,
    put_bodies: PublicationPuts,
) -> kube::Client {
    let service = tower::service_fn(move |request: Request<Body>| {
        let objects = objects.clone();
        let tickets = tickets.clone();
        let writes = writes.clone();
        let read_paths = read_paths.clone();
        let malformed_404_path = malformed_404_path.clone();
        let faults = faults.clone();
        let put_bodies = put_bodies.clone();
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            let after_get = {
                let mut pending = faults.lock().unwrap();
                if method == http::Method::GET
                    && matches!(pending.front(), Some(PublicationFault::AfterGet(_)))
                {
                    pending.pop_front()
                } else {
                    None
                }
            };
            let (code, mut body) = if method == http::Method::POST {
                writes.fetch_add(1, Ordering::SeqCst);
                let mut body: serde_json::Value =
                    serde_json::from_slice(&request.into_body().collect_bytes().await.unwrap())
                        .unwrap();
                let name = body["metadata"]["name"].as_str().unwrap().to_owned();
                let uid = format!("original-{name}-uid");
                body["metadata"]["uid"] = uid.clone().into();
                body["metadata"]["namespace"] = "default".into();
                body["metadata"]["annotations"]
                    [awaken_container_contract::ISOLATION_ID_ANNOTATION] =
                    "0123456789abcdef0123456789abcdef".into();
                body["metadata"]["annotations"]
                    [awaken_container_contract::REQUEST_DIGEST_ANNOTATION] = "b".repeat(64).into();
                let resource = path.rsplit('/').next().unwrap();
                tickets.lock().unwrap().push(CreateTicket {
                    request: CreateTicketRequest::new(
                        "original-worker-group",
                        resource,
                        "default",
                        &name,
                        "b".repeat(64),
                    )
                    .unwrap(),
                    status: TicketStatus::Finished(TicketResult::Created { uid }),
                    origin: Default::default(),
                    isolation_id: "0123456789abcdef0123456789abcdef".into(),
                    isolation_pending: false,
                });
                objects
                    .lock()
                    .unwrap()
                    .insert(format!("{path}/{name}"), body.clone());
                (201, body)
            } else if method == http::Method::PUT {
                writes.fetch_add(1, Ordering::SeqCst);
                let body: serde_json::Value =
                    serde_json::from_slice(&request.into_body().collect_bytes().await.unwrap())
                        .unwrap();
                put_bodies.lock().unwrap().push(body.clone());
                let fault = {
                    let mut pending = faults.lock().unwrap();
                    if matches!(pending.front(), Some(PublicationFault::AfterGet(_))) {
                        None
                    } else {
                        pending.pop_front()
                    }
                };
                projected_replace(&objects, &path, body, fault)?
            } else {
                assert_eq!(
                    method,
                    http::Method::GET,
                    "no cleanup mutation during reconstruction"
                );
                read_paths.lock().unwrap().push(path.clone());
                if malformed_404_path.lock().unwrap().as_deref() == Some(path.as_str()) {
                    (
                        404,
                        json!({"apiVersion":"v1","kind":"Status","code":404,"reason":"NotFound"}),
                    )
                } else {
                    match objects.lock().unwrap().get(&path).cloned() {
                        Some(body) => (200, body),
                        None => (
                            404,
                            json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":404,"reason":"NotFound","message":"original object gone"}),
                        ),
                    }
                }
            };
            if let Some(PublicationFault::AfterGet(change)) = after_get {
                change(&mut body);
            }
            Ok::<_, std::io::Error>(
                Response::builder()
                    .status(code)
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
        }
    });
    kube::Client::new(service, "default")
}

fn projected_replace(
    objects: &ProjectionObjects,
    path: &str,
    mut body: serde_json::Value,
    fault: Option<PublicationFault>,
) -> Result<(u16, serde_json::Value), std::io::Error> {
    let status = |code| {
        (
            code,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Conflict","message":"injected API refusal","code":code}),
        )
    };
    let mut objects = objects.lock().unwrap();
    if matches!(&fault, Some(PublicationFault::Transport)) {
        return Err(std::io::Error::other(
            "injected transport failure before reply",
        ));
    }
    if let Some(PublicationFault::Reject(code)) = &fault {
        return Ok(status(*code));
    }
    if matches!(&fault, Some(PublicationFault::Gone)) {
        objects.remove(path);
        return Ok(status(409));
    }
    let current = objects
        .get_mut(path)
        .expect("the fixed original Pod exists");
    let changed = matches!(
        &fault,
        Some(
            PublicationFault::Status
                | PublicationFault::Uid
                | PublicationFault::Attempt
                | PublicationFault::Fence
                | PublicationFault::Declaration
                | PublicationFault::Members
                | PublicationFault::Revoke(_)
        )
    );
    if changed {
        let version = current["metadata"]["resourceVersion"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            + 1;
        current["metadata"]["resourceVersion"] = version.to_string().into();
        current["status"] = json!({"phase":"Pending"});
        current["metadata"]["annotations"]["fixture.awaken.dev/controller"] = "kept".into();
        current["metadata"]["finalizers"] = json!(["fixture.awaken.dev/controller"]);
        match &fault {
            Some(PublicationFault::Uid) => current["metadata"]["uid"] = "replacement-pod".into(),
            Some(PublicationFault::Attempt) => {
                current["metadata"]["annotations"][SANDBOX_ATTEMPT_ANNOTATION] =
                    "other-attempt".into()
            }
            Some(PublicationFault::Fence) => {
                current["metadata"]["annotations"][super::super::SANDBOX_EFFECT_EPOCH_ANNOTATION] =
                    "2".into()
            }
            Some(PublicationFault::Declaration) => {
                current["metadata"]["annotations"][DECLARATION] = "[]".into()
            }
            Some(PublicationFault::Members) => {
                let mut members: serde_json::Value = serde_json::from_str(
                    body["metadata"]["annotations"][RESULTS].as_str().unwrap(),
                )
                .unwrap();
                members["objects"][0]["uid"] = "other-member".into();
                current["metadata"]["annotations"][RESULTS] =
                    serde_json::to_string(&members).unwrap().into();
            }
            Some(PublicationFault::Revoke(active)) => active.store(false, Ordering::SeqCst),
            _ => {}
        }
    }
    // Actual UID/RV conditional replacement. A status write alone invalidates
    // the first request; the product must GET the same UID before another PUT.
    if body["metadata"]["uid"] != current["metadata"]["uid"]
        || body["metadata"]["resourceVersion"] != current["metadata"]["resourceVersion"]
    {
        return Ok(status(409));
    }
    let version = current["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        + 1;
    body["metadata"]["resourceVersion"] = version.to_string().into();
    objects.insert(path.to_owned(), body.clone());
    match fault {
        Some(PublicationFault::ReplyUid) => body["metadata"]["uid"] = "replacement-reply".into(),
        Some(PublicationFault::ReplyAttempt) => {
            body["metadata"]["annotations"][SANDBOX_ATTEMPT_ANNOTATION] = "other-attempt".into()
        }
        Some(PublicationFault::ReplyResults) => {
            body["metadata"]["annotations"]
                .as_object_mut()
                .unwrap()
                .remove(RESULTS);
        }
        Some(PublicationFault::ResponseLost) => {
            return Err(std::io::Error::other("injected lost accepted response"));
        }
        _ => {}
    }
    Ok((200, body))
}

async fn metadata_publication_cas_matrix(
    runtime: &K8sRuntime,
    original_pod: &Pod,
    participants: &KubernetesProjectedParticipants,
    objects: &ProjectionObjects,
    io: (&Arc<AtomicUsize>, &Arc<std::sync::Mutex<Vec<String>>>),
    faults: &PublicationFaults,
    put_bodies: &PublicationPuts,
) {
    // C1 original UID + status/RV-only409 -> original results converge, preserving
    // unrelated status/metadata. C2 replaced/gone UID -> no second PUT. C3 original
    // attempt/fence/declaration/members drift -> reject, no overwrite/UID sampling.
    // C4 exact retained replay -> zero IO. C5 multiple409s keep the original
    // identity, not a new numeric budget. C6 non409/transport/response loss -> Err,
    // never success or blind POST. C7 existing callback revoked -> no next request.
    // C8 bad successful response -> reject. C9 owner-transfer oracles stay in the
    // original creation case; this matrix does not synthesize owner/permission.
    let (writes, read_paths) = io;
    let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let scenarios = [
        ("status", vec![PublicationFault::Status], true, 2, 1),
        (
            "three-status",
            vec![PublicationFault::Status; 3],
            true,
            4,
            3,
        ),
        ("uid", vec![PublicationFault::Uid], false, 1, 1),
        ("gone", vec![PublicationFault::Gone], false, 1, 1),
        ("attempt", vec![PublicationFault::Attempt], false, 1, 1),
        ("fence", vec![PublicationFault::Fence], false, 1, 1),
        (
            "declaration",
            vec![PublicationFault::Declaration],
            false,
            1,
            1,
        ),
        ("members", vec![PublicationFault::Members], false, 1, 1),
        (
            "forbidden",
            vec![PublicationFault::Reject(403)],
            false,
            1,
            0,
        ),
        ("server", vec![PublicationFault::Reject(500)], false, 1, 0),
        ("transport", vec![PublicationFault::Transport], false, 1, 0),
        (
            "response-lost",
            vec![PublicationFault::ResponseLost],
            false,
            1,
            0,
        ),
        ("reply-uid", vec![PublicationFault::ReplyUid], false, 1, 0),
        (
            "reply-attempt",
            vec![PublicationFault::ReplyAttempt],
            false,
            1,
            0,
        ),
        (
            "reply-results",
            vec![PublicationFault::ReplyResults],
            false,
            1,
            0,
        ),
        ("already", vec![], true, 0, 0),
        (
            "revoke-after-two",
            vec![
                PublicationFault::Status,
                PublicationFault::Status,
                PublicationFault::Revoke(active.clone()),
            ],
            false,
            3,
            2,
        ),
        ("denied", vec![], false, 0, 0),
    ];
    let name = original_pod.metadata.name.as_ref().unwrap();
    let path = format!("/api/v1/namespaces/default/pods/{name}");
    for (row, script, accepted, puts, gets) in scenarios {
        let mut pod = original_pod.clone();
        if row != "already" {
            pod.metadata.annotations.as_mut().unwrap().remove(RESULTS);
        }
        objects
            .lock()
            .unwrap()
            .insert(path.clone(), serde_json::to_value(&pod).unwrap());
        *faults.lock().unwrap() = script.into();
        put_bodies.lock().unwrap().clear();
        read_paths.lock().unwrap().clear();
        active.store(row != "denied", Ordering::SeqCst);
        let before_writes = writes.load(Ordering::SeqCst);
        let admit = || {
            if active.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(RuntimeError::EffectFenceExpired)
            }
        };
        let result = publish(runtime, pod, participants.pod_uid(), participants, &admit).await;
        assert_eq!(result.is_ok(), accepted, "C1-C8/{row}: {result:?}");
        assert_eq!(
            writes.load(Ordering::SeqCst) - before_writes,
            puts,
            "C1-C8 no repeated POST/{row}"
        );
        assert_eq!(
            read_paths.lock().unwrap().len(),
            gets,
            "C2/C7 exact GET boundary/{row}"
        );
        let bodies = put_bodies.lock().unwrap();
        assert_eq!(bodies.len(), puts, "C1-C8 metadata PUTs only/{row}");
        if row == "status" || row == "three-status" {
            assert_publication_preserved_original(
                row,
                &bodies,
                participants,
                &objects.lock().unwrap()[&path],
            );
        }
        if row == "forbidden" {
            assert!(
                matches!(result, Err(RuntimeError::Unauthorized(_))),
                "C6 original403"
            );
        }
        if row == "gone" {
            assert!(
                matches!(result, Err(RuntimeError::NotFound(_))),
                "C2 required Pod404"
            );
        }
    }
}

fn assert_publication_preserved_original(
    row: &str,
    bodies: &[serde_json::Value],
    participants: &KubernetesProjectedParticipants,
    stored: &serde_json::Value,
) {
    let last = bodies.last().unwrap();
    assert_eq!(last["status"]["phase"], "Pending", "C1 fresh status/{row}");
    assert_eq!(
        last["metadata"]["annotations"]["fixture.awaken.dev/controller"], "kept",
        "C1 unrelated annotation/{row}"
    );
    assert_eq!(
        last["metadata"]["finalizers"],
        json!(["fixture.awaken.dev/controller"]),
        "C1 unrelated finalizer/{row}"
    );
    assert_eq!(
        last["metadata"]["uid"],
        participants.pod_uid(),
        "C1 exact original UID/{row}"
    );
    let written: Pod = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(
        retained(&written).unwrap().as_ref(),
        Some(participants),
        "C1 original member UID set/{row}"
    );
}

async fn metadata_owner_cas_matrix(
    runtime: &K8sRuntime,
    original_pod: &Pod,
    objects: &ProjectionObjects,
    io: (&Arc<AtomicUsize>, &Arc<std::sync::Mutex<Vec<String>>>),
    faults: &PublicationFaults,
    put_bodies: &PublicationPuts,
) {
    // C9 retains the old four-argument owner interface: no effect/expiry
    // callback is invented. Same UID + status409 converges; a new UID, effect,
    // or original member rejects before a second PUT. Same owner costs zero IO.
    // Cancellation is the caller's boundary; the real admitted publisher rows
    // above independently prove revocation, not this originally unfenced API.
    let (writes, read_paths) = io;
    let path = format!(
        "/api/v1/namespaces/default/pods/{}",
        original_pod.metadata.name.as_ref().unwrap()
    );
    for (row, script, accepted, puts, gets) in [
        ("owner-status", vec![PublicationFault::Status], true, 2, 1),
        ("owner-uid", vec![PublicationFault::Uid], false, 1, 1),
        (
            "owner-attempt",
            vec![PublicationFault::Attempt],
            false,
            1,
            1,
        ),
        ("owner-fence", vec![PublicationFault::Fence], false, 1, 1),
        (
            "owner-members",
            vec![PublicationFault::Members],
            false,
            1,
            1,
        ),
        ("owner-already", vec![], true, 0, 0),
    ] {
        let mut pod = original_pod.clone();
        pod.metadata.labels.as_mut().unwrap().insert(
            awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.into(),
            if row == "owner-already" {
                "new-owner"
            } else {
                "old-owner"
            }
            .into(),
        );
        objects
            .lock()
            .unwrap()
            .insert(path.clone(), serde_json::to_value(&pod).unwrap());
        *faults.lock().unwrap() = script.into();
        put_bodies.lock().unwrap().clear();
        read_paths.lock().unwrap().clear();
        let before = writes.load(Ordering::SeqCst);
        let result = super::super::participants::metadata::transfer_runtime_owner(
            &runtime.pods(),
            pod,
            original_pod.metadata.uid.as_ref().unwrap(),
            "new-owner",
            &awaken_worker_fleet_kubernetes::pod_metadata::unfenced_checkpoint,
        )
        .await;
        assert_eq!(result.is_ok(), accepted, "C9/{row}: {result:?}");
        assert_eq!(
            writes.load(Ordering::SeqCst) - before,
            puts,
            "C9 no POST/{row}"
        );
        assert_eq!(put_bodies.lock().unwrap().len(), puts, "C9 PUT/{row}");
        assert_eq!(read_paths.lock().unwrap().len(), gets, "C9 GET/{row}");
        if let Ok(written) = result {
            assert_eq!(
                written.metadata.uid, original_pod.metadata.uid,
                "C9 fixed UID/{row}"
            );
            assert_eq!(
                written.metadata.labels.as_ref().unwrap()
                    [awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL],
                "new-owner",
                "C9 owner/{row}"
            );
            assert_eq!(
                retained(&written).unwrap(),
                retained(original_pod).unwrap(),
                "C9 unchanged original members/{row}"
            );
        }
    }
}

mod restore_recovery;
