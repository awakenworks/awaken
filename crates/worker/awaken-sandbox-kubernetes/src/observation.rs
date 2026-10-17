// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Observation of one recorded Sandbox incarnation for recovery or terminal cleanup; a
// @formal reason: missing, stale or foreign object is reported, never adopted or deleted here.
//! Observation of one recorded Sandbox incarnation, for recovery or for
//! terminal cleanup, including the terminal-only guarded claim.

use super::ContinuationObservationDisposition;
use super::K8sRuntime;
use super::api_not_found;
use super::backend;
use super::guarded_claim::observe_guarded_claim;
use super::observed_pod;
use super::participants;
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerObservationExpectation;
use awaken_sandbox_container::ContainerObservationPurpose;
use awaken_sandbox_container::RuntimeError;
use awaken_sandbox_container::engine::ExistingRealizationPhase;
use awaken_sandbox_container::engine::sandbox_observation;

pub(super) async fn observe(
    runtime: &K8sRuntime,
    expectation: ContainerObservationExpectation<'_>,
) -> Result<pc::SandboxObservation, RuntimeError> {
    if expectation.purpose == ContainerObservationPurpose::Recovery
        && expectation
            .runtime_handle
            .is_some_and(awaken_sandbox_container::ContainerContinuationHandle::terminal_only)
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: "joined-empty evidence is terminal cleanup only".into(),
        });
    }
    if let Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(
        guarded,
    )) = expectation.runtime_handle
    {
        return observe_guarded_claim(runtime, expectation, guarded).await;
    }
    let expected_pod_uid = match expectation.runtime_handle {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            ..
        })
        | Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            ..
        }) => Some(pod_uid.as_str()),
        Some(awaken_sandbox_container::ContainerContinuationHandle::HostBindRestoration(_)) => {
            return Err(backend(
                "Kubernetes observation received a host-bind continuation handle",
            ));
        }
        _ => None,
    };
    let current_handle = expectation.realization_fingerprint.is_some();
    let effect_fenced_total_absence = expectation.effect_fence.is_some()
        && expectation
            .runtime_handle
            .and_then(awaken_sandbox_container::ContainerContinuationHandle::kubernetes_incarnation)
            .is_some_and(|(_, claim)| claim.is_some());
    let pod = match runtime.pods().get(expectation.container_id).await {
        Ok(pod) => Some(pod),
        Err(error) if api_not_found(&error) => None,
        Err(error) => return Err(backend(error)),
    };
    let continuation = if current_handle {
        super::existing_realization::observed_continuation_disposition(
            runtime,
            expectation.container_id,
            pod.as_ref(),
            expectation.runtime_handle,
            expectation.purpose,
            expectation.effect_fence.is_some(),
            effect_fenced_total_absence,
        )
        .await?
    } else {
        ContinuationObservationDisposition::Live
    };
    if continuation == ContinuationObservationDisposition::Incompatible {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: "Kubernetes Sandbox continuation state differs from its durable handle".into(),
        });
    }
    let mut observations = pod
        .as_ref()
        .map(observed_pod)
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    if matches!(
        continuation,
        ContinuationObservationDisposition::Disposing
            | ContinuationObservationDisposition::RetainedOriginal
    ) && !observations.is_empty()
    {
        observations[0].phase = ExistingRealizationPhase::Terminal;
    }
    let observation = sandbox_observation(
        expected_pod_uid,
        expectation.adoption_fingerprint,
        expectation.realization_fingerprint,
        expectation.effect_fence,
        &observations,
        expectation.retained_attempt,
    )?;
    let observation = if continuation == ContinuationObservationDisposition::Disposing {
        match observation {
            pc::SandboxObservation::Terminal {
                physical_incarnation,
            } => pc::SandboxObservation::Disposing {
                physical_incarnation,
            },
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some(physical_incarnation),
            } => pc::SandboxObservation::Disposing {
                physical_incarnation,
            },
            pc::SandboxObservation::Incompatible { reason } => {
                pc::SandboxObservation::Incompatible { reason }
            }
            other => {
                return Err(backend(format!(
                    "Kubernetes disposing continuation projected an invalid observation: {other:?}"
                )));
            }
        }
    } else {
        observation
    };
    if !current_handle
        && matches!(
            observation,
            pc::SandboxObservation::Provisioning | pc::SandboxObservation::Ready
        )
    {
        let pod = pod.as_ref().expect("live observation has one Pod");
        if super::existing_realization::observed_continuation_disposition(
            runtime,
            expectation.container_id,
            Some(pod),
            expectation.runtime_handle,
            expectation.purpose,
            false,
            false,
        )
        .await?
            == ContinuationObservationDisposition::Incompatible
        {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: "Kubernetes Sandbox continuation state differs from its durable handle"
                    .into(),
            });
        }
    }
    participants::terminal_observation(
        runtime,
        expectation,
        expected_pod_uid,
        pod.as_ref(),
        observation,
    )
    .await
}
