// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: A sealed provider ticket reserves the exact Kind/name before
// @formal reason: it can be called joined. The API denial and live-name tests
// @formal reason: qualify selected transitions, not universal K8s refinement.
//! Physical same-Kind name reservation for a Create whose response was lost.
//!
//! This is part of the existing provider ledger, not a second retry queue.
//! The reservation is deliberately retained: releasing its name before the
//! original API request joins would allow that request to publish late.

use awaken_container_contract::{
    CreateTicket, ISOLATION_ID_ANNOTATION, ISOLATION_LABEL, ORIGINAL_GUARD_FINALIZER,
    REQUEST_DIGEST_ANNOTATION, TicketResult, original_request_metadata_matches,
};
use k8s_openapi::api::admissionregistration::v1::{
    ValidatingAdmissionPolicy, ValidatingAdmissionPolicyBinding,
    ValidatingAdmissionPolicyBindingSpec, ValidatingAdmissionPolicySpec,
};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, PostParams, Preconditions};
use kube::{Api, Client};
use serde_json::json;

use super::state::valid_isolation_id;

use crate::api_defaults;
use crate::{api_conflict, backend};
use awaken_container_contract::RuntimeError;

const FINALIZER: &str = ISOLATION_LABEL;
const PVC_PROTECTION_FINALIZER: &str = "kubernetes.io/pvc-protection";
const GATE: &str = "awaken.dev/create-isolation";
pub(crate) const POLICY: &str = "awaken-create-isolation";

pub(crate) fn expected_policy_spec() -> ValidatingAdmissionPolicySpec {
    serde_json::from_value(json!({
        "failurePolicy": "Fail",
        "matchConstraints": {
            "matchPolicy": "Equivalent",
            "namespaceSelector": {},
            "objectSelector": {"matchLabels": {(ISOLATION_LABEL): "true"}},
            "resourceRules": [
                {
                    "apiGroups": [""], "apiVersions": ["v1"],
                    "operations": ["UPDATE", "DELETE"],
                    "resources": ["pods", "persistentvolumeclaims", "configmaps", "secrets"],
                    "scope": "Namespaced"
                },
                {
                    "apiGroups": ["batch"], "apiVersions": ["v1"],
                    "operations": ["UPDATE", "DELETE"], "resources": ["jobs"],
                    "scope": "Namespaced"
                },
                {
                    "apiGroups": ["apps"], "apiVersions": ["v1"],
                    "operations": ["UPDATE", "DELETE"], "resources": ["daemonsets"],
                    "scope": "Namespaced"
                }
            ]
        },
        "validations": [{"expression": "false", "message": "Awaken Create isolation is retained until the original request is joined"}]
    }))
    .expect("fixed admission policy contract is valid")
}

pub(crate) fn expected_binding_spec() -> ValidatingAdmissionPolicyBindingSpec {
    serde_json::from_value(json!({
        "policyName": POLICY,
        "validationActions": ["Deny"]
    }))
    .expect("fixed admission binding contract is valid")
}

/// Read the deployed API policy rather than inferring protection from the Pod
/// finalizer. RBAC keeps policy mutation outside both runtime SAs. The actual
/// DELETE denial is also probed on the exact UID below, closing policy-sync
/// and admission-configuration uncertainty before any terminal receipt.
async fn policy_ready(client: &Client) -> Result<bool, RuntimeError> {
    let policies: Api<ValidatingAdmissionPolicy> = Api::all(client.clone());
    let bindings: Api<ValidatingAdmissionPolicyBinding> = Api::all(client.clone());
    let Some(policy) = crate::get_optional(&policies, POLICY).await? else {
        return Ok(false);
    };
    let Some(binding) = crate::get_optional(&bindings, POLICY).await? else {
        return Ok(false);
    };
    Ok(policy.metadata.deletion_timestamp.is_none()
        && binding.metadata.deletion_timestamp.is_none()
        && policy.spec.as_ref() == Some(&expected_policy_spec())
        && binding.spec.as_ref() == Some(&expected_binding_spec())
        && policy.metadata.generation.is_some_and(|generation| {
            policy
                .status
                .as_ref()
                .and_then(|status| status.observed_generation)
                == Some(generation)
        }))
}

fn reservation(ticket: &CreateTicket) -> Result<Pod, RuntimeError> {
    if ticket.request.resource != "pods" || !valid_isolation_id(&ticket.isolation_id) {
        return Err(backend("Pod Create isolation lacks an admitted identity"));
    }
    serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {
            "namespace": ticket.request.namespace,
            "name": ticket.request.name,
            "labels": {(ISOLATION_LABEL): "true"},
            "annotations": {
                (ISOLATION_ID_ANNOTATION): ticket.isolation_id,
                (REQUEST_DIGEST_ANNOTATION): ticket.request.digest
            },
            "finalizers": [FINALIZER]
        },
        "spec": {
            "automountServiceAccountToken": false,
            "restartPolicy": "Never",
            "schedulingGates": [{"name": GATE}],
            "nodeSelector": {"awaken.dev/create-isolation-node": "never"},
            "containers": [{
                "name": "isolation", "image": "invalid.invalid/awaken-create-isolation:never",
                "imagePullPolicy": "Never", "command": ["/bin/false"],
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "capabilities": {"drop": ["ALL"]},
                    "readOnlyRootFilesystem": true,
                    "runAsNonRoot": true,
                    "runAsUser": 10001,
                    "seccompProfile": {"type": "RuntimeDefault"}
                }
            }]
        }
    }))
    .map_err(backend)
}

fn exact_reservation_metadata(
    ticket: &CreateTicket,
    metadata: &ObjectMeta,
    uid: Option<&str>,
) -> bool {
    let exact_finalizers = match metadata.finalizers.as_deref() {
        Some([provider]) if provider == FINALIZER => true,
        Some([first, second]) if ticket.request.resource == "persistentvolumeclaims" => {
            (first == FINALIZER && second == PVC_PROTECTION_FINALIZER)
                || (first == PVC_PROTECTION_FINALIZER && second == FINALIZER)
        }
        _ => false,
    };
    request_fields_match(ticket, metadata)
        && metadata.uid.as_deref().is_some_and(|actual| {
            !actual.trim().is_empty() && uid.is_none_or(|expected| actual == expected)
        })
        && metadata.owner_references.is_none()
        && exact_finalizers
        && metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(ISOLATION_LABEL))
            .map(String::as_str)
            == Some("true")
}

fn request_fields_match(ticket: &CreateTicket, metadata: &ObjectMeta) -> bool {
    original_request_metadata_matches(
        &ticket.request,
        &ticket.isolation_id,
        metadata.name.as_deref(),
        metadata.namespace.as_deref(),
        metadata
            .annotations
            .as_ref()
            .and_then(|values| values.get(ISOLATION_ID_ANNOTATION))
            .map(String::as_str),
        metadata
            .annotations
            .as_ref()
            .and_then(|values| values.get(REQUEST_DIGEST_ANNOTATION))
            .map(String::as_str),
        metadata.deletion_timestamp.is_none(),
    )
}

fn exact_original_metadata(ticket: &CreateTicket, metadata: &ObjectMeta) -> bool {
    request_fields_match(ticket, metadata)
        && metadata
            .uid
            .as_deref()
            .is_some_and(|uid| !uid.trim().is_empty())
        && metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(ISOLATION_LABEL))
            .is_none()
}

fn exact_reservation(ticket: &CreateTicket, observed: &Pod, uid: Option<&str>) -> bool {
    let Ok(expected) = reservation(ticket) else {
        return false;
    };
    let (Some(mut desired), Some(mut actual)) = (expected.spec, observed.spec.clone()) else {
        return false;
    };
    // The shared ordinary-Pod normalizer intentionally ignores scheduler
    // outputs. A reservation has no execution owner: it must remain unbound
    // and have no kubelet-reported container execution at all.
    let unbound = actual.node_name.is_none()
        && observed.status.as_ref().is_none_or(|status| {
            status
                .phase
                .as_deref()
                .is_none_or(|phase| phase == "Pending")
                && status.container_statuses.as_ref().is_none_or(Vec::is_empty)
                && status
                    .init_container_statuses
                    .as_ref()
                    .is_none_or(Vec::is_empty)
                && status
                    .ephemeral_container_statuses
                    .as_ref()
                    .is_none_or(Vec::is_empty)
        });
    api_defaults::normalize_pod_pair(&mut desired, &mut actual);
    exact_reservation_metadata(ticket, &observed.metadata, uid) && unbound && desired == actual
}

fn exact_original(ticket: &CreateTicket, observed: &Pod) -> bool {
    exact_original_metadata(ticket, &observed.metadata)
        && observed
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|finalizers| {
                finalizers
                    .iter()
                    .any(|value| value == ORIGINAL_GUARD_FINALIZER)
            })
}

fn delete_probe_params(uid: &str) -> DeleteParams {
    DeleteParams {
        dry_run: true,
        preconditions: Some(Preconditions {
            uid: Some(uid.into()),
            resource_version: None,
        }),
        ..Default::default()
    }
}

fn deletion_denial<T>(result: Result<T, kube::Error>) -> Result<bool, RuntimeError> {
    match result {
        Err(kube::Error::Api(error))
            if matches!(error.code, 403 | 422) && error.message.contains(POLICY) =>
        {
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(kube::Error::Api(error)) if matches!(error.code, 404 | 409) => Ok(false),
        Err(error) => Err(backend(error)),
    }
}

async fn deletion_is_denied(api: &Api<Pod>, name: &str, uid: &str) -> Result<bool, RuntimeError> {
    deletion_denial(api.delete(name, &delete_probe_params(uid)).await)
}

pub async fn verify_reservation(
    client: &Client,
    ticket: &CreateTicket,
    uid: &str,
) -> Result<bool, RuntimeError> {
    if !policy_ready(client).await? {
        return Ok(false);
    }
    let api: Api<Pod> = Api::namespaced(client.clone(), &ticket.request.namespace);
    let Some(pod) = crate::get_optional(&api, &ticket.request.name).await? else {
        return Ok(false);
    };
    if !exact_reservation(ticket, &pod, Some(uid)) {
        return Ok(false);
    }
    deletion_is_denied(&api, &ticket.request.name, uid).await
}

/// Race an inert reservation against the one previously admitted original
/// Create. Kubernetes's same-name uniqueness linearizes these two POSTs. A
/// conflict must be read back as this exact reservation or this exact stamped
/// original; all foreign and disappearing objects remain Unknown.
pub(crate) async fn isolate_pod(
    client: &Client,
    ticket: &CreateTicket,
) -> Result<Option<TicketResult>, RuntimeError> {
    if !policy_ready(client).await? {
        return Ok(None);
    }
    let api: Api<Pod> = Api::namespaced(client.clone(), &ticket.request.namespace);
    let desired = reservation(ticket)?;
    let observed = match api.create(&PostParams::default(), &desired).await {
        Ok(pod) => pod,
        Err(error) if api_conflict(&error) => {
            let Some(pod) = crate::get_optional(&api, &ticket.request.name).await? else {
                return Ok(None);
            };
            pod
        }
        Err(error) => return Err(backend(error)),
    };
    let Some(uid) = observed.metadata.uid.clone() else {
        return Ok(None);
    };
    if exact_reservation(ticket, &observed, Some(&uid)) {
        Ok(verify_reservation(client, ticket, &uid)
            .await?
            .then_some(TicketResult::Isolated { uid }))
    } else if exact_original(ticket, &observed) {
        Ok(Some(TicketResult::Created { uid }))
    } else {
        Ok(None)
    }
}

mod non_pod;

pub fn original_guard_release_required(
    observed: Option<&Pod>,
    release_original_guard: bool,
) -> Result<bool, RuntimeError> {
    let guarded = observed.is_some_and(|pod| {
        pod.metadata
            .finalizers
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == ORIGINAL_GUARD_FINALIZER))
    });
    if guarded && !release_original_guard {
        return Err(backend(
            "brokered original Pod needs its durable handle before guard release",
        ));
    }
    Ok(guarded)
}

pub(super) async fn isolate(
    client: &Client,
    ticket: &CreateTicket,
) -> Result<Option<TicketResult>, RuntimeError> {
    if ticket.request.resource == "pods" {
        isolate_pod(client, ticket).await
    } else {
        non_pod::isolate_other(client, ticket).await
    }
}

/// Release only the broker's original-Create guard, through the existing
/// exact-UID/resourceVersion finalizer CAS. The caller has already initiated
/// UID-fenced terminal Pod deletion under its original cleanup authority.
pub async fn release_original_guard(
    pods: &Api<Pod>,
    name: &str,
    uid: &str,
    checkpoint: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    use crate::pod_metadata::{ExactFinalizerAdmission, update_exact_pod_finalizer};
    update_exact_pod_finalizer(
        pods,
        name,
        ORIGINAL_GUARD_FINALIZER,
        true,
        checkpoint,
        &|pod| {
            if pod.metadata.uid.as_deref() != Some(uid) {
                return Err(backend("original Create guard names another Pod UID"));
            }
            if pod
                .metadata
                .finalizers
                .as_ref()
                .is_some_and(|values| values.iter().any(|value| value == ORIGINAL_GUARD_FINALIZER))
            {
                if pod.metadata.deletion_timestamp.is_none() {
                    return Err(backend(
                        "original Create guard cannot be released before exact deletion intent",
                    ));
                }
                Ok(ExactFinalizerAdmission::Remove)
            } else {
                Ok(ExactFinalizerAdmission::Complete)
            }
        },
        &|_| {},
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        IsolationApi as Fixture, isolation_api as client, isolation_ticket as ticket,
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn original_metadata_field_law_keeps_isolation_purpose_and_unknown_uid() {
        // Causes C1: original name/namespace/nonce/request digest and undeleted
        // state; C2: observed UID missing/empty/blank/other/nonblank with whitespace;
        // C3: isolation marker present; C4: historical empty nonce. Effects:
        // L1 all C1+nonblank UID+!C3 -> original metadata; L2 any changed C1,
        // missing/blank UID or C3 -> refuse. L3 spaced nonblank UID and matching
        // empty nonce preserve this reader's old semantics; neither borrows the
        // broker's exact retained UID/nonempty-nonce purpose. This pure law does
        // not admit historical isolation or turn a reservation into Created.
        for row in [
            "exact",
            "name",
            "namespace",
            "nonce",
            "digest",
            "deleting",
            "uid-missing",
            "uid-empty",
            "uid-blank",
            "uid-spaced",
            "uid-other",
            "isolation-marker",
            "nonce-empty",
        ] {
            let mut ticket = ticket();
            let mut metadata = ObjectMeta {
                name: Some(ticket.request.name.clone()),
                namespace: Some(ticket.request.namespace.clone()),
                uid: Some("original-uid".into()),
                annotations: Some(
                    [
                        (ISOLATION_ID_ANNOTATION.into(), ticket.isolation_id.clone()),
                        (
                            REQUEST_DIGEST_ANNOTATION.into(),
                            ticket.request.digest.clone(),
                        ),
                    ]
                    .into(),
                ),
                ..Default::default()
            };
            match row {
                "name" => metadata.name = Some("other".into()),
                "namespace" => metadata.namespace = Some("other".into()),
                "nonce" => {
                    metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert(ISOLATION_ID_ANNOTATION.into(), "other".into());
                }
                "digest" => {
                    metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert(REQUEST_DIGEST_ANNOTATION.into(), "b".repeat(64));
                }
                "deleting" => {
                    metadata.deletion_timestamp = Some(
                        serde_json::from_value(serde_json::json!("2026-10-12T00:00:00Z")).unwrap(),
                    )
                }
                "uid-missing" => metadata.uid = None,
                "uid-empty" => metadata.uid = Some(String::new()),
                "uid-blank" => metadata.uid = Some(" \t ".into()),
                "uid-spaced" => metadata.uid = Some(" original-uid ".into()),
                "uid-other" => metadata.uid = Some("another-original-uid".into()),
                "isolation-marker" => {
                    metadata.labels = Some([(ISOLATION_LABEL.into(), "false".into())].into())
                }
                "nonce-empty" => {
                    ticket.isolation_id.clear();
                    metadata
                        .annotations
                        .as_mut()
                        .unwrap()
                        .insert(ISOLATION_ID_ANNOTATION.into(), String::new());
                }
                _ => {}
            }
            assert_eq!(
                exact_original_metadata(&ticket, &metadata),
                matches!(row, "exact" | "uid-spaced" | "uid-other" | "nonce-empty"),
                "L1-L3/{row}"
            );
        }
        let ticket = ticket();
        let mut isolated = reservation(&ticket).unwrap();
        isolated.metadata.uid = Some("reserved-uid".into());
        assert!(exact_reservation(&ticket, &isolated, Some("reserved-uid")));
        assert!(
            !exact_original(&ticket, &isolated),
            "L2 purposes do not swap"
        );
        // L4/L5: reservation shares only the original-request field law. Its
        // optional expected UID, exact finalizers, absent owner and true marker
        // stay independent; each false axis still refuses the inert object.
        assert!(exact_reservation(&ticket, &isolated, None));
        assert!(!exact_reservation(&ticket, &isolated, Some("other-uid")));
        for row in ["finalizer", "owner", "marker", "deleting"] {
            let mut changed = isolated.clone();
            match row {
                "finalizer" => changed.metadata.finalizers = None,
                "owner" => changed.metadata.owner_references = Some(Vec::new()),
                "marker" => {
                    changed
                        .metadata
                        .labels
                        .as_mut()
                        .unwrap()
                        .insert(ISOLATION_LABEL.into(), "false".into());
                }
                "deleting" => {
                    changed.metadata.deletion_timestamp = Some(
                        serde_json::from_value(serde_json::json!("2026-10-12T00:00:00Z")).unwrap(),
                    )
                }
                _ => unreachable!(),
            }
            assert!(!exact_reservation(&ticket, &changed, None), "L5/{row}");
        }
    }

    #[test]
    fn deployment_policy_matches_the_runtime_attestation_contract() {
        // Causes: C1 the six-kind VAP/binding selector, operation or
        // enforcement drifts; C2 Product/Worker lose read-only policy access
        // or the shared runtime Role gains protected Create/deletecollection.
        // Effects: E1 C1 fails before rollout and live
        // attestation returns Unknown; E2 C2 fails the deployment contract.
        // Rules D1=!C1+!C2->pass, D2=C1|C2->fail. The policy and RBAC remain
        // deployment-owned rather than a second runtime configuration.
        let manifest =
            include_str!("../../../../../deploy/k3d/product-backend/create-control/resources.yaml");
        // Y6 the complete YAML stream keeps every policy/binding document and
        // typed JSON value; the original authority comparisons below are the
        // compatibility oracle, not a second manifest/schema implementation.
        let docs = serde_saphyr::from_multiple::<serde_json::Value>(manifest).unwrap();
        let policy = docs
            .iter()
            .find(|doc| doc["kind"] == "ValidatingAdmissionPolicy")
            .unwrap();
        let binding = docs
            .iter()
            .find(|doc| doc["kind"] == "ValidatingAdmissionPolicyBinding")
            .unwrap();
        let deployed: ValidatingAdmissionPolicySpec =
            serde_json::from_value(policy["spec"].clone()).unwrap();
        let deployed_binding: ValidatingAdmissionPolicyBindingSpec =
            serde_json::from_value(binding["spec"].clone()).unwrap();
        assert_eq!(deployed, expected_policy_spec(), "D1 policy");
        assert_eq!(deployed_binding, expected_binding_spec(), "D1 binding");
        let role = docs
            .iter()
            .find(|doc| {
                doc["kind"] == "ClusterRole"
                    && doc["metadata"]["name"] == "awaken-create-isolation-read"
            })
            .unwrap();
        let rule = &role["rules"][0];
        assert_eq!(
            rule["resourceNames"],
            json!([POLICY]),
            "D1 exact policy read"
        );
        assert_eq!(
            rule["resources"],
            json!([
                "validatingadmissionpolicies",
                "validatingadmissionpolicybindings"
            ])
        );
        assert_eq!(rule["verbs"], json!(["get"]), "D1 no policy mutation");
        let role_binding = docs
            .iter()
            .find(|doc| {
                doc["kind"] == "ClusterRoleBinding"
                    && doc["metadata"]["name"] == "awaken-create-isolation-read"
            })
            .unwrap();
        for name in ["awaken-product", "awaken-worker"] {
            assert!(
                role_binding["subjects"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|subject| subject["name"] == name
                        && subject["namespace"] == "awaken-system"),
                "D1 {name} can attest the policy"
            );
        }
        // Both Product and distributed overlays import this one RBAC base.
        // Inspect its authoritative Role rather than the old inline Product
        // manifest; overlay import/render checks live with the deployment gate.
        let runtime_manifest =
            include_str!("../../../../../deploy/k3d/bases/sandbox-runtime-rbac/resources.yaml");
        let runtime_docs =
            serde_saphyr::from_multiple::<serde_json::Value>(runtime_manifest).unwrap();
        let runtime_role = runtime_docs
            .iter()
            .find(|doc| {
                doc["kind"] == "Role" && doc["metadata"]["name"] == "awaken-runtime-read-dispose"
            })
            .unwrap();
        for rule in runtime_role["rules"].as_array().unwrap() {
            let protected = rule["resources"].as_array().unwrap().iter().any(|value| {
                matches!(
                    value.as_str(),
                    Some(
                        "pods"
                            | "persistentvolumeclaims"
                            | "configmaps"
                            | "secrets"
                            | "jobs"
                            | "daemonsets"
                    )
                )
            });
            if protected {
                assert!(
                    !rule["verbs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|verb| verb == "create" || verb == "deletecollection"),
                    "D1 no protected Kind direct Create or collection delete bypass: {rule}"
                );
            }
        }
    }

    #[tokio::test]
    async fn same_name_reservation_requires_exact_uid_shape_and_live_delete_denial() {
        // Causes: P exact policy+binding active at the observed generation;
        // N name is free, occupied by
        // this sentinel, stamped original, or foreign Pod; D UID-fenced dry
        // DELETE is denied by this policy; M retained Pod/UID/spec or
        // unscheduled execution status is mutated.
        // Effects: E1 P+N(free/sentinel)+D yields one Isolated UID and replay;
        // E2 P+N(original) yields Created for later full projection cleanup;
        // E3 !P|!D|N(foreign)|M yields no close witness. Rules I1=E1,
        // I2=E2, I3=E3. A pending reservation replays the same Pod POST;
        // Kubernetes returns the retained UID through its 409 readback.
        // The ledger CAS test separately covers the durable
        // state transition after these physical observations.
        let ticket = ticket();
        let state = Arc::new(Mutex::new(Fixture {
            policy: true,
            delete_denied: true,
            ..Default::default()
        }));
        let api = client(state.clone());
        let isolated = Some(TicketResult::Isolated {
            uid: "sentinel-uid".into(),
        });
        assert_eq!(
            isolate_pod(&api, &ticket).await.unwrap(),
            isolated,
            "I1 first POST"
        );
        assert_eq!(
            isolate_pod(&api, &ticket).await.unwrap(),
            isolated,
            "I1 lost response replay"
        );
        assert_eq!(state.lock().unwrap().creates, 2, "I1 one physical UID");
        state.lock().unwrap().denial_code = 403;
        assert!(
            verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I1 403 policy denial"
        );
        state.lock().unwrap().denial_code = 0;
        assert!(
            !verify_reservation(&api, &ticket, "foreign-uid")
                .await
                .unwrap(),
            "I3 UID"
        );
        state.lock().unwrap().delete_denied = false;
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 missing denial"
        );
        state.lock().unwrap().delete_denied = true;
        let original_spec = state.lock().unwrap().pod.as_ref().unwrap().spec.clone();
        state
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .spec
            .as_mut()
            .unwrap()
            .containers[0]
            .image = Some("foreign:latest".into());
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 changed executable"
        );
        state.lock().unwrap().pod.as_mut().unwrap().spec = original_spec.clone();
        state
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .spec
            .as_mut()
            .unwrap()
            .node_name = Some("node-a".into());
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 scheduled reservation"
        );
        state.lock().unwrap().pod.as_mut().unwrap().spec = original_spec;
        state.lock().unwrap().pod.as_mut().unwrap().status =
            Some(k8s_openapi::api::core::v1::PodStatus {
                phase: Some("Running".into()),
                ..Default::default()
            });
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 container execution status"
        );
        state.lock().unwrap().pod.as_mut().unwrap().status = None;
        state.lock().unwrap().policy = false;
        assert_eq!(
            isolate_pod(&api, &ticket).await.unwrap(),
            None,
            "I3 missing policy"
        );
        state.lock().unwrap().policy = true;
        state.lock().unwrap().policy_tampered = true;
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 changed policy"
        );
        state.lock().unwrap().policy_tampered = false;
        state.lock().unwrap().binding_tampered = true;
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 changed binding"
        );
        state.lock().unwrap().binding_tampered = false;
        state.lock().unwrap().policy_lagged = true;
        assert!(
            !verify_reservation(&api, &ticket, "sentinel-uid")
                .await
                .unwrap(),
            "I3 policy generation not observed"
        );

        let original_state = Arc::new(Mutex::new(Fixture {
            policy: true,
            delete_denied: true,
            ..Default::default()
        }));
        let mut original = reservation(&ticket).unwrap();
        original.metadata.uid = Some("original-uid".into());
        original.metadata.labels = None;
        original.metadata.finalizers = Some(vec![ORIGINAL_GUARD_FINALIZER.into()]);
        original_state.lock().unwrap().pod = Some(original);
        assert_eq!(
            isolate_pod(&client(original_state.clone()), &ticket)
                .await
                .unwrap(),
            Some(TicketResult::Created {
                uid: "original-uid".into()
            }),
            "I2"
        );
        original_state
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .metadata
            .finalizers = None;
        assert_eq!(
            isolate_pod(&client(original_state.clone()), &ticket)
                .await
                .unwrap(),
            None,
            "I3 unguarded historical Pod cannot prove original ownership"
        );
        original_state
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .metadata
            .finalizers = Some(vec![ORIGINAL_GUARD_FINALIZER.into()]);
        original_state
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .remove(ISOLATION_ID_ANNOTATION);
        assert_eq!(
            isolate_pod(&client(original_state), &ticket).await.unwrap(),
            None,
            "I3 foreign same name"
        );
    }

    #[tokio::test]
    async fn original_create_guard_releases_only_after_exact_delete_intent() {
        // Causes: C1 broker-stamped original Pod has a persisted UID and
        // finalizer; C2 terminal owner has/has not made exact deletion intent;
        // C3 observed UID matches/changes. Effects: E1 C1+!C2 rejects guard
        // removal, so a 201-before-ticket-CAS Pod cannot vanish; E2 C1+C2+
        // matching C3 removes only this guard with RV CAS; E3 foreign UID
        // rejects. Rules G1=C1+!C2->E1, G2=C1+C2+exact->E2,
        // G3=C1+C2+foreign->E3. The ordinary cleanup path owns the call.
        let mut pod = reservation(&ticket()).unwrap();
        pod.metadata.uid = Some("original-uid".into());
        pod.metadata.resource_version = Some("1".into());
        pod.metadata.finalizers = Some(vec![
            ORIGINAL_GUARD_FINALIZER.into(),
            "other/finalizer".into(),
        ]);
        let state = Arc::new(Mutex::new(Fixture {
            pod: Some(pod),
            ..Default::default()
        }));
        let api: Api<Pod> = Api::namespaced(client(state.clone()), "sandbox-ns");
        assert!(
            release_original_guard(&api, "sandbox-pod", "original-uid", &|| Ok(()))
                .await
                .is_err(),
            "G1"
        );
        state
            .lock()
            .unwrap()
            .pod
            .as_mut()
            .unwrap()
            .metadata
            .deletion_timestamp =
            Some(serde_json::from_value(json!("2026-10-05T00:00:00Z")).unwrap());
        assert!(
            release_original_guard(&api, "sandbox-pod", "foreign-uid", &|| Ok(()))
                .await
                .is_err(),
            "G3"
        );
        release_original_guard(&api, "sandbox-pod", "original-uid", &|| Ok(()))
            .await
            .unwrap();
        let finalizers = state
            .lock()
            .unwrap()
            .pod
            .as_ref()
            .unwrap()
            .metadata
            .finalizers
            .clone()
            .unwrap();
        assert_eq!(
            finalizers,
            vec!["other/finalizer"],
            "G2 only the broker guard is released"
        );
        release_original_guard(&api, "sandbox-pod", "original-uid", &|| Ok(()))
            .await
            .unwrap();
    }
}
