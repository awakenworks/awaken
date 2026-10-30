// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-realization/src/environment_holding/lifecycle.rs
// @formal reason: Validate exact retained adoption sources without publishing Candidates or replacing cleanup owners.
use super::*;

#[tokio::test]
async fn adoption_validation_retains_one_exact_wrapper_without_publishing_it() {
    // V1 vacant, V2 Resident, V3 hidden Candidate, V4 retiring Candidate.
    // Validation returns only the selected original Arc. It cannot publish a
    // Candidate, erase a retirement, or observe/adopt another physical owner.
    let thread = "adoption-source-selection";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_provider_roles();
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
    let candidate = UnboundSessionEnvironment {
        origin: UnboundSessionEnvironmentOrigin::Adoption,
        intent: None,
        binding: binding.clone(),
        environment: environment.clone(),
    };
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("validated Host dependencies");
    for (case, owner, retained) in [
        ("V1", SessionEnvironmentOwner::Vacant, false),
        ("V2", resident(thread, environment.clone()), true),
        (
            "V3",
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                candidate.clone(),
            )),
            true,
        ),
        (
            "V4",
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Unbound(candidate.clone()),
            }),
            true,
        ),
    ] {
        host.run_assembly
            .installed_sessions
            .install_realization(thread, |r| {
                r.environment_mut().owner = owner.clone();
            });
        let validated = host
            .realization_executor
            .environments()
            .validated_session_environment_adoption(
                host.realization_executor.peers(),
                thread,
                &binding,
                &provider,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            validated.retained_environment().is_some(),
            retained,
            "{case}"
        );
        if retained {
            assert!(
                Arc::ptr_eq(validated.retained_environment().unwrap(), &environment),
                "{case}"
            );
        }
        let actual = host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| r.environment().owner.clone())
            .unwrap()
            .unwrap();
        assert_eq!(
            std::mem::discriminant(&actual),
            std::mem::discriminant(&owner),
            "{case}"
        );
        if case == "V3" {
            assert!(
                matches!(actual, SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(ref observed))
                if observed.exact_matches(&candidate)),
                "V3 remains unpublished"
            );
        }
        if case == "V4" {
            assert!(
                matches!(actual, SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                owned: RetiringEnvironmentOwner::Unbound(ref observed), ..
            }) if observed.exact_matches(&candidate)),
                "V4 keeps cleanup ownership"
            );
        }
    }
}
