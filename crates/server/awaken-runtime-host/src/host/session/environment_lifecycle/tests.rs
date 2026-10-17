//! Session environment lifecycle tests, split by topic; shared fixtures live here.
use super::state::ProjectedEnvironmentOwner;
use super::*;
use crate::environment_owner::UnboundSessionEnvironmentOrigin;
use awaken_session_environment::test_support::SandboxEnvironmentAccess;

#[path = "tests/candidate_adoption.rs"]
mod candidate_adoption;
#[path = "tests/checkpoint_admission.rs"]
mod checkpoint_admission;
#[path = "tests/owner_absorption.rs"]
mod owner_absorption;
#[path = "tests/recovery.rs"]
mod recovery;
#[path = "tests/revocation_retry.rs"]
mod revocation_retry;
#[path = "tests/terminal_background.rs"]
mod terminal_background;

fn environment_effect_fence(thread: &str) -> awaken_provisioning_contract::SandboxEffectFence {
    awaken_provisioning_contract::SandboxEffectFence::new(
        format!("environment-lifecycle-fixture:{thread}"),
        "environment-lifecycle-tests",
        "environment-lifecycle-tests",
        1,
        u64::MAX,
    )
    .expect("test Environment effect fence")
}

async fn environment(
    provider: &crate::session_environment::SessionEnvironmentProvider,
    thread: &str,
) -> Arc<crate::session_environment::SessionEnvironment> {
    // Fixture cause/effect rule: C1 lifecycle tests later perform exact,
    // spec-aware adoption; therefore C2 creation must carry the same current
    // Realization fingerprint and incarnation evidence as production. E1 use
    // the canonical fenced provider edge and emit one V2 handle. A legacy
    // unfenced create would test the decode-only V1 compatibility path instead.
    let effect_fence = environment_effect_fence(thread);
    Arc::new(
        provider
            .realizer()
            .create_effective_for_effect(
                &crate::sandbox_projection::agent_run_sandbox_spec(thread),
                crate::session_environment::ProviderEffect::Fenced {
                    fence: &effect_fence,
                    authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &effect_fence,
                    ),
                },
                None,
                awaken_session_environment_contract::EnvironmentRealizationKind::Create,
            )
            .await
            .expect("test Environment"),
    )
}

fn disposal_authorization(
    prepared: awaken_provisioning_contract::SandboxEffectFence,
    preparation_fingerprint: &str,
) -> awaken_provisioning_contract::SandboxDisposalAuthorization {
    let preparation = awaken_provisioning_contract::SandboxDisposalPreparation::new(
        prepared.clone(),
        preparation_fingerprint,
    )
    .unwrap();
    let current = awaken_provisioning_contract::SandboxEffectFence::new(
        preparation.operation_id().unwrap(),
        prepared.owner(),
        prepared.runtime_incarnation(),
        prepared.epoch(),
        prepared.expires_at_unix_ms(),
    )
    .unwrap();
    preparation.authorize(current).unwrap()
}

async fn dispose_current_environment(
    environment: &crate::session_environment::SessionEnvironment,
    thread: &str,
) {
    // Fixture cause/effect rule: C1 a current V2 Environment may not cross the
    // legacy unfenced disposal edge; C2 provider preparation must retain the
    // exact create-effect predecessor. E1 prepare that current predecessor,
    // derive the typed aggregate authorization, then perform physical disposal.
    let prepared = environment
        .lifecycle()
        .prepare_disposal_for_effect(
            &environment_effect_fence(thread),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                &environment_effect_fence(thread),
            ),
        )
        .await
        .expect("prepare current Environment disposal");
    let authorization = disposal_authorization(
        prepared,
        "environment-lifecycle-current-disposal-preparation",
    );
    environment
        .lifecycle()
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("dispose current Environment through aggregate effect fence");
}

fn generation(thread: &str) -> awaken_session_state_contract::SandboxGeneration {
    awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(thread),
        1,
        u64::MAX,
        "environment",
        "image",
    )
}

fn identity(thread: &str) -> BoundSessionEnvironmentIdentity {
    BoundSessionEnvironmentIdentity::Durable {
        effect_id: format!("effect-{thread}"),
        generation: generation(thread),
    }
}

fn source_disposal_authorization(
    preparation_operation_id: &str,
) -> awaken_provisioning_contract::SandboxDisposalAuthorization {
    let prepared = awaken_provisioning_contract::SandboxEffectFence::new(
        preparation_operation_id,
        "source-disposal-test-owner",
        "source-disposal-test-runtime",
        1,
        u64::MAX,
    )
    .unwrap();
    disposal_authorization(prepared, "source-disposal-test-preparation")
}

fn resident(
    thread: &str,
    environment: Arc<crate::session_environment::SessionEnvironment>,
) -> SessionEnvironmentOwner {
    SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
        identity: identity(thread),
        binding: serde_json::to_string(&environment.lifecycle().handle()).unwrap(),
        environment,
    })
}
