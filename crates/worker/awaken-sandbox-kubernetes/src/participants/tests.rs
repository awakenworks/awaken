// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Original result/response-loss adapter decisions; these mocks do not qualify physical closure.
use super::super::ephemeral_plan;
use super::*;
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
    pub(crate) tickets: Arc<std::sync::Mutex<Vec<super::super::CreateTicket>>>,
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
    ) -> Result<super::super::RequestJoin, RuntimeError> {
        Ok(if self.complete.load(Ordering::SeqCst) {
            super::super::RequestJoin::Complete(self.tickets.lock().unwrap().clone())
        } else {
            super::super::RequestJoin::Unknown
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
    use awaken_sandbox_container::{CreateTicket, CreateTicketRequest, TicketResult, TicketStatus};
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
    crate::package_realization::stamp_sandbox_release_annotations(&mut pod, scope, &plan.image);
    source.stamp_effect_evidence(&mut pod, &context, &fingerprint);
    declare(&mut pod, &id, &plan).unwrap();
    super::super::stamp_pod_realization(&mut pod).unwrap();
    pod.metadata.namespace = Some("default".into());
    pod.metadata.uid = Some("original-rich-pod-uid".into());
    pod.metadata.resource_version = Some("7".into());
    let nonce = "0123456789abcdef0123456789abcdef";
    pod.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(super::super::ISOLATION_ID_ANNOTATION.into(), nonce.into());
    pod.metadata.annotations.as_mut().unwrap().insert(
        super::super::REQUEST_DIGEST_ANNOTATION.into(),
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
    let service = tower::service_fn({
        let objects = objects.clone();
        let tickets = tickets.clone();
        let writes = writes.clone();
        let read_paths = read_paths.clone();
        let malformed_404_path = malformed_404_path.clone();
        move |request: Request<Body>| {
            let objects = objects.clone();
            let tickets = tickets.clone();
            let writes = writes.clone();
            let read_paths = read_paths.clone();
            let malformed_404_path = malformed_404_path.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let (code, body) = if method == http::Method::POST {
                    writes.fetch_add(1, Ordering::SeqCst);
                    let mut body: serde_json::Value =
                        serde_json::from_slice(&request.into_body().collect_bytes().await.unwrap())
                            .unwrap();
                    let name = body["metadata"]["name"].as_str().unwrap().to_owned();
                    let uid = format!("original-{name}-uid");
                    body["metadata"]["uid"] = uid.clone().into();
                    body["metadata"]["namespace"] = "default".into();
                    body["metadata"]["annotations"][super::super::ISOLATION_ID_ANNOTATION] =
                        nonce.into();
                    body["metadata"]["annotations"][super::super::REQUEST_DIGEST_ANNOTATION] =
                        "b".repeat(64).into();
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
                        isolation_id: nonce.into(),
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
                    objects.lock().unwrap().insert(path, body.clone());
                    (200, body)
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
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(code)
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
            }
        }
    });
    let client = awaken_outbound_http::kubernetes_client_with_verified_absence(kube::Client::new(
        service, "default",
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
    .unwrap();
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
}
