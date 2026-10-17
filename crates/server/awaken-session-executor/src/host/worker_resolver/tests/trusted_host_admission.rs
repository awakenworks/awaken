//! Trusted-host Session environment admission: a permanent refusal is classified before any provider effect.
use super::*;

#[tokio::test]
async fn trusted_host_admission_rejection_preserves_permanent_class_without_effects() {
    // Cause/effect graph: explicit trusted-host provider + Namespace spec ->
    // deterministic PrepareError; exact trusted-host Workdir spec -> admission.
    // T1 the incompatible spec must be BadRequest (never endlessly retried),
    // T2 the compatible spec remains valid, T3 neither pure check creates a
    // Sandbox or calls a model. Runtime/transport failures are covered by the
    // existing realization disposition table and the claim projection below.
    let root = tempfile::tempdir().expect("provider root");
    let host = adoption_host().await;
    let provider = awaken_session_environment::SandboxEnvironmentProvider::
        trusted_host_workdir_with_agent_stderr(root.path(), false)
        .into_session_provider();
    let mut spec =
        awaken_session_configuration_contract::environment_projection::default_session_sandbox_spec(
            "trusted-admission-class",
        );
    spec.isolation = awaken_provisioning_contract::IsolationClass::Namespace;
    spec.network = awaken_provisioning_contract::NetworkPolicy::Unrestricted;
    let rejected = host
        .realizer
        .validate_session_environment_capabilities(&provider, &spec)
        .expect_err("T1 incompatible exact tier");
    assert_eq!(
        rejected.disposition(),
        crate::realization_error::Disposition::Rejected,
        "T1 absorbing admission"
    );
    spec.isolation = awaken_provisioning_contract::IsolationClass::Workdir;
    host.realizer
        .validate_session_environment_capabilities(&provider, &spec)
        .expect("T2 exact Workdir admission");
    assert_eq!(
        std::fs::read_dir(root.path()).expect("T3 root").count(),
        0,
        "T3 no provider effects"
    );
}
