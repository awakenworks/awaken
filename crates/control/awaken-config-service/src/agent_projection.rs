//! Pure projection from one immutable publication into the existing neutral
//! Session configuration view carried by executable registration.

use awaken_config_resolver::AgentInputConfig;
use awaken_config_store::ModelSelection;
use awaken_runtime_contract::{ExecutableAgentSnapshot, ResolvedInputVersion};
use awaken_session_contract::{
    AgentClientToolView, AgentConfigView, AgentEnvironmentBindingView, AgentMcpServerView,
};

/// Build the Session-facing projection from the exact publication and the
/// current Agent-default inputs. A pinned defaults revision mismatch fails
/// closed so Control cannot register a mixed-revision application view.
pub(crate) fn registered_agent_view(
    snapshot: &ExecutableAgentSnapshot,
    authored_model_selection: &ModelSelection,
    current_defaults: Option<AgentInputConfig>,
) -> Option<AgentConfigView> {
    project(snapshot, authored_model_selection, current_defaults, true)
}

/// Historical exact revisions remain addressable for replay even when the
/// current defaults repository no longer retains their old value. They never
/// become current during reconciliation: newest registrations are submitted
/// first, and this projection intentionally carries no guessed defaults.
pub(crate) fn historical_agent_view(
    snapshot: &ExecutableAgentSnapshot,
    authored_model_selection: &ModelSelection,
) -> AgentConfigView {
    project(snapshot, authored_model_selection, None, false)
        .expect("historical projection ignores unavailable Session defaults")
}

fn project(
    snapshot: &ExecutableAgentSnapshot,
    authored_model_selection: &ModelSelection,
    current_defaults: Option<AgentInputConfig>,
    require_exact_defaults: bool,
) -> Option<AgentConfigView> {
    let spec = &snapshot.resolved_spec;
    let bindings = spec.plugin_config.agent.clone();
    let pinned_defaults_revision = snapshot
        .metadata
        .resolution
        .inputs
        .iter()
        .find(|input| {
            input.kind == "agent_session_defaults" && input.id == snapshot.root_agent_id.0
        })
        .and_then(|input| match input.version {
            ResolvedInputVersion::Revision(revision) => Some(revision as i64),
            _ => None,
        });
    let defaults = match (pinned_defaults_revision, current_defaults) {
        (Some(expected), Some(defaults)) if defaults.revision == expected => defaults,
        (Some(_), _) if require_exact_defaults => return None,
        _ => AgentInputConfig {
            agent_id: snapshot.root_agent_id.0.clone(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    let managed_model = crate::render_managed_model_id(authored_model_selection)
        .or_else(|_| {
            crate::render_managed_model_id(&ModelSelection::Pinned(
                spec.model_binding.binding.clone(),
            ))
        })
        .ok();
    Some(AgentConfigView {
        model: managed_model,
        execution_model_ref: Some(spec.model_binding.binding.model_ref.clone()),
        backend_ref: spec.model_binding.backend_ref.clone(),
        system: (!spec.instructions.is_empty()).then(|| spec.instructions.clone()),
        tool_ids: spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted
            })
            .map(|descriptor| descriptor.id.clone())
            .collect(),
        toolsets: bindings.toolsets.clone(),
        client_tools: spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
            })
            .map(|descriptor| AgentClientToolView {
                name: descriptor.id.clone(),
                description: descriptor.description.clone(),
                input_schema: descriptor.parameters.clone(),
            })
            .collect(),
        mcp_servers: bindings
            .mcp_servers
            .into_iter()
            .map(|server| AgentMcpServerView {
                name: server.name,
                url: server.url,
                prompts_as_skills: server.prompts_as_skills,
                credential_source_id: server
                    .credential
                    .as_ref()
                    .map(|credential| credential.id.clone()),
                credential_revision: server
                    .credential
                    .as_ref()
                    .map(|credential| credential.revision),
            })
            .collect(),
        skills: bindings.skills,
        delegate_ids: bindings
            .delegates
            .into_iter()
            .map(|binding| binding.agent_id.0)
            .collect(),
        resources: defaults.inputs,
        environment: defaults
            .environment
            .map(|binding| AgentEnvironmentBindingView {
                environment_id: binding.environment_id,
                revision: binding.revision,
            }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::{
        AgentEnvironmentBinding, AgentInputConfig, BindingId, FileId, InputBinding,
        InputResourceId, ResourceAccess,
    };
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_runtime_contract::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, ResolutionManifest, ResolvedInputRef,
    };

    #[test]
    fn session_defaults_revision_is_exact_or_fails_closed() {
        // Cause/effect decision table:
        // P1 publication pins defaults rev2 + current rev2 -> project bindings;
        // P2 current rev3 or P3 missing current -> no current registration view;
        // P4 historical projection -> executable fields retained, defaults empty.
        let mut snapshot = ExecutableAgentSnapshot::builder("agent-a")
            .fingerprint("fp")
            .build();
        snapshot.metadata = AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId("agent-a".into()),
                revision: 4,
            },
            publication_version: AgentPublicationVersion("v4".into()),
            resolution: ResolutionManifest::new([ResolvedInputRef {
                kind: "agent_session_defaults".into(),
                id: "agent-a".into(),
                version: ResolvedInputVersion::Revision(2),
            }])
            .unwrap(),
            fingerprint: AgentSnapshotFingerprint("fp".into()),
        };
        let defaults = |revision| AgentInputConfig {
            agent_id: "agent-a".into(),
            environment: Some(AgentEnvironmentBinding {
                environment_id: "env-a".into(),
                revision: 9,
            }),
            inputs: vec![InputBinding {
                binding_id: BindingId::from("input-a"),
                target: InputResourceId::File(FileId::from("file-a")),
                mount_path: "/workspace/a".into(),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            }],
            revision,
        };
        let model = ModelSelection::Pinned(spec_model(&snapshot));
        let exact = registered_agent_view(&snapshot, &model, Some(defaults(2))).unwrap();
        assert_eq!(exact.resources.len(), 1, "P1");
        assert!(
            registered_agent_view(&snapshot, &model, Some(defaults(3))).is_none(),
            "P2"
        );
        assert!(
            registered_agent_view(&snapshot, &model, None).is_none(),
            "P3"
        );
        assert!(
            historical_agent_view(&snapshot, &model)
                .resources
                .is_empty(),
            "P4"
        );
    }

    fn spec_model(snapshot: &ExecutableAgentSnapshot) -> awaken_runtime_contract::ModelBinding {
        snapshot.resolved_spec.model_binding.binding.clone()
    }
}
