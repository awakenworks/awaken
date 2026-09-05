use super::tests::{config, tool};
use super::*;
use crate::config::ModelSelection;

#[test]
fn acp_alias_publication_decision_table() {
    // C1 alias intent; C2 catalog capability frozen in the candidate;
    // C3 description/exposure requested; C4 fallback capability; C5 duplicate
    // target. R1 !C1 => normal publication, R2 C1+C2+!C3 => publish,
    // R3 C1+!C2 => reject, R4 C3 => reject, R5 !C4 => reject, R6 C5 => reject.
    // This covers both managed/BackendOwned through the shared profile gate;
    // no CLI-name branch is allowed in compiler/UI.
    use crate::config::ToolOverride;
    use awaken_runtime_contract::resolved::{BackendModelSelection, ModelBinding};
    let binding = ModelBinding::new("identity", "", "acp:claude");
    let candidate = |supported| {
        ResolvedModelCandidate::try_backend_owned(
            binding.clone(),
            awaken_runtime_contract::CredentialRef {
                id: "identity".into(),
                revision: 1,
            },
            BackendModelSelection::Default,
            "0.69.0",
            "sha256:profile",
            Default::default(),
        )
        .unwrap()
        .with_acp_tool_alias_support(supported)
    };
    let mut cfg = config(&["write"]);
    // Exact identity of authoring and publication must agree.
    cfg.model_binding = ModelSelection::pinned("identity", "", "acp:claude");
    let tools = [tool("write")];
    let publish = |cfg: &AgentConfig, supported| {
        compile_published(
            cfg,
            &tools,
            AgentSnapshotMetadata::default(),
            candidate(supported),
            vec![],
            None,
        )
    };
    assert!(publish(&cfg, false).is_ok());
    cfg.tool_overrides = vec![ToolOverride {
        target: "write".into(),
        alias: Some("Write".into()),
        description: None,
        exposure: None,
    }];
    assert!(publish(&cfg, true).is_ok());
    assert!(matches!(
        publish(&cfg, false),
        Err(CompileError::InvalidToolOverride { .. })
    ));
    cfg.tool_overrides[0].description = Some("different".into());
    assert!(matches!(
        publish(&cfg, true),
        Err(CompileError::InvalidToolOverride { .. })
    ));
    cfg.tool_overrides[0].description = None;
    let mut managed_cfg = cfg.clone();
    managed_cfg.model_binding = ModelSelection::pinned("identity", "model", "acp:claude");
    let managed = |supported| {
        ResolvedModelCandidate::try_provider_with_acp(
            ModelBinding::new("identity", "model", "acp:claude"),
            "provider@1",
            "route@1",
            "workspace",
            None,
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "http://127.0.0.1:1".into(),
                upstream_model: "test".into(),
                processing_placement: None,
            },
            awaken_runtime_contract::resolved::AcpExecutionProfile {
                supports_tool_aliases: supported,
                capability_adapter_version: "0.69.0".into(),
                capability_fingerprint: "sha256:profile".into(),
                session_configuration: Default::default(),
            },
        )
        .unwrap()
    };
    for supported in [false, true] {
        let result = compile_published(
            &managed_cfg,
            &tools,
            AgentSnapshotMetadata::default(),
            managed(supported),
            vec![],
            None,
        );
        assert_eq!(result.is_ok(), supported, "managed provisioning");
    }
    cfg.model_fallbacks = vec![binding.clone()];
    assert!(matches!(
        compile_published(
            &cfg,
            &tools,
            AgentSnapshotMetadata::default(),
            candidate(true),
            vec![candidate(false)],
            None
        ),
        Err(CompileError::InvalidToolOverride { .. })
    ));
    cfg.model_fallbacks.clear();
    cfg.tool_overrides.push(cfg.tool_overrides[0].clone());
    assert!(matches!(
        publish(&cfg, true),
        Err(CompileError::InvalidToolOverride { .. })
    ));
}
