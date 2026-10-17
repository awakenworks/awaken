//! Non-Pod name isolation in the existing provider Create ledger.
//! This module shares the parent policy and exact metadata checks; it owns no
//! separate ticket, cleanup authority, or terminal receipt.

use super::{
    CreateTicket, FINALIZER, ISOLATION_ID_ANNOTATION, ISOLATION_LABEL, REQUEST_DIGEST_ANNOTATION,
    TicketResult, api_conflict, backend, delete_probe_params, deletion_denial,
    exact_original_metadata, exact_reservation_metadata, policy_ready, valid_isolation_id,
};
use awaken_sandbox_container::RuntimeError;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{ListParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client};
use serde_json::{Value, json};

fn only_keys(value: Option<&Value>, allowed: &[&str]) -> bool {
    value
        .and_then(Value::as_object)
        .is_some_and(|fields| fields.keys().all(|field| allowed.contains(&field.as_str())))
}

fn inert_container(value: &Value) -> bool {
    let required = json!({
        "name":"isolation", "image":"invalid.invalid/awaken-create-isolation:never",
        "imagePullPolicy":"Never", "command":["/bin/false"]
    });
    let defaults = json!({
        "resources":{}, "terminationMessagePath":"/dev/termination-log",
        "terminationMessagePolicy":"File"
    });
    required
        .as_object()
        .expect("fixed required fields")
        .iter()
        .all(|(key, expected)| value.get(key) == Some(expected))
        && value.as_object().is_some_and(|fields| {
            fields.iter().all(|(key, actual)| {
                required.get(key) == Some(actual) || defaults.get(key) == Some(actual)
            })
        })
}

fn inert_template_spec(value: &Value, restart: &str, affinity: bool) -> bool {
    only_keys(
        Some(value),
        &[
            "affinity",
            "automountServiceAccountToken",
            "containers",
            "dnsPolicy",
            "restartPolicy",
            "schedulerName",
            "securityContext",
            "terminationGracePeriodSeconds",
        ],
    ) && (affinity || value.get("affinity").is_none())
        && value.get("automountServiceAccountToken") == Some(&json!(false))
        && [
            ("restartPolicy", json!(restart)),
            ("dnsPolicy", json!("ClusterFirst")),
            ("schedulerName", json!("default-scheduler")),
            ("securityContext", json!({})),
            ("terminationGracePeriodSeconds", json!(30)),
        ]
        .into_iter()
        .all(|(key, expected)| value.get(key).is_none_or(|actual| actual == &expected))
        && value
            .get("containers")
            .and_then(Value::as_array)
            .is_some_and(|containers| containers.len() == 1 && inert_container(&containers[0]))
}

fn exact_daemonset_reservation(value: &Value, id: &str) -> bool {
    let terms = value.pointer("/spec/template/spec/affinity/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms");
    only_keys(
        value.pointer("/spec"),
        &["selector", "template", "updateStrategy", "revisionHistoryLimit"],
    ) && value
        .pointer("/spec/revisionHistoryLimit")
        .is_none_or(|limit| limit == 10)
        && value
            .pointer("/spec/updateStrategy")
            .is_none_or(|strategy| {
                strategy
                    == &json!({
                        "type":"RollingUpdate","rollingUpdate":{"maxSurge":0,"maxUnavailable":1}
                    })
            })
        && only_keys(value.pointer("/spec/template"), &["metadata", "spec"])
        && only_keys(
            value.pointer("/spec/template/metadata"),
            &["creationTimestamp", "labels"],
        )
        && value
            .pointer("/spec/template/metadata/creationTimestamp")
            .is_none_or(Value::is_null)
        && value
            .pointer("/spec/template/spec")
            .is_some_and(|spec| inert_template_spec(spec, "Always", true))
        && value
            .pointer("/spec/template/spec/affinity")
            .is_some_and(|affinity| only_keys(Some(affinity), &["nodeAffinity"]))
        && value
            .pointer("/spec/template/spec/affinity/nodeAffinity")
            .is_some_and(|affinity| {
                only_keys(Some(affinity), &["requiredDuringSchedulingIgnoredDuringExecution"])
            })
        && value
            .pointer("/spec/template/spec/affinity/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution")
            .is_some_and(|required| only_keys(Some(required), &["nodeSelectorTerms"]))
        && terms
            == Some(&json!([{"matchExpressions":[
                {"key":"awaken.dev/create-isolation-node","operator":"Exists"},
                {"key":"awaken.dev/create-isolation-node","operator":"DoesNotExist"}
            ]}]))
        && value.pointer("/spec/selector")
            == Some(&json!({"matchLabels":{"awaken.dev/create-isolation-child":id}}))
        && value.pointer("/spec/template/metadata/labels")
            == Some(&json!({"awaken.dev/create-isolation-child":id}))
        && ["desiredNumberScheduled", "currentNumberScheduled", "numberReady"]
            .into_iter()
            .all(|field| {
                value
                    .pointer(&format!("/status/{field}"))
                    .is_none_or(|count| count == 0 || count.is_null())
            })
}

fn other_resource(resource: &str) -> Result<ApiResource, RuntimeError> {
    let (group, kind) = match resource {
        "persistentvolumeclaims" => ("", "PersistentVolumeClaim"),
        "configmaps" => ("", "ConfigMap"),
        "secrets" => ("", "Secret"),
        "jobs" => ("batch", "Job"),
        "daemonsets" => ("apps", "DaemonSet"),
        _ => {
            return Err(backend(
                "Kubernetes Create isolation has an unsupported Kind",
            ));
        }
    };
    Ok(ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(group, "v1", kind),
        resource,
    ))
}

fn other_reservation(ticket: &CreateTicket) -> Result<(ApiResource, DynamicObject), RuntimeError> {
    if !valid_isolation_id(&ticket.isolation_id) {
        return Err(backend(
            "Kubernetes Create isolation lacks an admitted identity",
        ));
    }
    let resource = other_resource(&ticket.request.resource)?;
    let id = ticket.isolation_id.as_str();
    let body = match ticket.request.resource.as_str() {
        "persistentvolumeclaims" => json!({"spec":{
            "accessModes":["ReadWriteOnce"], "storageClassName":"",
            "selector":{"matchLabels":{"awaken.dev/create-isolation-pv":id}},
            "resources":{"requests":{"storage":"1Mi"}}
        }}),
        "configmaps" => json!({"immutable":true,"data":{"awaken-isolation":id}}),
        "secrets" => json!({"immutable":true,"type":"Opaque"}),
        "jobs" => json!({"spec":{
            "suspend":true,"parallelism":0,
            "template":{"metadata":{"labels":{"awaken.dev/create-isolation-child":id}},
                "spec":{"restartPolicy":"Never","automountServiceAccountToken":false,
                    "containers":[{"name":"isolation","image":"invalid.invalid/awaken-create-isolation:never",
                        "imagePullPolicy":"Never","command":["/bin/false"]}]}}
        }}),
        "daemonsets" => json!({"spec":{
            "selector":{"matchLabels":{"awaken.dev/create-isolation-child":id}},
            "template":{"metadata":{"labels":{"awaken.dev/create-isolation-child":id}},
                "spec":{"automountServiceAccountToken":false,
                    "affinity":{"nodeAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":{
                        "nodeSelectorTerms":[{"matchExpressions":[
                            {"key":"awaken.dev/create-isolation-node","operator":"Exists"},
                            {"key":"awaken.dev/create-isolation-node","operator":"DoesNotExist"}
                        ]}]
                    }}},
                    "containers":[{"name":"isolation","image":"invalid.invalid/awaken-create-isolation:never",
                        "imagePullPolicy":"Never","command":["/bin/false"]}]}}
        }}),
        _ => unreachable!("checked above"),
    };
    let mut object = json!({
        "apiVersion":resource.api_version,
        "kind":resource.kind,
        "metadata":{
            "namespace":ticket.request.namespace,
            "name":ticket.request.name,
            "labels":{(ISOLATION_LABEL):"true"},
            "annotations":{
                (ISOLATION_ID_ANNOTATION):ticket.isolation_id,
                (REQUEST_DIGEST_ANNOTATION):ticket.request.digest
            },
            "finalizers":[FINALIZER]
        }
    });
    object
        .as_object_mut()
        .expect("fixed reservation is an object")
        .extend(
            body.as_object()
                .expect("fixed Kind body is an object")
                .clone(),
        );
    serde_json::from_value(object)
        .map(|object| (resource, object))
        .map_err(backend)
}

fn exact_other_reservation(
    ticket: &CreateTicket,
    observed: &DynamicObject,
    uid: Option<&str>,
) -> bool {
    if !exact_reservation_metadata(ticket, &observed.metadata, uid) {
        return false;
    }
    let Ok(value) = serde_json::to_value(observed) else {
        return false;
    };
    let id = ticket.isolation_id.as_str();
    let annotations = value.pointer("/metadata/annotations");
    let expected_annotations = json!({
        (ISOLATION_ID_ANNOTATION):id,
        (REQUEST_DIGEST_ANNOTATION):ticket.request.digest,
    });
    // Kubernetes may report the first DaemonSet template generation alongside
    // the protected Create result. The admission policy forbids later UPDATE;
    // no arbitrary annotation or later generation is admitted.
    let daemonset_generation_annotations = json!({
        (ISOLATION_ID_ANNOTATION):id,
        (REQUEST_DIGEST_ANNOTATION):ticket.request.digest,
        "deprecated.daemonset.template.generation":"1",
    });
    if value.pointer("/metadata/labels") != Some(&json!({(ISOLATION_LABEL):"true"}))
        || (annotations != Some(&expected_annotations)
            && !(ticket.request.resource == "daemonsets"
                && annotations == Some(&daemonset_generation_annotations)))
    {
        return false;
    }
    match ticket.request.resource.as_str() {
        "persistentvolumeclaims" => {
            only_keys(
                value.pointer("/spec"),
                &[
                    "accessModes",
                    "storageClassName",
                    "selector",
                    "resources",
                    "volumeMode",
                    "volumeName",
                ],
            ) && value.pointer("/spec/accessModes") == Some(&json!(["ReadWriteOnce"]))
                && value.pointer("/spec/storageClassName") == Some(&json!(""))
                && value.pointer("/spec/selector")
                    == Some(&json!({"matchLabels":{
                        "awaken.dev/create-isolation-pv":id
                    }}))
                && value.pointer("/spec/resources") == Some(&json!({"requests":{"storage":"1Mi"}}))
                && value
                    .pointer("/spec/volumeMode")
                    .is_none_or(|mode| mode == "Filesystem")
                && value
                    .pointer("/spec/volumeName")
                    .is_none_or(|name| name.is_null() || name == "")
                && value
                    .pointer("/status/phase")
                    .is_none_or(|phase| phase == "Pending" || phase.is_null())
        }
        "configmaps" => {
            value.pointer("/immutable") == Some(&json!(true))
                && value.pointer("/data") == Some(&json!({"awaken-isolation":id}))
                && value
                    .pointer("/binaryData")
                    .is_none_or(|binary| binary.is_null() || binary == &json!({}))
        }
        "secrets" => {
            value.pointer("/immutable") == Some(&json!(true))
                && value.pointer("/type") == Some(&json!("Opaque"))
                && value.pointer("/data").is_none_or(|data| {
                    data.is_null() || data.as_object().is_some_and(serde_json::Map::is_empty)
                })
                && value
                    .pointer("/stringData")
                    .is_none_or(|data| data.is_null() || data == &json!({}))
        }
        "jobs" => {
            let observed_uid = observed.metadata.uid.as_deref().unwrap_or("");
            only_keys(
                value.pointer("/spec"),
                &[
                    "suspend",
                    "parallelism",
                    "template",
                    "backoffLimit",
                    "completionMode",
                    "manualSelector",
                    "podReplacementPolicy",
                    "selector",
                ],
            ) && value.pointer("/spec/suspend") == Some(&json!(true))
                && value.pointer("/spec/parallelism") == Some(&json!(0))
                && value
                    .pointer("/spec/backoffLimit")
                    .is_none_or(|limit| limit == 6)
                && value
                    .pointer("/spec/completionMode")
                    .is_none_or(|mode| mode == "NonIndexed")
                && value
                    .pointer("/spec/manualSelector")
                    .is_none_or(|manual| manual == false)
                && value
                    .pointer("/spec/podReplacementPolicy")
                    .is_none_or(|policy| policy == "TerminatingOrFailed")
                && value.pointer("/spec/selector").is_none_or(|selector| {
                    selector
                        == &json!({"matchLabels":{
                            "batch.kubernetes.io/controller-uid":observed_uid
                        }})
                })
                && ["active", "succeeded", "failed"].into_iter().all(|field| {
                    value
                        .pointer(&format!("/status/{field}"))
                        .is_none_or(|count| count == 0 || count.is_null())
                })
                && only_keys(value.pointer("/spec/template"), &["metadata", "spec"])
                && only_keys(
                    value.pointer("/spec/template/metadata"),
                    &["creationTimestamp", "labels"],
                )
                && value
                    .pointer("/spec/template/metadata/creationTimestamp")
                    .is_none_or(Value::is_null)
                && value
                    .pointer("/spec/template/metadata/labels")
                    .is_some_and(|labels| {
                        labels == &json!({"awaken.dev/create-isolation-child":id})
                            || labels
                                == &json!({
                                    "awaken.dev/create-isolation-child":id,
                                    "batch.kubernetes.io/controller-uid":observed_uid,
                                    "batch.kubernetes.io/job-name":ticket.request.name,
                                    "controller-uid":observed_uid,
                                    "job-name":ticket.request.name,
                                })
                    })
                && value
                    .pointer("/spec/template/spec")
                    .is_some_and(|spec| inert_template_spec(spec, "Never", false))
        }
        "daemonsets" => exact_daemonset_reservation(&value, id),
        _ => false,
    }
}

fn exact_other_original(ticket: &CreateTicket, observed: &DynamicObject) -> bool {
    exact_original_metadata(ticket, &observed.metadata)
        && observed
            .metadata
            .finalizers
            .as_ref()
            .is_none_or(|values| !values.iter().any(|value| value == FINALIZER))
}

async fn no_controller_children(
    client: &Client,
    ticket: &CreateTicket,
    uid: &str,
) -> Result<bool, RuntimeError> {
    if !matches!(ticket.request.resource.as_str(), "jobs" | "daemonsets") {
        return Ok(true);
    }
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ticket.request.namespace);
    let observed = pods.list(&ListParams::default()).await.map_err(backend)?;
    if observed
        .metadata
        .continue_
        .as_deref()
        .is_some_and(|token| !token.is_empty())
    {
        return Ok(false);
    }
    Ok(!observed.items.iter().any(|pod| {
        pod.metadata
            .owner_references
            .as_ref()
            .is_some_and(|owners| owners.iter().any(|owner| owner.uid == uid))
    }))
}

async fn deletion_is_denied_other(
    api: &Api<DynamicObject>,
    name: &str,
    uid: &str,
) -> Result<bool, RuntimeError> {
    deletion_denial(api.delete(name, &delete_probe_params(uid)).await)
}

async fn verify_other_reservation(
    client: &Client,
    ticket: &CreateTicket,
    uid: &str,
) -> Result<bool, RuntimeError> {
    if !policy_ready(client).await? {
        return Ok(false);
    }
    let (resource, _) = other_reservation(ticket)?;
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), &ticket.request.namespace, &resource);
    let Some(observed) = crate::get_optional(&api, &ticket.request.name).await? else {
        return Ok(false);
    };
    if !exact_other_reservation(ticket, &observed, Some(uid))
        || !no_controller_children(client, ticket, uid).await?
    {
        return Ok(false);
    }
    deletion_is_denied_other(&api, &ticket.request.name, uid).await
}

pub(super) async fn isolate_other(
    client: &Client,
    ticket: &CreateTicket,
) -> Result<Option<TicketResult>, RuntimeError> {
    if !policy_ready(client).await? {
        return Ok(None);
    }
    let (resource, desired) = other_reservation(ticket)?;
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), &ticket.request.namespace, &resource);
    let observed = match api.create(&PostParams::default(), &desired).await {
        Ok(object) => object,
        Err(error) if api_conflict(&error) => {
            let Some(object) = crate::get_optional(&api, &ticket.request.name).await? else {
                return Ok(None);
            };
            object
        }
        Err(error) => return Err(backend(error)),
    };
    let Some(uid) = observed.metadata.uid.clone() else {
        return Ok(None);
    };
    if exact_other_reservation(ticket, &observed, Some(&uid)) {
        Ok(verify_other_reservation(client, ticket, &uid)
            .await?
            .then_some(TicketResult::Isolated { uid }))
    } else if exact_other_original(ticket, &observed) {
        Ok(Some(TicketResult::Created { uid }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::PVC_PROTECTION_FINALIZER;
    use super::*;
    use awaken_sandbox_container::{CreateTicketRequest, TicketStatus};
    use http::{Method, Request, Response};
    use kube::client::Body;

    fn ticket() -> CreateTicket {
        CreateTicket {
            request: CreateTicketRequest::new(
                "group",
                "pods",
                "sandbox-ns",
                "sandbox-pod",
                "a".repeat(64),
            )
            .unwrap(),
            status: TicketStatus::InFlight,
            origin: Default::default(),
            isolation_id: "0123456789abcdef0123456789abcdef".into(),
            isolation_pending: true,
        }
    }

    fn response(code: u16, body: Value) -> Response<Body> {
        Response::builder()
            .status(code)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[test]
    fn inert_container_requires_every_original_field_and_only_exact_defaults() {
        // C1 complete inert command; C2 required field absent/changed/null;
        // C3 optional default absent/exact/foreign/null; C4 added field.
        // K1 C1 with absent/exact C3 -> accept; K2 C2 or foreign/null C3
        // or C4 -> refuse. The key set and values come from the same shape.
        let original = json!({"name":"isolation","image":"invalid.invalid/awaken-create-isolation:never",
            "imagePullPolicy":"Never","command":["/bin/false"]});
        assert!(inert_container(&original), "K1 absent defaults");
        for key in ["name", "image", "imagePullPolicy", "command"] {
            for change in [None, Some(Value::Null), Some(json!("foreign"))] {
                let mut observed = original.clone();
                let fields = observed.as_object_mut().unwrap();
                match change {
                    Some(value) => {
                        fields.insert(key.into(), value);
                    }
                    None => {
                        fields.remove(key);
                    }
                }
                assert!(!inert_container(&observed), "K2 required {key}");
            }
        }
        for (key, default) in [
            ("resources", json!({})),
            ("terminationMessagePath", json!("/dev/termination-log")),
            ("terminationMessagePolicy", json!("File")),
        ] {
            for (value, accepted) in [
                (default, true),
                (Value::Null, false),
                (json!("foreign"), false),
            ] {
                let mut observed = original.clone();
                observed[key] = value;
                assert_eq!(inert_container(&observed), accepted, "K1/K2 default {key}");
            }
        }
        let mut unknown = original;
        unknown["env"] = json!([]);
        assert!(!inert_container(&unknown), "K2 unknown field");
    }

    #[test]
    fn inert_template_defaults_keep_the_same_closed_field_law() {
        // C1 Job/DaemonSet template; C2 optional API-default field absent,
        // exact, foreign or null; C3 affinity key; C4 unknown/executable field.
        // T1 absent/exact C2 -> accept shape; T2 foreign/null -> refuse;
        // T3 C3 permitted only for DaemonSet (outer reader still verifies its
        // exact unschedulable affinity); T4 C4 -> refuse. No effect authority.
        for (restart, affinity) in [("Never", false), ("Always", true)] {
            let base = json!({"automountServiceAccountToken":false,"containers":[{
                "name":"isolation","image":"invalid.invalid/awaken-create-isolation:never",
                "imagePullPolicy":"Never","command":["/bin/false"]
            }]});
            assert!(inert_template_spec(&base, restart, affinity), "T1 absent");
            for (key, default) in [
                ("restartPolicy", json!(restart)),
                ("dnsPolicy", json!("ClusterFirst")),
                ("schedulerName", json!("default-scheduler")),
                ("securityContext", json!({})),
                ("terminationGracePeriodSeconds", json!(30)),
            ] {
                for (value, accepted) in [
                    (default, true),
                    (json!("foreign"), false),
                    (Value::Null, false),
                ] {
                    let mut observed = base.clone();
                    observed[key] = value;
                    assert_eq!(
                        inert_template_spec(&observed, restart, affinity),
                        accepted,
                        "T1/T2 {key}"
                    );
                }
            }
            for (key, accepted) in [
                ("affinity", affinity),
                ("nodeName", false),
                ("volumes", false),
            ] {
                let mut observed = base.clone();
                observed[key] = json!({});
                assert_eq!(
                    inert_template_spec(&observed, restart, affinity),
                    accepted,
                    "T3/T4 {key}"
                );
            }
        }
    }

    #[test]
    fn non_pod_reservation_requires_exact_identity_and_inert_kind_shape() {
        // Causes: C1 each non-Pod Kind has the provider's UID, nonce, digest,
        // label and finalizer; C2 one identity field is foreign or missing;
        // C3 its inert spec/status or metadata gains a binding, data-bearing,
        // executable or controller-eligible field; C4 Kubernetes adds pvc-protection to a
        // Pending claim. Effects: E1 only C1 with the exact inert shape can
        // proceed to live admission and child checks, including C4 only for
        // PVC; C5 DaemonSet reports its first template-generation annotation.
        // E2 C2/C3 or C4 on another Kind or a later C5 remains Unknown. Decision rows
        // O1=C1+inert->E1, O2=C2|C3->E2, O3=C1+C4+PVC->E1,
        // O4=C1+C4+non-PVC->E2, O5=C1+C5(first)->E1,
        // O6=C1+C5(later)->E2. Live K3d qualifies API defaulting and policy.
        for resource in [
            "persistentvolumeclaims",
            "configmaps",
            "secrets",
            "jobs",
            "daemonsets",
        ] {
            let ticket = CreateTicket {
                request: CreateTicketRequest::new(
                    "group",
                    resource,
                    "sandbox-ns",
                    "reserved",
                    "a".repeat(64),
                )
                .unwrap(),
                ..ticket()
            };
            let (_, mut observed) = other_reservation(&ticket).unwrap();
            observed.metadata.uid = Some("reserved-uid".into());
            assert!(
                exact_other_reservation(&ticket, &observed, Some("reserved-uid")),
                "O1 {resource}"
            );
            let mut protected = observed.clone();
            protected.metadata.finalizers =
                Some(vec![FINALIZER.into(), PVC_PROTECTION_FINALIZER.into()]);
            assert_eq!(
                exact_other_reservation(&ticket, &protected, Some("reserved-uid")),
                resource == "persistentvolumeclaims",
                "O3/O4 {resource}"
            );
            protected
                .metadata
                .finalizers
                .as_mut()
                .unwrap()
                .push("foreign/finalizer".into());
            assert!(
                !exact_other_reservation(&ticket, &protected, Some("reserved-uid")),
                "O2 extra finalizer {resource}"
            );
            assert!(
                !exact_other_reservation(&ticket, &observed, Some("foreign-uid")),
                "O2 UID {resource}"
            );
            let original = serde_json::to_value(&observed).unwrap();
            if resource == "daemonsets" {
                let mut first_generation = original.clone();
                first_generation["metadata"]["annotations"]["deprecated.daemonset.template.generation"] =
                    "1".into();
                let first_generation: DynamicObject =
                    serde_json::from_value(first_generation).unwrap();
                assert!(
                    exact_other_reservation(&ticket, &first_generation, Some("reserved-uid")),
                    "O5 first generation"
                );
                let mut later_generation = serde_json::to_value(&first_generation).unwrap();
                later_generation["metadata"]["annotations"]["deprecated.daemonset.template.generation"] =
                    "2".into();
                let later_generation: DynamicObject =
                    serde_json::from_value(later_generation).unwrap();
                assert!(
                    !exact_other_reservation(&ticket, &later_generation, Some("reserved-uid")),
                    "O6 later generation"
                );
            }
            let mut foreign = original.clone();
            foreign["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = "foreign".into();
            let foreign: DynamicObject = serde_json::from_value(foreign).unwrap();
            assert!(
                !exact_other_reservation(&ticket, &foreign, Some("reserved-uid")),
                "O2 nonce {resource}"
            );
            let mut extra_metadata = original.clone();
            extra_metadata["metadata"]["annotations"]["foreign/effect"] = "unexpected".into();
            let extra_metadata: DynamicObject = serde_json::from_value(extra_metadata).unwrap();
            assert!(
                !exact_other_reservation(&ticket, &extra_metadata, Some("reserved-uid")),
                "O2 extra metadata {resource}"
            );
            let mut changed = original;
            match resource {
                "persistentvolumeclaims" => changed["status"] = json!({"phase":"Bound"}),
                "configmaps" => changed["data"]["awaken-isolation"] = "foreign".into(),
                "secrets" => changed["data"] = json!({"key":"dmFsdWU="}),
                "jobs" => changed["spec"]["suspend"] = false.into(),
                "daemonsets" => changed["spec"]["template"]["spec"]["affinity"] = Value::Null,
                _ => unreachable!(),
            }
            let changed: DynamicObject = serde_json::from_value(changed).unwrap();
            assert!(
                !exact_other_reservation(&ticket, &changed, Some("reserved-uid")),
                "O2 shape {resource}"
            );
            let mut added = serde_json::to_value(&observed).unwrap();
            match resource {
                "persistentvolumeclaims" => {
                    added["spec"]["dataSource"] = json!({
                        "kind":"PersistentVolumeClaim","name":"foreign-claim"
                    })
                }
                "configmaps" => added["binaryData"] = json!({"payload":"dmFsdWU="}),
                "secrets" => added["stringData"] = json!({"token":"secret"}),
                "jobs" | "daemonsets" => {
                    added["spec"]["template"]["spec"]["initContainers"] = json!([{
                        "name":"unexpected","image":"busybox"
                    }])
                }
                _ => unreachable!(),
            }
            let added: DynamicObject = serde_json::from_value(added).unwrap();
            assert!(
                !exact_other_reservation(&ticket, &added, Some("reserved-uid")),
                "O2 added effect field {resource}"
            );
        }
    }

    #[test]
    fn non_pod_original_winner_needs_provider_stamp_but_no_unowned_guard() {
        // Causes: C1 the previously admitted original wins same-name Create
        // with exact UID/nonce/digest; C2 it carries no provider reservation
        // label/finalizer; C3 the nonce or UID is foreign. Effects: E1 C1+C2
        // may join the Created ticket, while participant cleanup remains a
        // separate Unknown; E2 C3 cannot be classified as the old original.
        // Rules W1=C1+C2->E1, W2=C3->E2. The real late-Create test owns the
        // Kubernetes same-name ordering, not this pure identity projection.
        for resource in [
            "persistentvolumeclaims",
            "configmaps",
            "secrets",
            "jobs",
            "daemonsets",
        ] {
            let ticket = CreateTicket {
                request: CreateTicketRequest::new(
                    "group",
                    resource,
                    "sandbox-ns",
                    "original",
                    "a".repeat(64),
                )
                .unwrap(),
                ..ticket()
            };
            let (_, mut original) = other_reservation(&ticket).unwrap();
            original.metadata.uid = Some("original-uid".into());
            original.metadata.labels = None;
            original.metadata.finalizers = None;
            assert!(exact_other_original(&ticket, &original), "W1 {resource}");
            original
                .metadata
                .annotations
                .as_mut()
                .unwrap()
                .insert(ISOLATION_ID_ANNOTATION.into(), "foreign".into());
            assert!(
                !exact_other_original(&ticket, &original),
                "W2 nonce {resource}"
            );
        }
    }

    #[tokio::test]
    async fn controller_child_absence_rejects_a_truncated_pod_list() {
        // Causes: C1 an inert Job/DaemonSet reservation is observed; C2 its
        // Pod LIST is complete or carries a continuation token; C3 the page
        // contains an exact owner UID child. Effects: E1 only complete and
        // child-free C2/C3 can support isolation; E2 truncated or childful
        // remains Unknown. Rules P1=C1+complete+no child->E1,
        // P2=C1+(truncated|child)->E2. A partial page is never a 404 proof.
        for (continuation, child, expected) in [
            ("", false, true),
            ("next-page", false, false),
            ("", true, false),
        ] {
            let body = json!({
                "apiVersion":"v1","kind":"PodList",
                "metadata":{"continue":continuation},
                "items": if child { vec![json!({
                    "apiVersion":"v1","kind":"Pod",
                    "metadata":{"name":"child","namespace":"sandbox-ns",
                        "ownerReferences":[{"apiVersion":"batch/v1","kind":"Job",
                            "name":"reserved","uid":"reserved-uid"}]}
                })] } else { Vec::new() }
            });
            let service = tower::service_fn(move |request: Request<Body>| {
                let body = body.clone();
                async move {
                    assert_eq!(request.method(), Method::GET);
                    Ok::<_, std::io::Error>(response(200, body))
                }
            });
            let client = Client::new(service, "sandbox-ns");
            let ticket = CreateTicket {
                request: CreateTicketRequest::new(
                    "group",
                    "jobs",
                    "sandbox-ns",
                    "reserved",
                    "a".repeat(64),
                )
                .unwrap(),
                ..ticket()
            };
            assert_eq!(
                no_controller_children(&client, &ticket, "reserved-uid")
                    .await
                    .unwrap(),
                expected,
                "P1/P2 continuation={continuation:?} child={child}"
            );
        }
    }
}
