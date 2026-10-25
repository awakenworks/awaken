// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Pure projections of a Session's frozen tool configuration and Managed coordination role
// @formal reason: onto the per-Run snapshot clone; the immutable publication never changes.
//! The executable snapshot a Session Run is activated with: the Session's
//! client-tool replacement and toolsets, and the one-level Managed
//! coordination surface, projected onto a per-Run clone of the Session's
//! publication. The Coordinator projects it when it admits a Run; the
//! executor projects the same when it builds a Run context.

use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_session_types::RunError;

#[derive(Clone, Copy, PartialEq, Eq)]
/// Which Managed coordination surface a Session Run receives.
pub enum ManagedCoordinationRole {
    /// The Session's primary Thread: fixed list/send coordination tools.
    Primary,
    /// A coordinated child: no nested delegation or Advisor.
    Child,
}

fn is_replaced_managed_coordination_descriptor(
    descriptor: &awaken_runtime_contract::resolved::ToolDescriptor,
) -> bool {
    descriptor.kind == awaken_runtime_contract::resolved::ToolKind::AgentDelegation
        || descriptor.id == awaken_ext_builtin_tools::AGENT_RUN
        || descriptor.id == awaken_ext_builtin_tools::LIST_AGENTS
        || descriptor.id == awaken_ext_builtin_tools::SEND_MESSAGE
}

/// Project the one-level Managed coordination surface on a per-Session clone.
/// Primary and child projections share one removal predicate; only the primary
/// receives the fixed coordination descriptors, while a child also loses its
/// Advisor/delegate capability. The immutable publication remains unchanged.
pub fn project_managed_coordination_surface(
    config: &mut ExecutableAgentSnapshot,
    role: ManagedCoordinationRole,
) -> bool {
    let before = config.resolved_spec.tool_descriptors.clone();
    let before_delegates = config.resolved_spec.plugin_config.agent.delegates.clone();
    let before_advisor = config.resolved_spec.plugin_config.agent.advisor.clone();
    if role == ManagedCoordinationRole::Child {
        config.resolved_spec.plugin_config.agent.delegates.clear();
        config.resolved_spec.plugin_config.agent.advisor = None;
    }
    config.resolved_spec.tool_descriptors.retain(|descriptor| {
        !is_replaced_managed_coordination_descriptor(descriptor)
            && (role == ManagedCoordinationRole::Primary
                || descriptor.kind != awaken_runtime_contract::resolved::ToolKind::Advisor)
    });
    if role == ManagedCoordinationRole::Primary {
        config.resolved_spec.tool_descriptors.extend(
            awaken_ext_builtin_tools::builtin_tools()
                .into_iter()
                .filter(|tool| tool.toolset() == awaken_ext_builtin_tools::Toolset::Coordination)
                .map(awaken_ext_builtin_tools::BuiltinTool::into_descriptor),
        );
    }
    config.resolved_spec.tool_descriptors != before
        || config.resolved_spec.plugin_config.agent.delegates != before_delegates
        || config.resolved_spec.plugin_config.agent.advisor != before_advisor
}

/// Preserve the exact Session-owned client tool contract while lowering it to
/// the Runtime's canonical execution-ownership descriptor.
pub fn session_client_tool_descriptor(
    tool: &awaken_agent_contract::ClientToolDescriptor,
) -> ToolDescriptor {
    ToolDescriptor::client_executed(
        tool.name.clone(),
        tool.description.clone(),
        tool.input_schema.clone(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
/// Whether Session toolsets enter the executable snapshot.
pub enum SessionToolsetProjection {
    /// An immutable publication keeps its own toolset policy.
    PreservePublished,
    /// A generated or inherited snapshot embeds the Session toolsets.
    ProjectIntoSnapshot,
}

/// Apply the Session's exact client-tool overlay in one place. Toolset policy
/// remains outside an immutable primary publication but is embedded in a
/// generated config or inherited child dispatch snapshot, whose executor has
/// no separate Session policy input.
pub fn project_session_tool_override(
    snapshot: &mut ExecutableAgentSnapshot,
    tools: &awaken_session_configuration_contract::SessionToolConfiguration,
    toolsets: SessionToolsetProjection,
) -> bool {
    let mut changed = false;
    if toolsets == SessionToolsetProjection::ProjectIntoSnapshot
        && snapshot.resolved_spec.plugin_config.agent.toolsets != tools.toolsets
    {
        snapshot.resolved_spec.plugin_config.agent.toolsets = tools.toolsets.clone();
        changed = true;
    }
    let projected = tools
        .client_tools
        .iter()
        .map(session_client_tool_descriptor)
        .collect::<Vec<_>>();
    let current = snapshot
        .resolved_spec
        .tool_descriptors
        .iter()
        .filter(|descriptor| {
            descriptor.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
        })
        .cloned()
        .collect::<Vec<_>>();
    if current != projected {
        snapshot
            .resolved_spec
            .tool_descriptors
            .retain(|descriptor| {
                descriptor.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted
            });
        snapshot.resolved_spec.tool_descriptors.extend(projected);
        changed = true;
    }
    changed
}

/// Apply the complete Session-owned tool replacement to one attempt-local
/// snapshot. The retained Agent publication stays immutable; direct, durable,
/// and recovered attempts all consume this same projection.
pub fn project_session_tools(
    snapshot: &mut ExecutableAgentSnapshot,
    tools: &awaken_session_configuration_contract::SessionToolConfiguration,
) -> Result<(), serde_json::Error> {
    if project_session_tool_override(
        snapshot,
        tools,
        SessionToolsetProjection::ProjectIntoSnapshot,
    ) {
        snapshot.recompute_fingerprint()?;
    }
    Ok(())
}

/// The snapshot a Session's primary Run is activated with, from the
/// Session's effective publication: its complete tool replacement over the
/// published surface, then — for a Session entered through the Session
/// application whose publication coordinates other Agents — the Managed
/// primary coordination surface. A changed clone gets a fresh fingerprint.
///
/// Decision table (causes → effect):
/// | Rule | Session tools | coordinates Agents | Session dispatch | Effect |
/// |---|---|---|---|---|
/// | A1 | none | no | any | the publication unchanged, same fingerprint |
/// | A2 | some | any | any | client tools replaced; published toolsets kept |
/// | A3 | any | yes | yes | Managed primary surface replaces `agent_run` |
/// | A4 | any | yes | no | the ordinary `agent_run` surface is kept |
pub fn session_activation_snapshot(
    mut publication: ExecutableAgentSnapshot,
    tools: Option<&awaken_session_configuration_contract::SessionToolConfiguration>,
    session_dispatch: bool,
) -> Result<ExecutableAgentSnapshot, RunError> {
    let agent = &publication.resolved_spec.plugin_config.agent;
    let coordinates = !agent.delegates.is_empty() || agent.advisor.is_some();
    let mut changed = false;
    if let Some(tools) = tools {
        changed |= project_session_tool_override(
            &mut publication,
            tools,
            SessionToolsetProjection::PreservePublished,
        );
    }
    if coordinates && session_dispatch {
        changed |= project_managed_coordination_surface(
            &mut publication,
            ManagedCoordinationRole::Primary,
        );
    }
    if changed {
        publication.recompute_fingerprint().map_err(|error| {
            RunError::internal(format!("fingerprint Session tool projection: {error}"))
        })?;
    }
    Ok(publication)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_tool_projection_has_one_role_and_session_override_decision_table() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        use awaken_agent_contract::{
            ClientToolDescriptor, ToolExecutionPolicy, ToolsetPolicy, ToolsetSource,
        };
        use awaken_runtime_contract::agent_bindings::{AgentAdvisorBinding, AgentDelegateBinding};
        use awaken_runtime_contract::resolved::{
            ADVISOR_TOOL_ID, ModelBinding, ResolvedModelCandidate, ToolDescriptor, ToolKind,
        };

        // Cause/effect graph: C1 native vs Managed root/child role selects exactly
        // one coordination surface; C2 generated/inherited vs immutable publication
        // selects whether Session toolsets enter the executable clone; C3 an explicit
        // Session client descriptor replaces published client ownership. Effects:
        // E1 Managed primary replaces native `agent_run` with fixed list/send;
        // E2 every Managed child loses nested delegation/advisor and receives no
        // coordination command; E3 generated/self-child embeds Session toolsets;
        // E4 published/non-self retains its publication toolsets; E5 explicit client
        // tools exact-replace. No obsolete Task descriptor participates in projection.
        //
        // | Rule | role | snapshot owner | Session overlay | Effects |
        // | M0 | native | published | preserve | agent_run only; no Managed/Task commands |
        // | M1 | primary | generated | project | E1,E3,E5 |
        // | M2 | primary | published | preserve | E1,E4,E5 |
        // | M3 | child | self/inherited | project | E2,E3,E5 |
        // | M4 | child | other Agent | absent | E2,E4 |
        let published_policy = ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: Vec::new(),
        };
        let session_policy = ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "session-policy".into(),
            },
            default: ToolExecutionPolicy::default(),
            overrides: Vec::new(),
        };
        let published_client = ClientToolDescriptor {
            name: "published_client".into(),
            description: "published".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let session_client = ClientToolDescriptor {
            name: "session_client".into(),
            description: "session".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        };
        let tools = awaken_session_configuration_contract::SessionToolConfiguration {
            toolsets: vec![session_policy.clone()],
            client_tools: vec![session_client.clone()],
        };
        let builtin = |id: &str| {
            awaken_ext_builtin_tools::builtin_tools()
                .into_iter()
                .find(|tool| tool.descriptor().id == id)
                .expect("canonical builtin descriptor")
                .into_descriptor()
        };
        let mut base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("coordinator")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "model", "native")
                    .expect("valid model"),
            )
            .build();
        base.resolved_spec.tool_descriptors = vec![
            ToolDescriptor::pinned(
                "test",
                "ordinary",
                "ordinary",
                serde_json::json!({"type": "object"}),
            ),
            session_client_tool_descriptor(&published_client),
            builtin(awaken_ext_builtin_tools::AGENT_RUN),
            builtin(awaken_ext_builtin_tools::LIST_AGENTS),
            builtin(awaken_ext_builtin_tools::SEND_MESSAGE),
            ToolDescriptor::pinned(
                "test",
                ADVISOR_TOOL_ID,
                "advisor",
                serde_json::json!({"type": "object"}),
            )
            .with_kind(ToolKind::Advisor),
        ];
        base.resolved_spec.plugin_config.agent.delegates = vec![AgentDelegateBinding {
            agent_id: awaken_runtime_contract::snapshot::AgentId("worker".into()),
            source_revision: Some(1),
            recursive_self: false,
        }];
        base.resolved_spec.plugin_config.agent.advisor = Some(AgentAdvisorBinding {
            model: "advisor-model".into(),
            candidate: ResolvedModelCandidate::host(ModelBinding::new(
                "provider",
                "advisor-model",
                "backend",
            )),
        });
        base.resolved_spec.plugin_config.agent.toolsets = vec![published_policy.clone()];

        let descriptor_ids = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot| {
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .map(|descriptor| descriptor.id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let assert_primary = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
                              rule: &str| {
            let ids = descriptor_ids(snapshot);
            assert!(
                ids.contains(awaken_ext_builtin_tools::LIST_AGENTS),
                "{rule}/E1"
            );
            assert!(
                ids.contains(awaken_ext_builtin_tools::SEND_MESSAGE),
                "{rule}/E1"
            );
            assert_eq!(
                snapshot
                    .resolved_spec
                    .tool_descriptors
                    .iter()
                    .filter(|descriptor| descriptor.id == awaken_ext_builtin_tools::SEND_MESSAGE)
                    .count(),
                1,
                "{rule}/E1 one Agent-message implementation"
            );
            assert!(
                !ids.contains(awaken_ext_builtin_tools::AGENT_RUN),
                "{rule}/E1"
            );
            assert!(
                ids.contains(ADVISOR_TOOL_ID),
                "{rule}/E1 advisor remains internal"
            );
        };
        let assert_child = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
                            rule: &str| {
            let ids = descriptor_ids(snapshot);
            for forbidden in [
                awaken_ext_builtin_tools::AGENT_RUN,
                awaken_ext_builtin_tools::LIST_AGENTS,
                awaken_ext_builtin_tools::SEND_MESSAGE,
                ADVISOR_TOOL_ID,
            ] {
                assert!(!ids.contains(forbidden), "{rule}/E2 forbids {forbidden}");
            }
            assert!(
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .delegates
                    .is_empty(),
                "{rule}/E2"
            );
            assert!(
                snapshot.resolved_spec.plugin_config.agent.advisor.is_none(),
                "{rule}/E2"
            );
        };

        let mut generated_primary = base.clone();
        project_session_tool_override(
            &mut generated_primary,
            &tools,
            SessionToolsetProjection::ProjectIntoSnapshot,
        );
        project_managed_coordination_surface(
            &mut generated_primary,
            ManagedCoordinationRole::Primary,
        );
        assert_primary(&generated_primary, "M1");
        assert_eq!(
            generated_primary
                .resolved_spec
                .plugin_config
                .agent
                .toolsets
                .as_slice(),
            std::slice::from_ref(&session_policy),
            "M1/E3"
        );

        let mut published_primary = base.clone();
        project_session_tool_override(
            &mut published_primary,
            &tools,
            SessionToolsetProjection::PreservePublished,
        );
        project_managed_coordination_surface(
            &mut published_primary,
            ManagedCoordinationRole::Primary,
        );
        assert_primary(&published_primary, "M2");
        assert_eq!(
            published_primary
                .resolved_spec
                .plugin_config
                .agent
                .toolsets
                .as_slice(),
            std::slice::from_ref(&published_policy),
            "M2/E4"
        );

        for (rule, snapshot) in [("M1", &generated_primary), ("M2", &published_primary)] {
            let clients = snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
                .collect::<Vec<_>>();
            assert_eq!(clients.len(), 1, "{rule}/E5");
            assert_eq!(clients[0].id, session_client.name, "{rule}/E5");
        }

        let mut inherited_child = base.clone();
        project_session_tool_override(
            &mut inherited_child,
            &tools,
            SessionToolsetProjection::ProjectIntoSnapshot,
        );
        project_managed_coordination_surface(&mut inherited_child, ManagedCoordinationRole::Child);
        assert_child(&inherited_child, "M3");
        assert_eq!(
            inherited_child.resolved_spec.plugin_config.agent.toolsets,
            [session_policy],
            "M3/E3"
        );
        assert_eq!(
            inherited_child
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
                .map(|descriptor| descriptor.id.as_str())
                .collect::<Vec<_>>(),
            [session_client.name.as_str()],
            "M3/E5"
        );

        let mut other_child = base;
        project_managed_coordination_surface(&mut other_child, ManagedCoordinationRole::Child);
        assert_child(&other_child, "M4");
        assert_eq!(
            other_child.resolved_spec.plugin_config.agent.toolsets,
            [published_policy],
            "M4/E4"
        );
        assert_eq!(
            other_child
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
                .map(|descriptor| descriptor.id.as_str())
                .collect::<Vec<_>>(),
            [published_client.name.as_str()],
            "M4/E4"
        );
    }

    #[test]
    fn session_activation_snapshot_follows_its_decision_table() {
        // Rows A1-A4 of `session_activation_snapshot`.
        use awaken_runtime_contract::agent_bindings::AgentDelegateBinding;
        let publication = awaken_runtime_contract::ExecutableAgentSnapshot::builder("primary")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "model", "native")
                    .expect("valid model"),
            )
            .build();
        let unchanged =
            session_activation_snapshot(publication.clone(), None, true).expect("A1 projection");
        assert_eq!(
            unchanged, publication,
            "A1 unchanged publication and fingerprint"
        );

        let tools = awaken_session_configuration_contract::SessionToolConfiguration {
            client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
                name: "pick_file".into(),
                description: "Pick a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            ..Default::default()
        };
        let replaced = session_activation_snapshot(publication.clone(), Some(&tools), false)
            .expect("A2 projection");
        assert!(
            replaced
                .resolved_spec
                .tool_descriptors
                .iter()
                .any(|descriptor| descriptor.id == "pick_file"),
            "A2 Session client tool"
        );
        assert_ne!(
            replaced.fingerprint, publication.fingerprint,
            "A2 new fingerprint"
        );

        let mut coordinating = publication;
        coordinating
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .push(AgentDelegateBinding {
                agent_id: awaken_runtime_contract::snapshot::AgentId("worker".into()),
                source_revision: Some(1),
                recursive_self: false,
            });
        coordinating
            .recompute_fingerprint()
            .expect("fixture fingerprint");
        let managed =
            session_activation_snapshot(coordinating.clone(), None, true).expect("A3 projection");
        let ids = managed
            .resolved_spec
            .tool_descriptors
            .iter()
            .map(|descriptor| descriptor.id.as_str())
            .collect::<Vec<_>>();
        assert!(
            ids.contains(&awaken_ext_builtin_tools::SEND_MESSAGE)
                && !ids.contains(&awaken_ext_builtin_tools::AGENT_RUN),
            "A3 Managed primary surface: {ids:?}"
        );
        let ordinary =
            session_activation_snapshot(coordinating.clone(), None, false).expect("A4 projection");
        assert_eq!(ordinary, coordinating, "A4 ordinary surface kept");
    }
}
