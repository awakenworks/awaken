// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Original projected-object results travel through the exact Pod CAS to the existing
// @formal reason: provider handle. Missing membership/UIDs never authorize absence; physical API
// @formal reason: progress is qualified by adapter tests, not by a new completion state.
//! One original projected-object projection shared by creation and cleanup.

use super::error::{api_not_found, backend};
use super::names::{configmap_name, credential_secret_name};
use super::pod_projection::{
    build_configmap, build_credential_secret, content_binds, credential_binds, credential_key,
};
use super::realization::stamp_realization;
use super::{
    K8sRuntime, SANDBOX_ATTEMPT_ANNOTATION, pod_effect_fence, pod_owner_reference,
    projected_content_delete_preconditions, verify_projected_content,
};
use super::{lifecycle, projection_verification, realization};
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::{
    BindPlan, ContainerPlan, ContainerRealizationContext, RuntimeError,
};
use awaken_sandbox_container::{
    KubernetesProjectedKind as Kind, KubernetesProjectedObject, KubernetesProjectedParticipants,
};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Resource};

pub(super) const DECLARATION: &str = "awaken.dev/projected-object-declaration";
const RESULTS: &str = "awaken.dev/projected-object-results";

enum DesiredObject {
    ConfigMap(ConfigMap),
    Secret(Secret),
}

fn projected_binds(plan: &ContainerPlan) -> Vec<(Kind, usize, &BindPlan)> {
    content_binds(plan)
        .into_iter()
        .enumerate()
        .filter(|(_, bind)| !awaken_sandbox_container::engine::manages_live_input(bind))
        .map(|(index, bind)| (Kind::ConfigMap, index, bind))
        .chain(
            credential_binds(plan)
                .into_iter()
                .enumerate()
                .map(|(index, bind)| (Kind::Secret, index, bind)),
        )
        .collect()
}

/// Before materialization, an unresolved reference has no content yet. Only
/// this closed inline shape can make object enumeration complete evidence.
pub(super) fn self_contained_inline_projection(plan: &ContainerPlan) -> bool {
    plan.memory_mounts.is_empty()
        && plan.binds.iter().all(|bind| {
            bind.read_only
                && !bind.secret_writeback
                && bind.secret_content.is_none()
                && (bind.content.is_some() != bind.content_bytes.is_some())
        })
}

/// Normalize only published legacy identity or revalidate this same derived
/// carrier. The original builder owns membership; no live API read supplies it.
pub(super) fn terminal_continuation(
    runtime: &K8sRuntime,
    plan: &ContainerPlan,
    expected_adoption: &pc::SandboxRealizationFingerprint,
    handle: &awaken_sandbox_container::DecodedContainerHandle,
) -> Result<Option<awaken_sandbox_container::ContainerContinuationHandle>, RuntimeError> {
    use awaken_sandbox_container::ContainerContinuationHandle::{
        KubernetesContinuationV2, KubernetesContinuationV3,
    };
    let (pod_uid, claim_uid, derived) = match &handle.payload.runtime_handle {
        Some(KubernetesContinuationV2 { pod_uid, claim_uid }) => (pod_uid, claim_uid, false),
        Some(KubernetesContinuationV3 {
            pod_uid,
            claim_uid,
            participants,
        }) if matches!(
            participants.origin(),
            awaken_sandbox_container::KubernetesProjectedOrigin::FrozenPlanEmpty { .. }
        ) =>
        {
            if participants.namespace() != runtime.namespace {
                return Err(backend(
                    "derived Kubernetes empty projection belongs to another namespace",
                ));
            }
            (pod_uid, claim_uid, true)
        }
        _ => return Ok(None),
    };
    let (Some(adoption), Some(realization)) = (
        &handle.adoption_fingerprint,
        &handle.realization_fingerprint,
    ) else {
        return Ok(None);
    };
    if adoption != expected_adoption
        || !self_contained_inline_projection(plan)
        || !projected_binds(plan).is_empty()
    {
        return if derived {
            Err(backend(
                "derived Kubernetes empty projection differs from its frozen plan",
            ))
        } else {
            Ok(None)
        };
    }
    let participants = KubernetesProjectedParticipants::frozen_plan_empty(
        runtime.namespace.clone(),
        pod_uid.clone(),
        adoption.clone(),
        realization.clone(),
    )
    .map_err(backend)?;
    Ok(Some(KubernetesContinuationV3 {
        pod_uid: pod_uid.clone(),
        claim_uid: claim_uid.clone(),
        participants: Box::new(participants),
    }))
}

/// Project only this observer's original API-membership obligation. The generic
/// Pod/PVC observer and exact UID disposer retain their separate authorities.
pub(super) async fn terminal_observation(
    runtime: &K8sRuntime,
    expectation: awaken_sandbox_container::ContainerObservationExpectation<'_>,
    expected_pod_uid: Option<&str>,
    pod: Option<&Pod>,
    observation: pc::SandboxObservation,
) -> Result<pc::SandboxObservation, RuntimeError> {
    if expectation.purpose != awaken_sandbox_container::ContainerObservationPurpose::TerminalCleanup
    {
        return Ok(observation);
    }
    let participants = expectation
        .runtime_handle
        .and_then(awaken_sandbox_container::ContainerContinuationHandle::projected_participants)
        .ok_or_else(|| backend("terminal Kubernetes projected-object membership is unknown"))?;
    if matches!(
        participants.origin(),
        awaken_sandbox_container::KubernetesProjectedOrigin::FrozenPlanEmpty { .. }
    ) {
        validate_bound(runtime, Some(participants), expected_pod_uid, pod, true)?;
    }
    let absent = projected_absent(runtime, participants, &|| Ok(())).await?;
    if !absent
        && let pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: Some(physical_incarnation),
        } = observation
    {
        return Ok(pc::SandboxObservation::Disposing {
            physical_incarnation,
        });
    }
    Ok(observation)
}

fn desired_objects(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
) -> Vec<DesiredObject> {
    projected_binds(plan)
        .into_iter()
        .map(|(kind, index, bind)| match kind {
            Kind::ConfigMap => DesiredObject::ConfigMap(build_configmap(
                id,
                index,
                bind.content.as_deref(),
                bind.content_bytes.as_deref(),
                owner,
            )),
            Kind::Secret => DesiredObject::Secret(build_credential_secret(
                id,
                index,
                credential_key(bind),
                bind.secret_content
                    .as_ref()
                    .expect("credential bind has secret bytes")
                    .expose(),
                owner,
            )),
        })
        .collect()
}

fn names(id: &str, plan: &ContainerPlan) -> Vec<(Kind, String)> {
    let mut names = projected_binds(plan)
        .into_iter()
        .map(|(kind, index, _)| {
            (
                kind,
                match kind {
                    Kind::ConfigMap => configmap_name(id, index),
                    Kind::Secret => credential_secret_name(id, index),
                },
            )
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub(super) fn owner_fingerprint(metadata: &ObjectMeta) -> Result<String, RuntimeError> {
    Ok(
        blake3::hash(&serde_json::to_vec(&metadata.owner_references).map_err(backend)?)
            .to_hex()
            .to_string(),
    )
}

pub(super) fn declare(pod: &mut Pod, id: &str, plan: &ContainerPlan) -> Result<(), RuntimeError> {
    pod.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(
            DECLARATION.into(),
            serde_json::to_string(&names(id, plan)).map_err(backend)?,
        );
    Ok(())
}

fn declaration(pod: &Pod) -> Result<Vec<(Kind, String)>, RuntimeError> {
    let encoded = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get(DECLARATION))
        .ok_or_else(|| backend("original Kubernetes projected-object membership is unknown"))?;
    serde_json::from_str(encoded).map_err(backend)
}

pub(super) fn retained(pod: &Pod) -> Result<Option<KubernetesProjectedParticipants>, RuntimeError> {
    let Some(encoded) = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get(RESULTS))
    else {
        return Ok(None);
    };
    let participants: KubernetesProjectedParticipants =
        serde_json::from_str(encoded).map_err(backend)?;
    let expected = declaration(pod)?;
    let actual = participants
        .objects()
        .iter()
        .map(|object| (object.kind, object.name.clone()))
        .collect::<Vec<_>>();
    if expected != actual
        || pod.metadata.uid.as_deref() != Some(participants.pod_uid())
        || participants.attempt()
            != pod
                .metadata
                .annotations
                .as_ref()
                .and_then(|values| values.get(SANDBOX_ATTEMPT_ANNOTATION))
                .map(String::as_str)
        || participants.restoration().map(|(evidence, _)| evidence)
            != realization::restoration_evidence(pod)?.as_ref()
        || participants.restoration().map(|(_, plan)| plan)
            != realization::restoration_plan_fingerprint(pod)
    {
        return Err(backend(
            "Kubernetes original projected-object result set differs from its declaration",
        ));
    }
    Ok(Some(participants))
}

async fn original_result<K>(
    api: &Api<K>,
    desired: &K,
) -> Result<KubernetesProjectedObject, RuntimeError>
where
    K: Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Resource<DynamicType = ()>
        + serde::Serialize,
{
    let outcome = realization::create_or_verify_with_status(api, desired).await?;
    // Broker replay returns the original ticket-bound object as a successful
    // response. A bare 409+GET has no original UID witness and cannot upgrade a
    // historical/partial set, even if the current object's digest matches.
    if !outcome.created {
        return Err(backend(
            "projected Create has no original ticket-bound UID result",
        ));
    }
    realization::verify_realization(desired, &outcome.object, None)?;
    if desired.meta().name != outcome.object.meta().name
        || desired.meta().owner_references != outcome.object.meta().owner_references
    {
        return Err(backend(
            "original projected Create changed its name or owner",
        ));
    }
    if let Some(attempt) = desired
        .meta()
        .annotations
        .as_ref()
        .and_then(|values| values.get(SANDBOX_ATTEMPT_ANNOTATION))
    {
        let owners = desired
            .meta()
            .owner_references
            .as_deref()
            .ok_or_else(|| backend("original projected object has no Pod owner"))?;
        let [owner] = owners else {
            return Err(backend("original projected object has multiple owners"));
        };
        verify_projected_content(outcome.object.meta(), &owner.name, &owner.uid, attempt)?;
    } else if let Some(evidence) = realization::restoration_evidence(desired)? {
        realization::verify_restoration(&outcome.object, &evidence)?;
        if realization::restoration_plan_fingerprint(desired)
            != realization::restoration_plan_fingerprint(&outcome.object)
        {
            return Err(backend(
                "original restored projected object changed its plan",
            ));
        }
    }
    let uid = outcome
        .object
        .meta()
        .uid
        .clone()
        .ok_or_else(|| backend("original projected Create has no UID"))?;
    let kind = match K::kind(&()).as_ref() {
        "ConfigMap" => Kind::ConfigMap,
        "Secret" => Kind::Secret,
        _ => return Err(backend("unsupported projected object kind")),
    };
    Ok(KubernetesProjectedObject {
        kind,
        name: desired.meta().name.clone().expect("builder names object"),
        uid,
        owner_references_fingerprint: owner_fingerprint(outcome.object.meta())?,
    })
}

pub(super) async fn realize(
    runtime: &K8sRuntime,
    pod: Pod,
    id: &str,
    plan: &ContainerPlan,
    context: Option<(
        &ContainerRealizationContext<'_>,
        &pc::SandboxRealizationFingerprint,
    )>,
    restoration: Option<(&pc::SandboxRestorationEvidence, &str)>,
) -> Result<KubernetesProjectedParticipants, RuntimeError> {
    if let Some((context, _)) = context {
        context.admit_current_effect().map_err(backend)?;
    }
    let expected = names(id, plan);
    if declaration(&pod)? != expected {
        return Err(backend(
            "original projected-object declaration differs from the frozen plan",
        ));
    }
    if let Some(participants) = retained(&pod)? {
        if participants.namespace() != runtime.namespace {
            return Err(backend("original projected-object namespace changed"));
        }
        return Ok(participants);
    }
    let pod_uid = pod
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| backend("projected-object handoff has no Pod UID"))?
        .to_owned();
    let owner = if context.is_some() {
        Some(pod_owner_reference(&pod, &pod_uid)?)
    } else {
        runtime.owner.clone()
    };
    let admit = || {
        if let Some((context, _)) = context {
            context.admit_current_effect().map_err(backend)?;
        }
        Ok(())
    };
    let objects = create_objects(runtime, id, plan, &owner, context, restoration, &admit).await?;
    let participants = KubernetesProjectedParticipants::new(
        runtime.namespace.clone(),
        pod_uid.clone(),
        objects,
        context.map(|(context, _)| context.attempt.as_str().to_owned()),
        restoration.map(|(evidence, plan)| (evidence.clone(), plan.to_owned())),
    )
    .map_err(backend)?;
    publish(runtime, pod, &pod_uid, &participants, &admit).await?;
    Ok(participants)
}

#[cfg(test)]
pub(super) mod tests;

fn stamp_desired<K: Resource<DynamicType = ()> + serde::Serialize>(
    runtime: &K8sRuntime,
    object: &mut K,
    context: Option<(
        &ContainerRealizationContext<'_>,
        &pc::SandboxRealizationFingerprint,
    )>,
    restoration: Option<(&pc::SandboxRestorationEvidence, &str)>,
) -> Result<(), RuntimeError> {
    if let Some((context, fingerprint)) = context {
        runtime.stamp_effect_evidence(object, context, fingerprint);
    }
    if let Some((evidence, fingerprint)) = restoration {
        realization::stamp_restoration(object, evidence);
        realization::stamp_restoration_plan(object, fingerprint);
    }
    stamp_realization(object)
}

/// One read-only membership/content verifier over the producer's same object
/// projection. Complete Worker request closure is required but grants no other
/// Session's resources. No current GET may supply an original UID.
pub(super) async fn verify_joined_original(
    runtime: &K8sRuntime,
    pod: &Pod,
    plan: &ContainerPlan,
    original_pod: &super::CreateTicket,
    tickets: &[super::CreateTicket],
    projected: &KubernetesProjectedParticipants,
    original: (
        &ContainerRealizationContext<'_>,
        &pc::SandboxRealizationFingerprint,
    ),
) -> Result<(), RuntimeError> {
    use super::create_ledger::{
        ISOLATION_ID_ANNOTATION, REQUEST_DIGEST_ANNOTATION, original_request_metadata_matches,
    };
    if original_pod.request.validate().is_err()
        || projected.namespace() != runtime.namespace
        || projected.restoration().is_some()
        || projected.terminal_only()
        || projected.pod_uid() != pod.metadata.uid.as_deref().unwrap_or("")
    {
        return Err(backend(
            "original joined projection has another namespace, Pod or origin",
        ));
    }
    let metadata_matches = |ticket: &super::CreateTicket, metadata: &ObjectMeta| {
        !ticket.isolation_id.is_empty()
            && original_request_metadata_matches(
                &ticket.request,
                &ticket.isolation_id,
                metadata.name.as_deref(),
                metadata.namespace.as_deref(),
                metadata
                    .annotations
                    .as_ref()
                    .and_then(|v| v.get(ISOLATION_ID_ANNOTATION))
                    .map(String::as_str),
                metadata
                    .annotations
                    .as_ref()
                    .and_then(|v| v.get(REQUEST_DIGEST_ANNOTATION))
                    .map(String::as_str),
                metadata.deletion_timestamp.is_none(),
            )
    };
    if !metadata_matches(original_pod, &pod.metadata) {
        return Err(backend("original Pod ticket metadata differs"));
    }
    let owner = Some(pod_owner_reference(pod, projected.pod_uid())?);
    let expected_owner = ObjectMeta {
        owner_references: owner.clone().map(|v| vec![v]),
        ..Default::default()
    };
    let owner_fingerprint = owner_fingerprint(&expected_owner)?;
    let id = runtime.realization_runtime_id(original.0.scope)?;
    if declaration(pod)? != names(&id, plan) {
        return Err(backend("original membership differs from the frozen plan"));
    }
    for mut desired in desired_objects(&id, plan, &owner) {
        let (kind, name) = match &desired {
            DesiredObject::ConfigMap(v) => {
                (Kind::ConfigMap, v.metadata.name.clone().unwrap_or_default())
            }
            DesiredObject::Secret(v) => (Kind::Secret, v.metadata.name.clone().unwrap_or_default()),
        };
        let resource = match kind {
            Kind::ConfigMap => "configmaps",
            Kind::Secret => "secrets",
        };
        let object = projected
            .objects()
            .iter()
            .find(|v| v.kind == kind && v.name == name)
            .ok_or_else(|| backend("original projected member is missing"))?;
        let mut matching = tickets.iter().filter(|t| {
            t.request.group == original_pod.request.group
                && t.request.resource == resource
                && t.request.namespace == projected.namespace()
                && t.request.name == name
        });
        let ticket = matching
            .next()
            .ok_or_else(|| backend("original projected ticket is missing"))?;
        if matching.next().is_some()
            || ticket.isolation_pending
            || ticket.isolation_id.is_empty()
            || ticket.request.validate().is_err()
            || !matches!(&ticket.status, super::TicketStatus::Finished(super::TicketResult::Created { uid }) if uid == &object.uid)
            || object.owner_references_fingerprint != owner_fingerprint
        {
            return Err(backend("original projected ticket UID or owner differs"));
        }
        macro_rules! verify {
            ($desired:expr, $api:expr) => {{
                stamp_desired(runtime, $desired, Some(original), None)?;
                let observed = crate::get_optional(&$api, &name).await?;
                if let Some(observed) = observed {
                    if observed.metadata.uid.as_deref() != Some(object.uid.as_str())
                        || !metadata_matches(ticket, &observed.metadata)
                        || self::owner_fingerprint(&observed.metadata)?
                            != object.owner_references_fingerprint
                    {
                        return Err(backend("original projected object identity differs"));
                    }
                    realization::verify_realization($desired, &observed, None)?;
                    // The annotations bind the original request, but copying
                    // those annotations cannot hide altered immutable bytes.
                    // Values stay local and never enter an error or log.
                    let expected_projection = serde_json::to_value(&*$desired).map_err(backend)?;
                    let observed_projection = serde_json::to_value(&observed).map_err(backend)?;
                    if ["data", "binaryData", "stringData", "immutable", "type"]
                        .into_iter()
                        .any(|field| {
                            expected_projection.get(field) != observed_projection.get(field)
                        })
                    {
                        return Err(backend(
                            "original projected content differs from the frozen plan",
                        ));
                    }
                    verify_projected_content(
                        &observed.metadata,
                        pod.metadata.name.as_deref().unwrap_or(""),
                        projected.pod_uid(),
                        original.0.attempt.as_str(),
                    )?;
                }
            }};
        }
        match &mut desired {
            DesiredObject::ConfigMap(v) => verify!(v, runtime.configmaps()),
            DesiredObject::Secret(v) => verify!(v, runtime.secrets()),
        }
    }
    Ok(())
}

pub(super) async fn create_objects(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    context: Option<(
        &ContainerRealizationContext<'_>,
        &pc::SandboxRealizationFingerprint,
    )>,
    restoration: Option<(&pc::SandboxRestorationEvidence, &str)>,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<Vec<KubernetesProjectedObject>, RuntimeError> {
    let mut results = Vec::new();
    for object in desired_objects(id, plan, owner) {
        admit()?;
        macro_rules! create {
            ($object:ident, $api:expr) => {{
                stamp_desired(runtime, &mut $object, context, restoration)?;
                original_result(&$api, &$object).await
            }};
        }
        let result = match object {
            DesiredObject::ConfigMap(mut object) => create!(object, runtime.configmaps()),
            DesiredObject::Secret(mut object) => create!(object, runtime.secrets()),
        };
        admit()?;
        results.push(result?);
    }
    Ok(results)
}

pub(super) async fn publish(
    runtime: &K8sRuntime,
    mut pod: Pod,
    pod_uid: &str,
    participants: &KubernetesProjectedParticipants,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    if let Some(previous) = retained(&pod)? {
        return if &previous == participants {
            admit()
        } else {
            Err(backend(
                "original projected-object results cannot be overwritten",
            ))
        };
    }
    let actual = participants
        .objects()
        .iter()
        .map(|object| (object.kind, object.name.clone()))
        .collect::<Vec<_>>();
    if declaration(&pod)? != actual
        || participants.namespace() != runtime.namespace
        || pod.metadata.uid.as_deref() != Some(pod_uid)
    {
        return Err(backend(
            "projected-object handoff differs from its original declaration or Pod",
        ));
    }
    if pod
        .metadata
        .resource_version
        .as_deref()
        .is_none_or(|version| version.trim().is_empty())
    {
        return Err(backend(
            "projected-object handoff has no exact Pod resourceVersion",
        ));
    }
    let name = pod
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| backend("projected-object handoff has no Pod name"))?
        .to_owned();
    pod.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(
            RESULTS.into(),
            serde_json::to_string(&participants).map_err(backend)?,
        );
    if retained(&pod)?.as_ref() != Some(participants) {
        return Err(backend(
            "original projected-object handoff lacks its original origin",
        ));
    }
    admit()?;
    let written = runtime
        .pods()
        .replace(&name, &PostParams::default(), &pod)
        .await;
    admit()?;
    let written = written.map_err(backend)?;
    projection_verification::verify_pod_projection(&pod, &written, None)?;
    if pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get(SANDBOX_ATTEMPT_ANNOTATION))
        != written
            .metadata
            .annotations
            .as_ref()
            .and_then(|values| values.get(SANDBOX_ATTEMPT_ANNOTATION))
        || pod_effect_fence(pod.metadata.annotations.as_ref())?
            != pod_effect_fence(written.metadata.annotations.as_ref())?
    {
        return Err(backend(
            "original projected-object CAS changed its effect or attempt",
        ));
    }
    if written.metadata.uid.as_deref() != Some(pod_uid)
        || retained(&written)?.as_ref() != Some(participants)
    {
        return Err(backend(
            "projected-object handoff CAS changed its exact evidence",
        ));
    }
    Ok(())
}

async fn observe_kind<K>(
    api: &Api<K>,
    object: &KubernetesProjectedObject,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<Option<K>, RuntimeError>
where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + Resource<DynamicType = ()>,
{
    admit()?;
    let observed = api.get(&object.name).await;
    admit()?;
    match observed {
        Err(error) if api_not_found(&error) => Ok(None),
        Err(error) => Err(backend(error)),
        Ok(observed) => {
            let uid = observed
                .meta()
                .uid
                .as_deref()
                .filter(|uid| !uid.trim().is_empty())
                .ok_or_else(|| backend("projected-object observation lacks UID"))?;
            Ok((uid == object.uid).then_some(observed))
        }
    }
}

pub(super) async fn projected_absent(
    runtime: &K8sRuntime,
    participants: &KubernetesProjectedParticipants,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<bool, RuntimeError> {
    if participants.namespace() != runtime.namespace {
        return Err(backend("original projected-object namespace differs"));
    }
    let mut absent = true;
    for object in participants.objects() {
        absent &= match object.kind {
            Kind::ConfigMap => observe_kind(&runtime.configmaps(), object, admit)
                .await?
                .is_none(),
            Kind::Secret => observe_kind(&runtime.secrets(), object, admit)
                .await?
                .is_none(),
        };
    }
    Ok(absent)
}

pub(super) async fn finish_projected(
    runtime: &K8sRuntime,
    participants: &KubernetesProjectedParticipants,
    pod_name: &str,
    delete: bool,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    if participants.namespace() != runtime.namespace {
        return Err(backend("original projected-object namespace differs"));
    }
    for object in participants.objects() {
        match object.kind {
            Kind::ConfigMap => {
                finish_kind(
                    &runtime.configmaps(),
                    object,
                    participants,
                    pod_name,
                    delete,
                    admit,
                )
                .await?
            }
            Kind::Secret => {
                finish_kind(
                    &runtime.secrets(),
                    object,
                    participants,
                    pod_name,
                    delete,
                    admit,
                )
                .await?
            }
        }
    }
    admit()
}

pub(super) fn validate_bound(
    runtime: &K8sRuntime,
    projected: Option<&KubernetesProjectedParticipants>,
    pod_uid: Option<&str>,
    pod: Option<&Pod>,
    required: bool,
) -> Result<(), RuntimeError> {
    let Some(projected) = projected else {
        return if required {
            Err(backend(
                "original Kubernetes projected-object membership is unknown",
            ))
        } else {
            Ok(())
        };
    };
    if projected.namespace() != runtime.namespace || pod_uid != Some(projected.pod_uid()) {
        return Err(backend(
            "original projected-object domain differs from its durable handle",
        ));
    }
    if let Some(pod) = pod {
        if let awaken_sandbox_container::KubernetesProjectedOrigin::FrozenPlanEmpty {
            adoption,
            realization,
        } = projected.origin()
        {
            let annotations = pod.metadata.annotations.as_ref();
            let stamped = |key| {
                annotations
                    .and_then(|values| values.get(key))
                    .map(String::as_str)
            };
            if pod.metadata.uid.as_deref() != Some(projected.pod_uid())
                || !projected.objects().is_empty()
                || stamped(super::SANDBOX_ADOPTION_ANNOTATION) != Some(adoption.as_str())
                || stamped(super::SANDBOX_REALIZATION_ANNOTATION) != Some(realization.as_str())
                || (annotations.is_some_and(|values| values.contains_key(DECLARATION))
                    && !declaration(pod)?.is_empty())
                || retained(pod)?.is_some_and(|retained| !retained.objects().is_empty())
            {
                return Err(backend(
                    "original Kubernetes Pod contradicts its frozen-plan empty projection",
                ));
            }
            return Ok(());
        }
        match retained(pod)? {
            Some(retained) if &retained == projected => {}
            None if matches!(projected.origin(),awaken_sandbox_container::KubernetesProjectedOrigin::JoinedEmpty {attempt,predecessor,..} if pod.metadata.annotations.as_ref().and_then(|values|values.get(SANDBOX_ATTEMPT_ANNOTATION))==Some(attempt) && pod_effect_fence(pod.metadata.annotations.as_ref())?.as_ref().is_some_and(|observed|observed.same_effect_identity(predecessor))) =>
                {}
            _ => {
                return Err(backend(
                    "original projected-object handoff differs from its durable handle",
                ));
            }
        }
    }
    Ok(())
}

pub(super) async fn finish_cleanup(
    runtime: &K8sRuntime,
    projected: Option<&KubernetesProjectedParticipants>,
    pod_name: &str,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    if let Some(projected) = projected {
        finish_projected(
            runtime,
            projected,
            pod_name,
            projected.restoration().is_some(),
            admit,
        )
        .await?;
    }
    admit()
}

async fn finish_kind<K>(
    api: &Api<K>,
    object: &KubernetesProjectedObject,
    participants: &KubernetesProjectedParticipants,
    pod_name: &str,
    delete: bool,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError>
where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + Resource<DynamicType = ()>,
{
    if delete && let Some(observed) = observe_kind(api, object, admit).await? {
        if owner_fingerprint(observed.meta())? != object.owner_references_fingerprint {
            return Err(backend(
                "original projected object changed its original ownerReferences",
            ));
        }
        let preconditions = if let Some(attempt) = participants.attempt() {
            projected_content_delete_preconditions(
                observed.meta(),
                pod_name,
                participants.pod_uid(),
                attempt,
            )?
            .ok_or_else(|| backend("original projected object changed its Pod owner or attempt"))?
        } else {
            let evidence = participants
                .restoration()
                .expect("complete manifest has exactly one origin")
                .0;
            realization::verify_restoration(&observed, evidence)?;
            if realization::restoration_plan_fingerprint(&observed)
                != participants.restoration().map(|(_, plan)| plan)
            {
                return Err(backend(
                    "original restored projected object changed its plan",
                ));
            }
            kube::api::Preconditions {
                uid: Some(object.uid.clone()),
                resource_version: Some(
                    observed.meta().resource_version.clone().ok_or_else(|| {
                        backend("projected-object disposal lacks resourceVersion")
                    })?,
                ),
            }
        };
        admit()?;
        let deleted = api
            .delete(
                &object.name,
                &DeleteParams::default().preconditions(preconditions),
            )
            .await;
        admit()?;
        match deleted {
            Ok(_) => {}
            Err(error) if api_not_found(&error) => {}
            Err(error) => return Err(backend(error)),
        }
    }
    lifecycle::await_exact_deletion(
        api,
        &object.name,
        &object.uid,
        std::time::Duration::from_secs(30),
        "Kubernetes projected object",
        admit,
    )
    .await
}
