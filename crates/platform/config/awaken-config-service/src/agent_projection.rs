// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: This source implements the repository-controlled agent profile product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Pure projection from one immutable publication into the existing neutral
//! Session configuration view carried by executable registration.

use awaken_agent_config::AgentConfig;
use awaken_config_resolver::AgentInputConfig;
use awaken_executable_agent_contract::{
    ExecutableAgentDelegate, ExecutableAgentEnvironment, ExecutableAgentMcpServer,
    ExecutableAgentSessionProfile, ManagedModelIdError, PublishedModelReferenceProjector,
    project_published_model_reference,
};
use awaken_runtime_contract::{ExecutableAgentSnapshot, ResolvedInputVersion};

/// Build the Session-facing projection from the exact publication and the
/// current Agent-default inputs. A pinned defaults revision mismatch fails
/// closed so Control cannot register a mixed-revision application view.
pub(crate) fn registered_session_profile(
    snapshot: &ExecutableAgentSnapshot,
    authored: &AgentConfig,
    authored_model_selection: &awaken_agent_config::ModelSelection,
    projector: &dyn PublishedModelReferenceProjector,
    current_defaults: Option<AgentInputConfig>,
) -> Result<Option<ExecutableAgentSessionProfile>, ManagedModelIdError> {
    project(
        snapshot,
        authored,
        authored_model_selection,
        projector,
        current_defaults,
        true,
    )
}

/// Historical exact revisions remain addressable for replay even when the
/// current defaults repository no longer retains their old value. They never
/// become current during reconciliation: newest registrations are submitted
/// first, and this projection intentionally carries no guessed defaults.
pub(crate) fn historical_session_profile(
    snapshot: &ExecutableAgentSnapshot,
    authored: &AgentConfig,
    authored_model_selection: &awaken_agent_config::ModelSelection,
    projector: &dyn PublishedModelReferenceProjector,
) -> Result<ExecutableAgentSessionProfile, ManagedModelIdError> {
    project(
        snapshot,
        authored,
        authored_model_selection,
        projector,
        None,
        false,
    )?
    .ok_or(ManagedModelIdError::UnsupportedSelection)
}

fn project(
    snapshot: &ExecutableAgentSnapshot,
    authored: &AgentConfig,
    authored_model_selection: &awaken_agent_config::ModelSelection,
    projector: &dyn PublishedModelReferenceProjector,
    current_defaults: Option<AgentInputConfig>,
    require_exact_defaults: bool,
) -> Result<Option<ExecutableAgentSessionProfile>, ManagedModelIdError> {
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
        (Some(_), _) if require_exact_defaults => return Ok(None),
        _ => AgentInputConfig {
            agent_id: snapshot.root_agent_id.0.clone(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    let binding_fallback = || {
        crate::render_managed_model_id(&awaken_agent_config::ModelSelection::Pinned(
            spec.model_binding.binding().clone(),
        ))
        .ok()
    };
    let managed_model = match crate::render_managed_model_id(authored_model_selection) {
        Ok(reference) => Some(reference),
        Err(_)
            if matches!(
                authored_model_selection,
                awaken_agent_config::ModelSelection::Auto
            ) =>
        {
            project_published_model_reference(&spec.model_binding, projector)?
                .or_else(binding_fallback)
        }
        Err(_) => binding_fallback(),
    };
    Ok(Some(ExecutableAgentSessionProfile {
        name: authored.name.clone(),
        description: authored.description.clone(),
        source_revision: snapshot.metadata.source.revision,
        model: managed_model,
        inference: spec.plugin_config.inference.clone(),
        execution_model_ref: Some(spec.model_binding.binding().identity().1.to_owned()),
        backend_ref: spec.model_binding.identity().2.to_owned(),
        system: (!spec.instructions.is_empty()).then(|| spec.instructions.clone()),
        tool_ids: spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| {
                !matches!(
                    descriptor.kind,
                    awaken_runtime_contract::resolved::ToolKind::ClientExecuted
                        | awaken_runtime_contract::resolved::ToolKind::Advisor
                )
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
            .map(|descriptor| awaken_agent_contract::ClientToolDescriptor {
                name: descriptor.id.clone(),
                description: descriptor.description.clone(),
                input_schema: descriptor.model_parameters(),
            })
            .collect(),
        mcp_servers: bindings
            .mcp_servers
            .into_iter()
            .map(|server| ExecutableAgentMcpServer {
                name: server.name,
                target: server
                    .transport
                    .normalize()
                    .expect("compiled MCP transport is normalized"),
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
        delegates: bindings
            .delegates
            .into_iter()
            .map(|binding| ExecutableAgentDelegate {
                agent_id: binding.agent_id.0,
                source_revision: binding.source_revision,
            })
            .collect(),
        advisor_model: bindings.advisor.map(|advisor| advisor.model),
        resources: defaults.inputs,
        environment: defaults
            .environment
            .map(|binding| ExecutableAgentEnvironment {
                environment_id: binding.environment_id,
                revision: binding.revision,
            }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::{
        AgentEnvironmentBinding, AgentInputConfig, BindingId, FileId, InputBinding,
        InputResourceId, ResourceAccess,
    };
    use awaken_runtime_contract::resolved::ModelBinding;
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
            .model(ModelBinding::new("test", "model", "native"))
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
        let authored = AgentConfig {
            model_binding: awaken_agent_config::ModelSelection::Pinned(spec_model(&snapshot)),
            name: Some("Agent A".into()),
            description: Some("Published identity".into()),
            ..Default::default()
        };
        let exact = registered_session_profile(
            &snapshot,
            &authored,
            &authored.model_binding,
            &awaken_executable_agent_contract::CanonicalPublishedModelReferenceProjector,
            Some(defaults(2)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(exact.resources.len(), 1, "P1");
        assert_eq!(exact.name.as_deref(), Some("Agent A"), "P1 identity");
        assert_eq!(exact.source_revision, 4, "P1 version");
        assert!(
            registered_session_profile(
                &snapshot,
                &authored,
                &authored.model_binding,
                &awaken_executable_agent_contract::CanonicalPublishedModelReferenceProjector,
                Some(defaults(3)),
            )
            .unwrap()
            .is_none(),
            "P2"
        );
        assert!(
            registered_session_profile(
                &snapshot,
                &authored,
                &authored.model_binding,
                &awaken_executable_agent_contract::CanonicalPublishedModelReferenceProjector,
                None
            )
            .unwrap()
            .is_none(),
            "P3"
        );
        assert!(
            historical_session_profile(
                &snapshot,
                &authored,
                &authored.model_binding,
                &awaken_executable_agent_contract::CanonicalPublishedModelReferenceProjector
            )
            .unwrap()
            .resources
            .is_empty(),
            "P4"
        );
    }

    #[test]
    fn auto_uses_frozen_source_identity_while_authored_selectors_remain_intent() {
        // A1 Auto + valid frozen source -> complete public primary, never its
        // private account. A2 Profile/Target/Pinned -> exact authored selector.
        // A3 invalid source -> typed error for Auto current AND historical
        // views, while A2 still needs no source projection. No live resolver is
        // available in this pure test; recovery's test owns the no-resolve gate.
        use awaken_agent_config::ModelSelection;
        let snapshot = |route: &str| {
            let candidate =
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_brokered_provider(
                    ModelBinding::parse("private-account", "chat", "gateway").unwrap(),
                    "public-vendor@2",
                    route,
                    "workspace",
                    awaken_runtime_contract::InferenceEndpoint {
                        adapter_kind: "openai".into(),
                        api_dialect: "open_ai_chat".into(),
                        base_url: "https://provider.example.invalid".into(),
                        upstream_model: "chat".into(),
                        processing_placement: None,
                    },
                )
                .unwrap();
            ExecutableAgentSnapshot::builder("agent")
                .resolved_model(candidate)
                .build()
        };
        let valid = snapshot("public-endpoint@4");
        let invalid = snapshot("unowned-opaque-route");
        let projector =
            &awaken_executable_agent_contract::CanonicalPublishedModelReferenceProjector;
        for (selection, expected) in [
            (
                ModelSelection::Auto,
                "chat;provider=public-vendor;api=open_ai_chat;endpoint=public-endpoint",
            ),
            (
                ModelSelection::Profile {
                    profile_id: "policy".into(),
                },
                "profile=policy",
            ),
            (
                crate::parse_managed_model_id("chat;provider=authored-vendor").unwrap(),
                "chat;provider=authored-vendor",
            ),
            (
                ModelSelection::pinned("authored-source", "authored-model", "genai").unwrap(),
                "authored-model;provider=authored-source",
            ),
        ] {
            let authored = AgentConfig {
                model_binding: selection,
                ..Default::default()
            };
            let current = registered_session_profile(
                &valid,
                &authored,
                &authored.model_binding,
                projector,
                None,
            )
            .unwrap()
            .unwrap();
            assert_eq!(current.model.as_deref(), Some(expected), "A1/A2");
            assert_eq!(current.execution_model_ref.as_deref(), Some("chat"));
            assert_eq!(current.backend_ref, "gateway");
            assert_eq!(
                historical_session_profile(&valid, &authored, &authored.model_binding, projector)
                    .unwrap(),
                current
            );
            let current_bad = registered_session_profile(
                &invalid,
                &authored,
                &authored.model_binding,
                projector,
                None,
            );
            let historical_bad =
                historical_session_profile(&invalid, &authored, &authored.model_binding, projector);
            if matches!(authored.model_binding, ModelSelection::Auto) {
                assert!(current_bad.is_err(), "A3 current");
                assert!(
                    historical_bad.is_err(),
                    "A3 historical is fallible, never panic"
                );
            } else {
                assert_eq!(
                    current_bad.unwrap().unwrap().model.as_deref(),
                    Some(expected),
                    "A2"
                );
                assert_eq!(
                    historical_bad.unwrap().model.as_deref(),
                    Some(expected),
                    "A2 historical"
                );
            }
        }
    }

    fn spec_model(snapshot: &ExecutableAgentSnapshot) -> awaken_runtime_contract::ModelBinding {
        snapshot.resolved_spec.model_binding.binding().clone()
    }
}
