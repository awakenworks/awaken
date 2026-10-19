// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Terminal evidence for a Session that lost its handle: an exact created, joined or
// @formal reason: isolated original, or a proven-unmaterialized absence; anything else stays Unknown.
//! Terminal evidence for a Session whose handle was lost: the exact original
//! a Worker created or joined, or proof that nothing was ever materialized.

use super::K8sRuntime;
use super::backend;
use super::continuation;
use super::continuation_claim_for_pod;
use super::create_ledger;
use super::creation;
use super::guarded_claim::verify_joined_original_claim;
use super::participants;
use super::pod_name;
use super::sandbox_control;
use awaken_container_contract::CreateTicket;
use awaken_container_contract::RequestJoin;
use awaken_container_contract::RuntimeError;
use awaken_container_contract::TicketResult;
use awaken_container_contract::TicketStatus;
use awaken_provisioning_contract::SandboxEffectFence;
use awaken_sandbox_container::ContainerPlan;
use awaken_sandbox_container::ContainerRealizationContext;
use awaken_sandbox_container::ContainerRealizationIntent;
use awaken_sandbox_container::MissingHandleTerminalEvidence;
use k8s_openapi::api::core::v1::Pod;

pub(super) fn unmaterialized_absence_shape_is_closed(
    has_packages: bool,
    has_binds: bool,
    has_memory: bool,
    has_continuation_claim: bool,
) -> bool {
    !(has_packages || has_binds || has_memory || has_continuation_claim)
}

#[derive(Default)]
pub(super) struct JoinedOriginalTickets {
    created_pod: Option<(CreateTicket, String)>,
    other_tickets: Vec<CreateTicket>,
    created_claim_uid: Option<String>,
    isolated_pod: Option<(CreateTicket, String)>,
}

pub(super) fn joined_original_tickets(
    tickets: Vec<CreateTicket>,
    namespace: &str,
    pod_name: &str,
    claim_name: &str,
    claim_required: bool,
) -> Option<JoinedOriginalTickets> {
    let mut joined = JoinedOriginalTickets::default();
    for ticket in tickets {
        if ticket.isolation_pending
            || !matches!(
                ticket.request.resource.as_str(),
                "pods"
                    | "persistentvolumeclaims"
                    | "configmaps"
                    | "secrets"
                    | "jobs"
                    | "daemonsets"
            )
        {
            return None;
        }
        match &ticket.status {
            TicketStatus::Finished(TicketResult::RejectedNoEffect { .. }) => {}
            TicketStatus::Finished(TicketResult::Created { uid })
                if ticket.request.resource == "pods"
                    && ticket.request.namespace == namespace
                    && ticket.request.name == pod_name
                    && joined.created_pod.is_none()
                    && joined.isolated_pod.is_none() =>
            {
                joined.created_pod = Some((ticket.clone(), uid.clone()));
            }
            TicketStatus::Finished(TicketResult::Created { uid })
                if claim_required
                    && ticket.request.resource == "persistentvolumeclaims"
                    && ticket.request.namespace == namespace
                    && ticket.request.name == claim_name
                    && joined.created_claim_uid.is_none() =>
            {
                joined.created_claim_uid = Some(uid.clone());
            }
            TicketStatus::Finished(TicketResult::Isolated { uid })
                if ticket.request.resource == "pods"
                    && ticket.request.namespace == namespace
                    && ticket.request.name == pod_name
                    && joined.created_pod.is_none()
                    && joined.isolated_pod.is_none() =>
            {
                joined.isolated_pod = Some((ticket.clone(), uid.clone()));
            }
            TicketStatus::Finished(
                TicketResult::Created { .. } | TicketResult::Isolated { .. },
            ) => {
                // A duplicate own-root/claim result is contradictory evidence,
                // never another Session's closed request.
                if ticket.request.namespace == namespace
                    && ((ticket.request.resource == "pods" && ticket.request.name == pod_name)
                        || (ticket.request.resource == "persistentvolumeclaims"
                            && ticket.request.name == claim_name))
                {
                    return None;
                }
                joined.other_tickets.push(ticket.clone());
            }
            _ => return None,
        }
    }
    if (joined.created_pod.is_none() || joined.isolated_pod.is_some())
        && !joined.other_tickets.is_empty()
    {
        return None;
    }
    Some(joined)
}

pub(super) async fn joined_original_evidence(
    runtime: &K8sRuntime,
    scope: &str,
    pod: &Pod,
    claim_uid: Option<String>,
    predecessor: &SandboxEffectFence,
    joined: &JoinedOriginalTickets,
    plan: &ContainerPlan,
) -> Result<MissingHandleTerminalEvidence, RuntimeError> {
    let Some((original_pod, pod_uid)) = &joined.created_pod else {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    };
    let Ok((original_fence, adoption, realization, attempt)) =
        creation::verify_interrupted_pod_projection(
            runtime,
            scope,
            plan,
            predecessor,
            claim_uid.as_deref(),
            pod,
        )
    else {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    };
    let intent = ContainerRealizationIntent::Create;
    let context = ContainerRealizationContext::new(
        scope,
        &adoption,
        Some(&original_fence),
        &intent,
        &attempt,
    );
    let projected = match participants::retained(pod) {
        Ok(Some(projected)) => {
            if participants::verify_joined_original(
                runtime,
                pod,
                plan,
                original_pod,
                &joined.other_tickets,
                &projected,
                (&context, &realization),
            )
            .await
            .is_err()
            {
                return Ok(MissingHandleTerminalEvidence::Unknown);
            }
            projected
        }
        Ok(None)
            if joined.other_tickets.is_empty()
                && plan.binds.is_empty()
                && plan.control_services.is_empty() =>
        {
            // Preserve the original Complete/narrow/full-projection empty law.
            // No missing/partial rich handoff is filled or written here.
            awaken_sandbox_container::KubernetesProjectedParticipants::joined_empty(
                runtime.namespace.clone(),
                pod_uid.clone(),
                attempt.as_str().to_owned(),
                predecessor.clone(),
                original_pod.request.group.clone(),
            )
            .map_err(backend)?
        }
        _ => return Ok(MissingHandleTerminalEvidence::Unknown),
    };
    Ok(MissingHandleTerminalEvidence::ExactIncarnation {
        container_id: pod
            .metadata
            .name
            .clone()
            .ok_or_else(|| backend("original Pod name is missing"))?,
        runtime_handle:
            awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                pod_uid: pod_uid.clone(),
                claim_uid,
                participants: Box::new(projected),
            },
    })
}

pub(super) struct GuardedOriginalContext<'a> {
    scope: &'a str,
    plan: &'a ContainerPlan,
    predecessor: &'a SandboxEffectFence,
    pod_name: &'a str,
    claim_name: &'a str,
    claim_required: bool,
    absence_shape_closed: bool,
}

pub(super) async fn isolated_pod_terminal_evidence(
    runtime: &K8sRuntime,
    context: GuardedOriginalContext<'_>,
    created_claim_uid: Option<String>,
    ticket: CreateTicket,
    uid: String,
) -> Result<MissingHandleTerminalEvidence, RuntimeError> {
    if !create_ledger::verify_pod_isolation(&runtime.clients.control, &ticket, &uid).await? {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    }
    let claim =
        crate::get_optional(&runtime.persistent_volume_claims(), context.claim_name).await?;
    if !context.claim_required {
        return Ok(
            if claim.is_none() && created_claim_uid.is_none() && context.absence_shape_closed {
                MissingHandleTerminalEvidence::Isolated
            } else {
                MissingHandleTerminalEvidence::Unknown
            },
        );
    }
    let Some(claim_uid) = created_claim_uid else {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    };
    let Some(claim) = claim else {
        // The original UID is already gone and the immutable guard still
        // excludes a late Pod. This is disposal response replay.
        return Ok(MissingHandleTerminalEvidence::Isolated);
    };
    if verify_joined_original_claim(
        runtime,
        context.scope,
        context.plan,
        context.predecessor,
        &claim_uid,
        &claim,
    )
    .is_err()
    {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    }
    Ok(MissingHandleTerminalEvidence::ExactIncarnation {
        container_id: context.pod_name.to_owned(),
        runtime_handle:
            awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(Box::new(
                awaken_sandbox_container::KubernetesGuardedClaimHandle::new(
                    context.scope,
                    context.predecessor,
                    ticket,
                    &claim_uid,
                )?,
            )),
    })
}

pub(super) async fn unmaterialized_after_join(
    runtime: &K8sRuntime,
    claim_name: &str,
    created_claim: bool,
    absence_shape_closed: bool,
    pod_present: bool,
) -> Result<MissingHandleTerminalEvidence, RuntimeError> {
    if created_claim || !absence_shape_closed || pod_present {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    }
    let claim = crate::get_optional(&runtime.persistent_volume_claims(), claim_name).await?;
    Ok(if claim.is_none() {
        MissingHandleTerminalEvidence::Unmaterialized
    } else {
        MissingHandleTerminalEvidence::Unknown
    })
}

pub(super) async fn missing_handle_terminal_evidence(
    runtime: &K8sRuntime,
    scope: &str,
    plan: &ContainerPlan,
    expected_effect_fence: Option<&SandboxEffectFence>,
) -> Result<MissingHandleTerminalEvidence, RuntimeError> {
    // Kubernetes creates the Pod before ConfigMaps, Secrets, Memory
    // projection, or control sidecars. Only the narrow shape can receive
    // an absence witness. A brokered rich attempt still seals its one
    // provider request ledger before returning Unknown, so the original
    // Worker cannot keep admitting physical Creates after terminal stop.
    let narrow_shape = plan.control_services.is_empty()
        && unmaterialized_absence_shape_is_closed(
            !plan.packages.is_empty(),
            !plan.binds.is_empty(),
            !plan.memory_mounts.is_empty(),
            false,
        );
    let reconstructible_shape = plan.packages.is_empty()
        && plan
            .control_services
            .is_subset(&sandbox_control::services(runtime))
        && participants::self_contained_inline_projection(plan);
    let claim_required = continuation::claim_required(plan, runtime.continuation_volume.is_some());
    let absence_shape_closed = narrow_shape && !claim_required;
    let Some(expected_effect_fence) = expected_effect_fence else {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    };
    let runtime_pod_name = pod_name(&runtime.realization_runtime_id(scope)?);
    let runtime_claim_name = continuation_claim_for_pod(&runtime_pod_name)?;
    let joined = if let Some(transport) = &runtime.create_transport {
        let RequestJoin::Complete(tickets) = transport
            .seal_predecessor(scope, expected_effect_fence, &runtime_pod_name)
            .await?
        else {
            return Ok(MissingHandleTerminalEvidence::Unknown);
        };
        // Whole-Worker Create inventory and this Session's membership differ.
        // One exact Pod/PVC and its retained frozen projected set select
        // our members. Other finished requests confer no membership;
        // unresolved requests or missing rich handoff remain Unknown.
        let Some(joined) = joined_original_tickets(
            tickets,
            &runtime.namespace,
            &runtime_pod_name,
            &runtime_claim_name,
            claim_required,
        ) else {
            return Ok(MissingHandleTerminalEvidence::Unknown);
        };
        joined
    } else {
        if !absence_shape_closed {
            return Ok(MissingHandleTerminalEvidence::Unknown);
        }
        let Some(_closed_attempt) = runtime
            .close_observed_creation_attempt(scope, expected_effect_fence)
            .await
        else {
            // A direct-mode replacement cannot infer closure of a request
            // it never joined. It never borrows the brokered proof path.
            return Ok(MissingHandleTerminalEvidence::Unknown);
        };
        JoinedOriginalTickets::default()
    };
    if !reconstructible_shape || (!narrow_shape && joined.created_pod.is_none()) {
        return Ok(MissingHandleTerminalEvidence::Unknown);
    }
    super::existing_realization::admit_plan(runtime, plan).await?;
    if let Some((ticket, uid)) = &joined.isolated_pod {
        return isolated_pod_terminal_evidence(
            runtime,
            GuardedOriginalContext {
                scope,
                plan,
                predecessor: expected_effect_fence,
                pod_name: &runtime_pod_name,
                claim_name: &runtime_claim_name,
                claim_required,
                absence_shape_closed,
            },
            joined.created_claim_uid.clone(),
            ticket.clone(),
            uid.clone(),
        )
        .await;
    }
    let pod = crate::get_optional(&runtime.pods(), &runtime_pod_name).await?;
    if let Some((_, pod_uid)) = &joined.created_pod {
        let Some(pod) = pod else {
            return Ok(MissingHandleTerminalEvidence::Unknown);
        };
        let claim =
            crate::get_optional(&runtime.persistent_volume_claims(), &runtime_claim_name).await?;
        let claim_uid = match (claim_required, &joined.created_claim_uid, claim.as_ref()) {
            (true, Some(ticket_uid), Some(observed))
                if verify_joined_original_claim(
                    runtime,
                    scope,
                    plan,
                    expected_effect_fence,
                    ticket_uid,
                    observed,
                )
                .is_ok() =>
            {
                Some(ticket_uid.to_owned())
            }
            (false, None, None) => None,
            _ => return Ok(MissingHandleTerminalEvidence::Unknown),
        };
        if pod.metadata.uid.as_deref() != Some(pod_uid.as_str())
            || pod.metadata.deletion_timestamp.is_some()
            || continuation::bound_claim_uid(&pod) != claim_uid.as_deref()
        {
            return Ok(MissingHandleTerminalEvidence::Unknown);
        }
        return joined_original_evidence(
            runtime,
            scope,
            &pod,
            claim_uid,
            expected_effect_fence,
            &joined,
            plan,
        )
        .await;
    }
    unmaterialized_after_join(
        runtime,
        &runtime_claim_name,
        joined.created_claim_uid.is_some(),
        absence_shape_closed,
        pod.is_some(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_shape_decision_table_is_total_and_non_widening() {
        // UA1: enumerate the complete four-cause state space. Only the row with
        // no independently governed participant may proceed to live Pod/PVC
        // observation; every richer topology remains handle-fenced.
        for bits in 0_u8..16 {
            let causes = [bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0];
            assert_eq!(
                unmaterialized_absence_shape_is_closed(causes[0], causes[1], causes[2], causes[3]),
                bits == 0,
                "UA1 row {bits:04b}"
            );
        }
    }
}
