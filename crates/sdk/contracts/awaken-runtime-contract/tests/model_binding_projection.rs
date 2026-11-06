// @formal evidence model_binding.constructor_and_wire_share_one_admission
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, Error};
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, NoDispatchProof};
use awaken_runtime_contract::resolved::ModelBinding;

#[test]
fn upstream_model_projection_reuses_joint_admission_and_preserves_the_original_request() {
    // Cause/effect graph: C1 Native / ACP / A2A admitted binding; C2 canonical
    // nonempty / empty / padded wire model. A1 all C1+canonical => exact new
    // model, SAME provider/backend, original request unchanged. A2 Native+
    // empty or any C1+padded => Binding error + NotDispatched(RequestProjection).
    // A3 ACP/A2A+empty => preserve the parser's licensed default, no invented
    // Native-only restriction. Both accepted copies and originals round-trip
    // through the unchanged three-field wire. M1-M3 compile-fail tests beside
    // ModelBinding own post-admission write exclusion; adapter tests own zero
    // actual provider/grant calls after A2. This pure projection performs no I/O.
    for (provider, model, backend, native) in [
        ("principal", "published-model", "native", true),
        ("principal", "published-model", "acp:codex", false),
        ("", "", "a2a:https://example.invalid", false),
    ] {
        let request = ChatRequest {
            model_binding: ModelBinding::parse(provider, model, backend).unwrap(),
            inference: Default::default(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("原始请求必须保留")],
            }],
            tools: vec![],
        };
        let before = request.clone();
        let original_wire = serde_json::json!({
            "provider_identity_ref": provider, "model_ref": model, "backend_ref": backend,
        });
        assert_eq!(
            serde_json::to_value(&request.model_binding).unwrap(),
            original_wire
        );
        for alias in ["wire-model", "", " wire-model", "wire-model "] {
            let projected = request.project_upstream_model(alias);
            let valid =
                !alias.starts_with(' ') && !alias.ends_with(' ') && (!native || !alias.is_empty());
            if valid {
                let binding = projected.unwrap();
                assert_eq!(binding.identity(), (provider, alias, backend), "A1/A3");
                let wire = serde_json::to_value(&binding).unwrap();
                assert_eq!(
                    wire,
                    serde_json::json!({
                        "provider_identity_ref": provider, "model_ref": alias, "backend_ref": backend,
                    })
                );
                let restored: ModelBinding = serde_json::from_value(wire).unwrap();
                assert_eq!(restored, binding);
            } else {
                let error = projected.unwrap_err();
                assert!(matches!(error.classification(), Error::Binding(_)), "A2");
                assert_eq!(
                    error.execution_evidence(),
                    ModelExecutionEvidence::NotDispatched(NoDispatchProof::request_projection(),),
                    "A2 exact pre-call proof"
                );
            }
            assert_eq!(
                request, before,
                "every row preserves original ownership and content"
            );
        }
    }
}
