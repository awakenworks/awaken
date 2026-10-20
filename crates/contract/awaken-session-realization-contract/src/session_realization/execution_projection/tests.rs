//! Shared admission/placement/realization laws require no Host or provider.
use super::super::FrozenAgentPublicationDecision;
use super::*;
use crate::{SessionBaselineInputs, SessionRuntimePlacement};
use awaken_runtime_contract::resolved::{BackendModelSelection, ModelBinding};
use awaken_session_configuration_contract::SessionModelPublication;

fn candidate(kind: &str) -> ResolvedModelCandidate {
    match kind {
        "remote" => ResolvedModelCandidate::try_remote(
            ModelBinding::parse("", "", "a2a:https://agent.example.test")
                .expect("valid remote model fixture"),
            "workspace",
            None,
            "transport-pin",
        )
        .unwrap(),
        "owned" => ResolvedModelCandidate::try_backend_owned(
            ModelBinding::parse("credential", "model", "acp:cli")
                .expect("valid backend model fixture"),
            awaken_agent_contract::credential::CredentialRef {
                id: "credential".into(),
                revision: 1,
            },
            BackendModelSelection::Exact,
            "adapter-v1",
            "sha256:capabilities",
            Default::default(),
        )
        .unwrap(),
        _ => ResolvedModelCandidate::host(
            ModelBinding::parse(
                "provider",
                "model",
                if kind == "acp" { "acp:cli" } else { "native" },
            )
            .expect("valid projected model fixture"),
        ),
    }
}

fn baseline() -> SessionBaseline {
    SessionBaseline::compile(SessionBaselineInputs {
        environment: awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "projection-environment".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "projection-environment-v1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        },
        runtime_placement: SessionRuntimePlacement::Local,
        mcp_authoring: Default::default(),
        agent_id: "agent".into(),
        agent_revision: Some(2),
        model: "provider/model".into(),
        model_override: None,
        runtime: None,
        delegate_ids: Vec::new(),
        toolsets: Vec::new(),
        mounts: Vec::new(),
        env: Vec::new(),
        prompts: Vec::new(),
        transcript_prefix: None,
    })
}

#[test]
fn complete_candidate_set_decides_local_and_opaque_requirements_independent_of_order() {
    // Native and both ACP modes need an Environment. Only projected ACP needs
    // opaque-process path fidelity. A Remote primary cannot hide a local/ACP
    // fallback, and a Native primary cannot hide a later projected ACP.
    for (kinds, local, opaque) in [
        (vec!["remote"], false, false),
        (vec!["native"], true, false),
        (vec!["acp"], true, true),
        (vec!["owned"], true, false),
        (vec!["remote", "native"], true, false),
        (vec!["remote", "acp"], true, true),
        (vec!["native", "owned"], true, false),
        (vec!["owned", "acp"], true, true),
    ] {
        for reverse in [false, true] {
            let mut values: Vec<_> = kinds.iter().map(|kind| candidate(kind)).collect();
            if reverse {
                values.reverse();
            }
            let primary = values.remove(0);
            let publication = ExecutableAgentSnapshot::builder("agent")
                .resolved_model(primary)
                .resolved_model_candidates(values)
                .build();
            assert_eq!(
                requires_local_environment(&publication.resolved_spec),
                local,
                "{kinds:?}/{reverse}"
            );
            assert_eq!(
                model_candidates_require_opaque_process(
                    std::iter::once(&publication.resolved_spec.model_binding)
                        .chain(publication.resolved_spec.model_candidates.iter())
                ),
                opaque,
                "{kinds:?}/{reverse}"
            );
        }
    }
}

#[test]
fn frozen_override_replaces_the_entire_candidate_set_and_inference_only_inherits_it() {
    // Complete overrides replace Agent execution candidates. Inference-only
    // selection inherits them; absent publications cannot invent a local ACP
    // cause. Each source is crossed with ACP only in a fallback.
    for agent_opaque in [false, true] {
        let publication = ExecutableAgentSnapshot::builder("agent")
            .resolved_model(candidate("native"))
            .resolved_model_candidates(agent_opaque.then(|| candidate("acp")))
            .build();
        for source in [
            "none",
            "inference",
            "native",
            "acp_fallback",
            "owned_fallback",
        ] {
            let mut frozen = baseline();
            frozen.model_override = match source {
                "none" => None,
                "inference" => Some(SessionModelOverride {
                    publication: None,
                    inference: Default::default(),
                }),
                _ => Some(SessionModelOverride {
                    publication: Some(Box::new(SessionModelPublication {
                        primary: candidate("native"),
                        candidates: match source {
                            "acp_fallback" => vec![candidate("acp")],
                            "owned_fallback" => vec![candidate("owned")],
                            _ => Vec::new(),
                        },
                    })),
                    inference: Default::default(),
                }),
            };
            let expected = match source {
                "none" | "inference" => agent_opaque,
                "acp_fallback" => true,
                _ => false,
            };
            assert_eq!(
                frozen_session_requires_opaque_process(Some(&publication), Some(&frozen)),
                expected,
                "{source}/{agent_opaque}"
            );
            assert_eq!(
                frozen_session_requires_opaque_process(None, Some(&frozen)),
                source == "acp_fallback",
                "missing/{source}"
            );
        }
        assert_eq!(
            frozen_session_requires_opaque_process(Some(&publication), None),
            agent_opaque
        );
    }
    assert!(!frozen_session_requires_opaque_process(None, None));
}

#[test]
fn canonical_projection_preserves_coordinate_sources_and_optional_publication_rules() {
    // Exact baseline and prospective-layout adapters obey the same closed
    // decision table; Worker+pin requires a publication while Local+pin can
    // remain optional. A legacy caller has neither pin nor model override.
    for (placement, present, revision, expected) in [
        (
            SessionRuntimePlacement::Local,
            false,
            2,
            FrozenAgentPublicationDecision::OptionalMissing,
        ),
        (
            SessionRuntimePlacement::Worker,
            false,
            2,
            FrozenAgentPublicationDecision::MissingRequired,
        ),
        (
            SessionRuntimePlacement::Worker,
            true,
            2,
            FrozenAgentPublicationDecision::Exact,
        ),
        (
            SessionRuntimePlacement::Local,
            true,
            3,
            FrozenAgentPublicationDecision::Mismatch,
        ),
    ] {
        let mut frozen = baseline();
        frozen.runtime_placement = placement;
        let layout = SessionSandboxLayout {
            workspace_id: "workspace".into(),
            agent_id: frozen.agent_id.clone(),
            agent_revision: frozen.agent_revision,
            runtime_placement: placement,
            model_override: frozen.model_override.clone(),
            runtime: frozen.runtime.clone(),
            mounts: Vec::new(),
            env: Vec::new(),
            environment: frozen.environment.clone(),
            resources: Vec::new(),
        };
        let publication = present.then(|| {
            let mut value = ExecutableAgentSnapshot::builder("agent")
                .resolved_model(candidate("native"))
                .build();
            value.metadata.source.agent_id = value.root_agent_id.clone();
            value.metadata.source.revision = revision;
            value.recompute_fingerprint().unwrap();
            value
        });
        for coordinates in [
            CanonicalSessionProjection::Baseline(&frozen),
            CanonicalSessionProjection::Layout(&layout),
        ] {
            assert_eq!(coordinates.agent(), "agent");
            assert_eq!(coordinates.agent_revision(), Some(2));
            assert!(coordinates.model_override().is_none());
            assert_eq!(
                coordinates.publication_decision(publication.as_ref()),
                expected
            );
        }
    }
    let legacy = CanonicalSessionProjection::LegacyAgent("legacy");
    assert_eq!(legacy.agent(), "legacy");
    assert_eq!(legacy.agent_revision(), None);
    assert!(legacy.model_override().is_none());
    assert_eq!(
        legacy.publication_decision(None),
        FrozenAgentPublicationDecision::Unpinned
    );
}
