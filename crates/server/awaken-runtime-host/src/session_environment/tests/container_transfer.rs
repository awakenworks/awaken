//! Container environment revocation and rebinding under the Host's Session lifecycle.

use super::*;

#[tokio::test]
async fn revoked_durable_container_rebinds_hand_without_reconstructing_its_owner() {
    use crate::host::{
        eager_environment, empty_frozen_projection_for_snapshot, managed_test_host, test_activation,
    };

    // Causes: C1 exact durable Retiring owner after revocation; C2 typed provider
    // observation unavailable/Ready; C3 attached/resident Hand; C4 repeated
    // revocation; C5 foreign handle/provider. Effects: E1 retain the original
    // owner on failure; E2 same physical Arc, fresh wrapper and usable Hand;
    // E3 no provider adopt/create/dispose, preserving original cleanup/attempt
    // participants; E4 reject mismatched rebinding without a process launch.
    // Rules: W1 C1+unavailable=>E1; W2 C1+Ready+(each C3)+C4=>E2/E3;
    // W3 C5=>E4. Real Docker probe additionally tests CurrentAttemptOnly proof,
    // authenticated Control retirement, new Run admission and physical cleanup.
    for residency in [
        awaken_session_environment_contract::ContainerHandResidency::AttachedExec,
        awaken_session_environment_contract::ContainerHandResidency::Resident,
    ] {
        let fake = Arc::new(FakeContainerProvider::default());
        let provider = SandboxEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
            fake.clone(),
            None,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "awaken-sandbox",
            std::time::Duration::ZERO,
            residency,
            awaken_time::system_clock(),
        )
        .into_session_provider();
        let thread = "session-container";
        let roots = crate::host::tests::ManagedSessionRootFixture::new().await;
        let host = roots
            .configure(
                crate::HostDependencies::ephemeral(
                    Arc::new(crate::no_model::NoModelConfiguredExecutor),
                    "stub",
                )
                .await,
            )
            .build()
            .expect("validated Host dependencies");
        let managed = managed_test_host(host.clone());
        let activation = test_activation(thread, "rebind-run");
        let mut projection = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &activation.snapshot,
        );
        projection.agent_publication = Some(activation.snapshot.clone());
        roots
            .install_projection(&managed, thread, projection.clone())
            .await
            .unwrap();
        let spec = host
            .realizer
            .staging
            .sandbox_spec_for_provider(thread, &provider);
        // W0: establish actual Root admission/binding before the revocation
        // table. The selected fake provider does not invent durable authority:
        // use the production sink's first CAS and generated receipt readback.
        let intent = host.realizer.new_environment_effect_intent(
            thread,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
            None,
        );
        let effect = host
            .realizer
            .authorize_environment_effect_before_io(thread, &intent)
            .await
            .unwrap();
        // W0a causes: exact/foreign operation fence and Create/Rebuild intent.
        // Exact Create is the only supported fake effect below. Foreign fence
        // rejects before allocation; Rebuild stays Unsupported, never inventing
        // original participant recovery. Both negative rules have zero creates.
        let fence = effect.provider_fence.as_ref().unwrap();
        let authority = effect.provider_authority.as_deref().unwrap();
        let foreign = effect
            .authorization
            .intent()
            .realization()
            .unwrap()
            .sandbox_effect_fence("foreign-fake-create")
            .unwrap();
        let denied =
            awaken_container_contract::ContainerEnvironmentProvider::create_environment_for_effect(
                fake.as_ref(),
                &spec,
                &foreign,
                authority,
                awaken_container_contract::EnvironmentRealizationIntent::Create,
            )
            .await;
        assert!(
            matches!(
                denied,
                Err(awaken_provisioning_contract::SandboxError::FenceRejected(_))
            ),
            "W0a foreign operation"
        );
        let denied =
            awaken_container_contract::ContainerEnvironmentProvider::create_environment_for_effect(
                fake.as_ref(),
                &spec,
                fence,
                authority,
                awaken_container_contract::EnvironmentRealizationIntent::Rebuild {
                    source: Box::new(
                        awaken_session_environment::test_support::current_fake_container_handle(
                            &spec,
                        ),
                    ),
                },
            )
            .await;
        assert!(
            matches!(
                denied,
                Err(awaken_provisioning_contract::SandboxError::Unsupported(_))
            ),
            "W0a no fake recovery proof"
        );
        assert_eq!(
            fake.creates.load(Ordering::SeqCst),
            0,
            "W0a zero allocation"
        );
        let original = Arc::new(
            provider
                .realizer()
                .create_effective_for_effect(
                    &spec,
                    crate::session_environment::ProviderEffect::from_parts(
                        effect.provider_fence.as_ref(),
                        effect.provider_authority.as_deref(),
                    )
                    .unwrap(),
                    None,
                    awaken_session_environment_contract::EnvironmentRealizationKind::Create,
                )
                .await
                .unwrap(),
        );
        let handle = original.lifecycle().handle();
        let binding = serde_json::to_string(&handle).unwrap();
        let identity = host
            .realizer
            .persist_authorized_environment_binding(thread, binding.clone(), &effect)
            .await
            .unwrap_or_else(|failure| panic!("W0 actual binding CAS: {}", failure.error));
        let candidate = host
            .realizer
            .begin_session_environment_preparation(
                thread,
                original.clone(),
                Some(effect.authorization.intent()),
            )
            .unwrap();
        host.realizer
            .publish_prepared_session_environment(thread, &candidate, identity)
            .unwrap();
        let SandboxEnvironment::Container {
            sandbox: physical, ..
        } = original.sandbox_environment()
        else {
            unreachable!()
        };
        let mut previous = original.clone();
        for turn in 0..2 {
            let call = ToolCall {
                call_id: format!("before-revoke-{turn}"),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            };
            previous
                .agent_tools()
                .tool_executor()
                .invoke(&call)
                .await
                .unwrap();
            {
                let lifecycle = host
                    .installed_sessions
                    .realizations()
                    .read_realization(thread, |r| r.environment.lifecycle.clone())
                    .unwrap();
                let _lifecycle = lifecycle.lock().await;
                host.retire_session_environment_for_revocation(thread)
                    .await
                    .unwrap();
            }
            assert!(
                previous
                    .agent_tools()
                    .tool_executor()
                    .invoke(&call)
                    .await
                    .is_err(),
                "W2 old Hand stays closed"
            );
            // W1/W2 new admitted work reinstalls the aggregate-owned Resource
            // transition through the production projection port. Publication
            // must execute its ordinary reconciliation, never a test-only ACK.
            let advanced = roots.advance_assignment(&managed, thread).await.unwrap();
            assert_eq!(
                advanced
                    .environment_materialization
                    .as_deref()
                    .unwrap()
                    .origin(),
                Some(effect.authorization.intent()),
                "W2 next generation preserves the first Create"
            );
            fake.shared.lock().unwrap().remove("__ready_observation");
            assert!(
                host.adopt_bound_session_environment(
                    thread,
                    Some(&binding),
                    &provider,
                    None,
                    false
                )
                .await
                .is_err(),
                "W1"
            );
            let retained = host
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment.owner.terminal_bound_environment())
                .flatten()
                .unwrap();
            assert!(
                Arc::ptr_eq(&retained.environment, &previous),
                "W1 exact owner retained"
            );
            fake.shared
                .lock()
                .unwrap()
                .insert("__ready_observation".into(), Vec::new());
            host.adopt_bound_session_environment(thread, Some(&binding), &provider, None, false)
                .await
                .unwrap();
            let rebound = host
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment.owner.terminal_bound_environment())
                .flatten()
                .unwrap()
                .environment;
            assert!(!Arc::ptr_eq(&previous, &rebound), "W2 fresh wrapper");
            assert!(
                !Arc::ptr_eq(
                    &previous.agent_tools().tool_executor(),
                    &rebound.agent_tools().tool_executor()
                ),
                "W2 fresh Hand"
            );
            let SandboxEnvironment::Container { sandbox, .. } = rebound.sandbox_environment()
            else {
                unreachable!()
            };
            assert!(Arc::ptr_eq(physical, sandbox), "W2 same physical owner");
            assert_eq!(
                rebound.lifecycle().handle(),
                handle,
                "W2 exact durable binding"
            );
            assert_eq!(fake.creates.load(Ordering::SeqCst), 1, "W2 no recreate");
            assert_eq!(
                fake.adopts.load(Ordering::SeqCst),
                0,
                "W2 no reconstructed participants"
            );
            assert_eq!(fake.disposals.load(Ordering::SeqCst), 0, "W2 no disposal");
            rebound
                .agent_tools()
                .tool_executor()
                .invoke(&call)
                .await
                .expect("W2 fresh Hand is usable");
            previous = rebound;
        }
        let mut foreign = handle.clone();
        foreign.sandbox_id.push_str("-foreign");
        assert!(
            provider
                .adoption()
                .rebind_retained(
                    &spec,
                    &foreign,
                    &previous,
                    crate::session_environment::ProviderEffect::Unattached
                )
                .await
                .is_err(),
            "W3 foreign handle"
        );
        let directory = tempfile::tempdir().unwrap();
        assert!(
            SandboxEnvironmentProvider::workdir(directory.path())
                .into_session_provider()
                .adoption()
                .rebind_retained(
                    &spec,
                    &handle,
                    &previous,
                    crate::session_environment::ProviderEffect::Unattached
                )
                .await
                .is_err(),
            "W3 foreign provider"
        );
        previous.lifecycle().stop_bound_processes().await.unwrap();
    }
}
