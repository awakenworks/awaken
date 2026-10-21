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
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxObservationPurpose;
use awaken_sandbox_container::ContainerObservationExpectation;
use awaken_sandbox_container::engine::{sandbox_observation, sandbox_observation_identity};

pub(super) async fn observe(
    runtime: &K8sRuntime,
    expectation: ContainerObservationExpectation<'_>,
) -> Result<pc::SandboxObservation, RuntimeError> {
    if expectation.purpose == SandboxObservationPurpose::Recovery
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
    let observation = project_continuation_observation(
        continuation,
        expectation,
        expected_pod_uid,
        pod.as_ref(),
    )?;
    if !current_handle
        && matches!(
            observation,
            pc::SandboxObservation::Provisioning { .. } | pc::SandboxObservation::Ready
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
    let observation = participants::terminal_observation(
        runtime,
        expectation,
        expected_pod_uid,
        pod.as_ref(),
        observation,
    )
    .await?;
    Ok(with_provisioning_deadline(
        runtime,
        pod.as_ref(),
        observation,
    ))
}

/// The cluster, not this process, progresses a created Pod, so this provider
/// owns the provisioning wait and gives a Provisioning observation its
/// deadline.
fn with_provisioning_deadline(
    runtime: &K8sRuntime,
    pod: Option<&k8s_openapi::api::core::v1::Pod>,
    observation: pc::SandboxObservation,
) -> pc::SandboxObservation {
    match observation {
        pc::SandboxObservation::Provisioning { deadline: None } => {
            pc::SandboxObservation::Provisioning {
                deadline: pod.and_then(|pod| {
                    super::realization::pod_provisioning_deadline(
                        pod,
                        runtime.unschedulable_wait,
                        runtime.clock.as_ref(),
                    )
                }),
            }
        }
        observation => observation,
    }
}

/// Apply the one immutable-identity classifier before interpreting the exact
/// provider-owned continuation fact. Neither branch rewrites physical phase.
fn project_continuation_observation(
    continuation: ContinuationObservationDisposition,
    expectation: ContainerObservationExpectation<'_>,
    expected_pod_uid: Option<&str>,
    pod: Option<&k8s_openapi::api::core::v1::Pod>,
) -> Result<pc::SandboxObservation, RuntimeError> {
    if continuation == ContinuationObservationDisposition::Incompatible {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: "Kubernetes Sandbox continuation state differs from its durable handle".into(),
        });
    }
    // A known continuation mismatch already decides incompatibility; malformed
    // Pod evidence must not replace that fact with a retryable backend error.
    let observations = pod
        .map(observed_pod)
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    if continuation == ContinuationObservationDisposition::Live {
        return sandbox_observation(
            expected_pod_uid,
            expectation.adoption_fingerprint,
            expectation.realization_fingerprint,
            expectation.effect_fence,
            &observations,
            expectation.retained_attempt,
        );
    }
    if expected_pod_uid.is_none() {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: "retained Kubernetes observation requires the original Pod UID".into(),
        });
    }
    let original = match sandbox_observation_identity(
        expected_pod_uid,
        expectation.adoption_fingerprint,
        expectation.realization_fingerprint,
        expectation.effect_fence,
        &observations,
        expectation.retained_attempt,
    ) {
        Ok(original) => original,
        Err(pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: Some(physical_incarnation),
        }) if continuation == ContinuationObservationDisposition::Disposing => {
            return Ok(pc::SandboxObservation::Disposing {
                physical_incarnation,
            });
        }
        Err(observation) => return Ok(observation),
    };
    let physical_incarnation = original.incarnation.identity.clone();
    match (continuation, expectation.purpose) {
        (ContinuationObservationDisposition::Disposing, _) => {
            Ok(pc::SandboxObservation::Disposing {
                physical_incarnation,
            })
        }
        (
            ContinuationObservationDisposition::RetainedOriginal,
            SandboxObservationPurpose::WorkerRetirement {
                source_prepared: false,
            },
        ) => Ok(pc::SandboxObservation::RetainedOriginal {
            physical_incarnation,
        }),
        (
            ContinuationObservationDisposition::RetainedOriginal,
            SandboxObservationPurpose::TerminalCleanup,
        ) => {
            // Terminal admission requires an actual Failed/Succeeded original,
            // including one whose deletion is held by its original guard.
            Ok(pc::SandboxObservation::Terminal {
                physical_incarnation,
            })
        }
        _ => Err(RuntimeError::Conflict(
            "original source retention was returned for an incompatible observation purpose".into(),
        )),
    }
}
