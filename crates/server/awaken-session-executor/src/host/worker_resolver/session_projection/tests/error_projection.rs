//! Actual frozen-projection callers preserve the Resource owner's Run fault.

use super::*;
use crate::SessionExecutionErrorKind;
use awaken_session_configuration_contract::resource::{
    ResolvedSessionResources, ResolvedSkillBinding, SessionResourceManifest,
};

#[tokio::test]
async fn transition_refusals_keep_the_original_fault_and_code() {
    // Causes: C1 bound legacy projection lacks its previous Resource manifest;
    // C2 previous/desired Workspaces contradict. R1=C1 -> Unavailable with the
    // original missing-transition code; R2=C2 -> BadRequest/invalid_request.
    // Both refuse before a per-installed Session or Resource effect is installed.
    // This calls the production consumer rather than only its error helper.
    let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .build()
        .expect("test executor");
    let mut missing = frozen_projection();
    missing.previous_resource_manifest = None;
    missing.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "original-opaque-binding".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let mut foreign = frozen_projection();
    foreign.previous_resource_manifest = Some(SessionResourceManifest::at_revision(
        "foreign-workspace",
        0,
        ResolvedSessionResources::default(),
    ));
    let mut actual = Vec::new();
    for (thread, projection) in [
        ("missing-transition", missing),
        ("foreign-transition", foreign),
    ] {
        let error = host
            .run_assembly
            .install_frozen_session_projection(thread, projection, None, true, None)
            .await
            .expect_err("invalid original transition");
        actual.push((error.kind, error.code));
        assert!(
            host.run_assembly
                .installed_sessions
                .read(thread, |_| ())
                .unwrap()
                .is_none(),
            "transition refusal precedes Session installation"
        );
    }
    assert_eq!(
        actual,
        vec![
            (
                SessionExecutionErrorKind::Unavailable,
                "session_resource_transition_missing".into()
            ),
            (
                SessionExecutionErrorKind::BadRequest,
                "invalid_request".into()
            ),
        ]
    );
}

#[tokio::test]
async fn staging_refusals_keep_the_original_fault_and_code() {
    // R3: exact transition + an unknown public Skill -> the staging owner's
    // NotFound projection (BadRequest/invalid_request), with no Agent or Skill
    // content execution and no publication of the prospective baseline.
    // The missing public Skill is resolved by the real builtin Skill owner;
    // no fake error projector can make this caller-level assertion pass.
    let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .build()
        .expect("test executor");
    let mut projection = frozen_projection();
    projection.resource_revision = 1;
    projection.resources = projection
        .resources
        .with_skills(vec![ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Anthropic,
            skill_id: "missing-public-skill".into(),
            version: 1,
            bundle_sha256: format!("sha256:{}", "a".repeat(64)),
        }])
        .expect("valid binding shape");
    let error = host
        .run_assembly
        .install_frozen_session_projection("missing-stage-skill", projection, None, true, None)
        .await
        .expect_err("the exact Skill is absent");
    assert!(
        error.message.contains("missing-public-skill"),
        "staging was reached: {error}"
    );
    assert_eq!(error.kind, SessionExecutionErrorKind::BadRequest, "R3 kind");
    assert_eq!(error.code, "invalid_request", "R3 code");
    assert!(
        host.run_assembly
            .installed_sessions
            .read("missing-stage-skill", |installed| installed
                .inputs
                .baseline
                .is_none())
            .unwrap()
            .unwrap_or(true),
        "R3 no prospective baseline publication"
    );
}
