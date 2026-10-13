// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: runtime-host Run recovery and input projection. Dynamic path: committed
// @formal reason: Run and Thread facts select the exact resume state supplied to the Agent loop after
// @formal reason: start or recovery. Evidence: runtime.agent_loop.runtime_execution and runtime-host
// @formal reason: recovery tests; this is a product-requirement classification, not a direct proof claim.
//! Pure projections used by Run recovery, delegation, and resume delivery.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct StepCommitExpectation<'a> {
    pub(super) messages_before: usize,
    pub(super) input_ids: &'a [String],
}

pub(super) fn project_delegated_runs(
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
) -> Vec<DelegatedRun> {
    registry
        .into_iter()
        .flat_map(|registry| {
            registry.delegations().map(|delegation| DelegatedRun {
                run_id: delegation.child_run_id.clone(),
                parent_call_id: delegation.parent_call_id.clone(),
                agent_id: delegation.target_agent_id.clone(),
                status: delegation.status,
            })
        })
        .collect()
}

pub(super) fn delegation_registry_from_snapshot(
    snapshot: &RunRecoverySnapshot,
    run_id: &RunId,
) -> Result<Option<awaken_agent_contract::agent::delegation::DelegationRegistry>, HostError> {
    delegation_registries_from_commands(&snapshot.state, Some(run_id))
        .map(|mut registries| registries.pop())
}

/// One Run-scoped state fold serves exact-Run recovery and all Run-scoped
/// delegation projections. Selection precedes decoding, so an unrelated Run's
/// damaged registry cannot change an exact-Run observation.
pub(super) fn delegation_registries_from_commands(
    commands: &[awaken_agent_contract::agent::state::Command],
    selected_run: Option<&RunId>,
) -> Result<Vec<awaken_agent_contract::agent::delegation::DelegationRegistry>, HostError> {
    let mut stores: HashMap<RunId, Store> = HashMap::new();
    for command in commands {
        let Some(run_id) = command.run_id.as_ref().filter(|run_id| {
            command.scope == Scope::Run && selected_run.is_none_or(|selected| selected == *run_id)
        }) else {
            continue;
        };
        stores.entry(run_id.clone()).or_default().apply(command);
    }
    let mut registries = Vec::new();
    for store in stores.values() {
        if let Some(registry) =
            RunDelegations::load(store).map_err(|error| HostError::internal(error.to_string()))?
        {
            registries.push(registry);
        }
    }
    Ok(registries)
}

pub(super) fn client_result_for_ticket(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    is_error: bool,
) -> ResumeResult {
    if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
        ResumeResult::Input(awaken_agent_contract::agent::content::extract_text(
            &content,
        ))
    } else {
        let output = if is_error {
            ToolOutput::error_blocks(tool_use_id, content)
        } else {
            ToolOutput::ok_blocks(tool_use_id, content)
        };
        ResumeResult::ToolResult(output)
    }
}

pub(in crate::host) fn session_thread_reply_result(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    reply: &awaken_session_execution_contract::SessionThreadToolReply,
) -> ResumeResult {
    match reply {
        awaken_session_execution_contract::SessionThreadToolReply::Input(text) => {
            ResumeResult::Input(text.clone())
        }
        awaken_session_execution_contract::SessionThreadToolReply::Confirm(decision) => {
            ResumeResult::Permission(decision.clone())
        }
        awaken_session_execution_contract::SessionThreadToolReply::Custom { content, is_error }
        | awaken_session_execution_contract::SessionThreadToolReply::Result { content, is_error } => {
            client_result_for_ticket(ticket, tool_use_id, content.clone(), *is_error)
        }
    }
}

/// The one answerable ticket of `run_id` under the neutral selector: two rows
/// or a foreign owner are damaged authority, never a first-match pick.
pub(super) fn recovery_ticket(
    committed: &RunRecoverySnapshot,
    run_id: &RunId,
) -> Result<Option<ResumeTicket>, HostError> {
    committed
        .answerable_ticket(run_id)
        .map(|ticket| ticket.cloned())
        .map_err(|error| HostError::internal(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::delegation::{
        DelegationId, DelegationLimits, DelegationRegistry, DelegationStatus, RequestDelegation,
    };
    use awaken_agent_contract::agent::state::Command as StateCommand;

    fn registry(parent: &RunId, call: &str) -> (DelegationRegistry, DelegationId) {
        let id = DelegationId::for_parent_call(parent, call);
        let mut registry = DelegationRegistry::new(
            parent.clone(),
            "coordinator",
            Vec::new(),
            0,
            DelegationLimits::default(),
        );
        registry
            .request(RequestDelegation {
                id: id.clone(),
                parent_call_id: call.into(),
                target_agent_id: "writer".into(),
                recursive_self: false,
                child_run_id: id.child_run_id(),
            })
            .unwrap();
        (registry, id)
    }

    fn command(registry: &DelegationRegistry) -> StateCommand {
        let mut command = RunDelegations::write(&Some(registry.clone()));
        command.bind_run(registry.parent_run_id());
        command
    }

    #[test]
    fn delegation_scope_selection_preserves_history_and_isolates_corruption() {
        // Causes: C1 selected Run is exact/absent or all Runs; C2 retained
        // registries span older A and newer B; C3 A has a later completion;
        // C4 malformed state is Run-owned B, unowned legacy, or Thread-scoped.
        // Effects: E1 exact A ignores B corruption; E2 all Runs retain A/B and
        // A's current status; E3 selecting B/all rejects B corruption; E4 absent
        // selection is empty; E5 unowned/Thread data cannot become Run history;
        // E6 cold serde and reads preserve the original command bytes.
        // R1=all+valid A/B+C3 => E2; R2=exact A+bad B => E1;
        // R3=exact B|all+bad B => E3; R4=absent => E4;
        // R5=legacy|Thread malformed => E5; R6=cold|repeated => E6.
        // This is the existing registry projection, not physical membership or
        // original-domain proof. Missing legacy ownership is never repaired.
        let a = RunId("older-parent".into());
        let b = RunId("newer-parent".into());
        let (mut registry_a, id_a) = registry(&a, "older-call");
        let (registry_b, id_b) = registry(&b, "newer-call");
        let mut commands = vec![command(&registry_a), command(&registry_b)];
        registry_a.complete(&id_a).unwrap();
        commands.push(command(&registry_a));
        let mut bad_b = command(&registry_b);
        bad_b.action =
            awaken_agent_contract::agent::state::Action::Set(serde_json::json!("broken"));
        let mut unowned = bad_b.clone();
        unowned.run_id = None;
        let mut thread_scoped = bad_b.clone();
        thread_scoped.scope = Scope::Thread;
        commands.extend([unowned, thread_scoped]);

        let before = serde_json::to_vec(&commands).unwrap();
        let cold: Vec<StateCommand> = serde_json::from_slice(&before).unwrap();
        for source in [&commands, &cold] {
            let registries = delegation_registries_from_commands(source, None).unwrap();
            assert_eq!(registries.len(), 2, "R1/R5 both original Run registries");
            let completed = registries
                .iter()
                .find(|registry| registry.parent_run_id() == &a)
                .unwrap();
            assert_eq!(
                completed.get(&id_a).unwrap().status,
                DelegationStatus::Completed,
                "R1 old Run survives later Run and status writes"
            );
            assert!(
                registries
                    .iter()
                    .any(|registry| registry.get(&id_b).is_some()),
                "R1 newer Run remains"
            );
            assert!(
                delegation_registries_from_commands(source, Some(&RunId("missing".into())))
                    .unwrap()
                    .is_empty(),
                "R4 absent does not select latest"
            );
        }
        commands.push(bad_b);
        let selected = delegation_registries_from_commands(&commands, Some(&a)).unwrap();
        assert_eq!(
            selected.len(),
            1,
            "R2 unrelated damage is outside exact selection"
        );
        assert_eq!(
            selected[0].get(&id_a).unwrap().status,
            DelegationStatus::Completed,
            "R2 exact old Run"
        );
        assert!(
            delegation_registries_from_commands(&commands, Some(&b)).is_err(),
            "R3 selected malformed registry"
        );
        assert!(
            delegation_registries_from_commands(&commands, None).is_err(),
            "R3 all Runs cannot skip malformed B"
        );
        commands.pop();
        assert_eq!(
            serde_json::to_vec(&commands).unwrap(),
            before,
            "R6 reads preserve the original journal"
        );
        assert_eq!(
            serde_json::to_vec(&cold).unwrap(),
            before,
            "R6 cold bytes unchanged"
        );
    }
}
