// @formal entrypoint delegation.independent_child_scheduling_lease
//! Durable dispatch construction for a child Run.
//!
//! A delegated child is admitted to the same queue as a root Run. This module
//! owns the one place that freezes a child's publication closure, resource
//! manifest, and backend placement into that durable request.

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::RunDispatch;
use awaken_runtime_contract::activation::RunActivation;

use super::AgentRunError;

pub(crate) fn child_dispatch_request(
    activation: RunActivation,
    parent_thread_id: ThreadId,
    session_resources: Option<crate::session_resource::SessionResourceManifest>,
    publication_source: Option<&dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
) -> Result<RunDispatch, AgentRunError> {
    // Delegated publications are frozen FROM a Workspace, so this coordinate
    // selects whose published Agent a child may inherit. A compiled-in default
    // would let a child freeze another tenant's publication, so an unknown
    // Workspace is refused rather than guessed. A parent that delegates to
    // nothing never consults it, and keeps working without a resource manifest.
    let workspace = match session_resources.as_ref() {
        Some(resources) => resources.workspace_id.as_str(),
        None if activation
            .snapshot
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .is_empty() =>
        {
            ""
        }
        None => {
            return Err(AgentRunError::Configuration(
                "child dispatch cannot freeze delegated publications without the \
                 session's Workspace"
                    .to_string(),
            ));
        }
    };
    let child_publications = crate::agent_catalog::RUN_PUBLICATIONS
        .freeze(&activation.snapshot, publication_source, workspace)
        .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let inference_holder =
        awaken_dispatch_execution::placement::self_hosted_inference_holder(&activation)
            .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let mut placement = awaken_dispatch_execution::placement::remote_worker_placement(
        &activation.snapshot.resolved_spec,
        None,
        session_resources.as_ref(),
        false,
    );
    if activation.inherited_toolsets().is_some() {
        placement
            .required_capabilities
            .insert(awaken_runtime_contract::activation::INHERITED_TOOLSETS_CAPABILITY.to_string());
    }
    let mut request = RunDispatch::new(activation)
        .for_session(parent_thread_id)
        .with_traceparent(awaken_observability::current_traceparent())
        .with_agent_publications(child_publications)
        .with_placement(placement);
    if let Some(holder) = inference_holder {
        request = request.with_inference_plaintext_holder(holder);
    }
    if let Some(resources) = session_resources {
        let envelope = awaken_dispatch_execution::session_envelope::resource_envelope(&resources)
            .map_err(|error| {
            AgentRunError::Configuration(format!(
                "serialize child Session resource manifest: {error}"
            ))
        })?;
        request = request
            .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from(resources.workspace_id.clone()),
            ))
            .with_session_resources(envelope);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime::RunInput;
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};

    use super::child_dispatch_request;
    use crate::agent_runner::fixtures::{agent, user};

    fn published_model(
        model: &str,
        credential: &str,
        provider: &str,
        route: &str,
    ) -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_provider(
            ModelBinding::new(provider, model, "native"),
            provider,
            route,
            "workspace-a",
            Some(
                awaken_agent_contract::credential::CredentialAccess::new(
                    awaken_agent_contract::credential::CredentialRef {
                        id: credential.into(),
                        revision: 0,
                    },
                    awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
                    awaken_agent_contract::credential::CredentialUsage::ProviderAdapter,
                    awaken_agent_contract::credential::CredentialExecutionPolicy::self_hosted_provider(),
                )
                .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                    awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
                    provider.split_once('@').map_or(provider, |(id, _)| id),
                )),
            ),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: "test".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
                processing_placement: None,
            },
        )
        .expect("coherent published test model")
    }

    #[test]
    fn child_dispatch_reuses_publication_pinned_model_candidates() {
        // Cause/effect graph and decision table:
        // C1 the child publication carries credentialed Native primary and
        // fallback candidates; C2 it inherits a frozen Session resource
        // manifest. E1 both exact candidates remain frozen; E2 the manifest is
        // carried in the parent's execution scope; E3 credential admission is
        // pinned to the canonical Worker plaintext holder.
        // R1=C1+C2 => E1+E2+E3 is covered here. The credential-free remote rule
        // is covered below. FMECA: omitting E3 lets the child reach the queue but
        // makes every credential-aware claim fail closed; the holder assertion
        // detects that parent/child dispatch-contract divergence.
        let mut config = agent("child", "child");
        let primary = published_model("primary", "cred-a", "provider-a@1", "route-a@1");
        let fallback = published_model("fallback", "cred-b", "provider-b@2", "route-b@3");
        config.resolved_spec.model_binding = primary.clone();
        config.resolved_spec.model_candidates = vec![fallback.clone()];
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );

        let manifest =
            awaken_session_configuration_contract::resource::SessionResourceManifest::new(
                "workspace-a",
                awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                    Vec::new(),
                    vec![
                        awaken_session_configuration_contract::resource::ResolvedSkillBinding {
                            kind: awaken_agent_contract::AgentSkillKind::Custom,
                            skill_id: "skill-a".into(),
                            version: 3,
                            bundle_sha256: "sha256:skill-a-v3".into(),
                        },
                    ],
                )
                .unwrap(),
            );
        let request = child_dispatch_request(
            activation,
            ThreadId("parent-thread".to_string()),
            Some(manifest.clone()),
            None,
        )
        .expect("build child dispatch");

        assert_eq!(
            request.activation.snapshot.resolved_spec.model_binding,
            primary
        );
        assert_eq!(
            request.activation.snapshot.resolved_spec.model_candidates,
            vec![fallback]
        );
        assert_eq!(request.session_thread_id.unwrap().0, "parent-thread");
        let carried = request
            .session_resources
            .as_ref()
            .expect("resource envelope")
            .decode_with(
                awaken_session_configuration_contract::resource::SessionResourceManifest::decode_resources,
            )
            .expect("decode resource envelope");
        assert_eq!(carried, manifest);
        assert!(
            request
                .placement
                .required_capabilities
                .contains(awaken_dispatch_contract::SESSION_RESOURCES_CAPABILITY)
        );
        assert_eq!(
            request.inference_plaintext_holder,
            Some(
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
                    .inference_holder
            ),
            "R1/E3"
        );
    }

    #[test]
    fn child_dispatch_freezes_nested_and_auxiliary_targets_from_one_source() {
        // Cause/effect graph: C1 a child publication delegates to a nested Agent;
        // C2 the same child selects a custom Memory auxiliary; C3 both targets
        // exist/one is absent in the inherited attempt source. Effects: E1 one
        // child dispatch freezes both exact non-root targets; E2 an incomplete
        // source fails before queue admission. Rules P1=C1+C2+complete=>E1 and
        // P2=C1+C2+missing=>E2. C4 the Session Workspace is known: P1/P2
        // include its frozen manifest; P3 missing Workspace rejects admission
        // even with a complete publication source. No consumer may guess it.
        use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};

        let nested = agent("nested-child", "nested");
        let auxiliary = awaken_ext_memory::memory_agent(
            "child-memory-extractor",
            nested.resolved_spec.model_binding.clone(),
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        );
        let config = awaken_runtime_contract::ExecutableAgentSnapshot::builder("child-owner")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "child-memory-extractor",
                    "recall_enabled": false
                }),
            )])
            .agent_bindings(AgentBindings {
                delegates: vec![AgentDelegateBinding {
                    agent_id: nested.root_agent_id.clone(),
                    source_revision: None,
                    recursive_self: false,
                }],
                ..Default::default()
            })
            .model(ModelBinding::new("default", "stub", "native"))
            .build();
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "child-owner-thread".into(),
            RunInput::from(vec![user("go")]),
        );
        let complete = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([
            nested.clone(),
            auxiliary.clone(),
        ])
        .expect("P1 complete attempt source");
        // Delegated publications are frozen FROM a Workspace, so a parent that
        // delegates must carry one; the manifest is where the Session states
        // it. Passing None here made the admission refuse rather than guess.
        let owner = awaken_session_configuration_contract::resource::SessionResourceManifest::new(
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
            awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                Vec::new(),
                Vec::new(),
            )
            .expect("P1 empty owned resources"),
        );
        let error = child_dispatch_request(
            activation.clone(),
            ThreadId("parent-thread".into()),
            None,
            Some(&complete),
        )
        .expect_err("P3 Workspace must not be inferred from publications");
        assert!(error.to_string().contains("Workspace"), "P3: {error}");
        let request = child_dispatch_request(
            activation.clone(),
            ThreadId("parent-thread".into()),
            Some(owner.clone()),
            Some(&complete),
        )
        .expect("P1 child admission");
        assert_eq!(
            request
                .agent_publications
                .iter()
                .map(|snapshot| snapshot.root_agent_id.0.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            [
                nested.root_agent_id.0.as_str(),
                auxiliary.root_agent_id.0.as_str()
            ]
            .into_iter()
            .collect(),
            "P1/E1"
        );

        let incomplete = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([nested])
            .expect("P2 incomplete attempt source shape is valid");
        assert!(
            child_dispatch_request(
                activation,
                ThreadId("parent-thread".into()),
                // Same owner as P1: without it the refusal would be about the
                // missing Workspace, and P2 would pass without ever exercising
                // an incomplete publication source.
                Some(owner),
                Some(&incomplete),
            )
            .is_err(),
            "P2/E2"
        );
    }

    #[test]
    fn child_dispatch_always_carries_backend_placement_without_resources() {
        // Cause/effect graph:
        // C1 the child publication pins a Remote backend; C2 the child has no
        // Session resources; C3 the child is remote-preferred rather than
        // remote-required. E1 admission still requires the A2A executor
        // capability; E2 no resource capability is invented; E3 the canonical
        // current dispatch/runtime protocol is frozen instead of legacy v0.
        //
        // Decision rule P1: C1+C2+C3 => E1+E2+E3. The resource-present/provider case
        // is rule P2 in `child_dispatch_reuses_publication_pinned_model_candidates`.
        // Together they prevent optional resource staging from controlling
        // backend or credential placement. FMECA: a newly-authored v0 child is
        // durably pending but incompatible with every current Worker; the
        // explicit version assertions detect that silent delegation stall.
        let mut config = agent("remote-child", "remote child");
        config.resolved_spec.model_binding = ResolvedModelCandidate::try_remote(
            ModelBinding::new("remote", "", "a2a:https://agent.example"),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "sha256:card",
        )
        .expect("coherent remote child candidate");
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "remote-child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );

        let request = child_dispatch_request(
            activation,
            ThreadId("parent-thread".to_string()),
            None,
            None,
        )
        .expect("build remote child dispatch");

        assert!(
            request
                .placement
                .required_capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY)
        );
        assert!(
            !request
                .placement
                .required_capabilities
                .contains(awaken_dispatch_contract::SESSION_RESOURCES_CAPABILITY)
        );
        assert_eq!(request.placement.contract_version, 1, "P1/E3");
        assert_eq!(request.placement.dispatch_contract_version, 1, "P1/E3");
        assert_eq!(
            request.placement.runtime_protocol_version,
            awaken_dispatch_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
            "P1/E3"
        );
    }
}
