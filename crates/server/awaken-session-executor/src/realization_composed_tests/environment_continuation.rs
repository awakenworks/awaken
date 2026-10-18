//! Original Host composition tests over canonical realization code.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[tokio::test]
    async fn continuation_fence_keeps_control_coordinates_but_requires_current_local_proof() {
        use crate::realization_state::SessionRealizationAuthority as Authority;
        use awaken_session_configuration_contract::resource::SessionRealizationLease;
        use awaken_session_realization_contract::SessionRealizationLeaseReceipt;
        // F1 exact live receipt with Control expiry older than local wall time
        // -> return the unchanged Control fence. F2 missing root, F3 expired
        // observation, F4 closed root, F5 replaced incarnation -> reject before
        // lowering a fence. These are adapter/call-path checks of the shared
        // slot rule; provider-side expiry semantics are intentionally not mocked
        // into a claim of end-to-end clock-skew compatibility.
        let lease = SessionRealizationLease {
            owner: "control".into(),
            runtime_incarnation: "boot".into(),
            epoch: 1,
            expires_at_unix_ms: 30_000,
        };
        for (rule, admitted) in [
            ("F1", true),
            ("F2", false),
            ("F3", false),
            ("F4", false),
            ("F5", false),
        ] {
            let host = (crate::SessionExecutorBuilder::ephemeral(
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                "stub",
            )
            .await)
                .build()
                .expect("validated Host dependencies");
            let mut current = lease.clone();
            if rule == "F5" {
                current.runtime_incarnation.push_str("-new");
            }
            let authority = match rule {
                "F2" => Authority::Uninstalled,
                "F3" => Authority::Live(
                    awaken_session_realization::test_support::test_expired_observed_lease(current),
                ),
                "F4" => Authority::Closed(Some(current)),
                _ => Authority::Live(
                    SessionRealizationLeaseReceipt::issued_at(current, 0)
                        .unwrap()
                        .observed(awaken_authority_clock::authority_proof_started().unwrap()),
                ),
            };
            host.run_assembly
                .installed_sessions
                .install_realization(rule, |r| r.execution_mut().authority = authority);
            let generation = awaken_session_state_contract::SandboxGeneration::new(
                &awaken_session_types::SessionId::from(rule),
                1,
                30_000,
                "env",
                "image",
            );
            let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
                "workspace",
                &awaken_session_types::SessionId::from(rule),
                "suspend",
                &generation,
                1,
                Some(lease.clone()),
                None,
            );
            let fence = host
                .realization_executor
                .continuation_effect_fence(rule, &operation);
            assert_eq!(fence.is_ok(), admitted, "{rule}");
            if let Ok(fence) = fence {
                assert_eq!(
                    fence,
                    lease.sandbox_effect_fence(&operation.effect_id).unwrap(),
                    "{rule}/no rewritten expiry or identity"
                );
            }
        }
    }

    #[tokio::test]
    async fn continuation_preparation_retains_aggregate_fence_and_uses_injected_process_clock() {
        use crate::realization_state::SessionRealizationAuthority as Authority;
        use awaken_session_configuration_contract::resource::SessionRealizationLease;

        // P1 a current preparation lowers its persisted aggregate fence.
        // P2 same-generation local renewal extends ordinary effects, but cannot
        // rewrite that persisted preparation. P3 the exact injected process
        // clock expires preparation even while the renewed local proof is live.
        let thread = "continuation-preparation-clock";
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let mut dependencies = crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await;
        dependencies.clock = clock.clone();
        let host = dependencies.build().expect("validated Host dependencies");
        let lease = SessionRealizationLease {
            owner: "control".into(),
            runtime_incarnation: "boot".into(),
            epoch: 1,
            expires_at_unix_ms: 30_000,
        };
        let install = |lease: SessionRealizationLease| {
            host.run_assembly
                .installed_sessions
                .install_realization(thread, |r| {
                    r.execution_mut().authority = Authority::Live(
                    awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                        lease, 0,
                    )
                    .unwrap()
                    .observed(awaken_authority_clock::authority_proof_started().unwrap()),
                );
                });
        };
        install(lease.clone());
        let generation = awaken_session_state_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from(thread),
            1,
            60_000,
            "env",
            "image",
        );
        let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
            "workspace",
            &awaken_session_types::SessionId::from(thread),
            "suspend",
            &generation,
            1,
            Some(lease.clone()),
            None,
        );
        let preparation: awaken_session_state_contract::SourceReleasePreparationEffect =
            serde_json::from_value(serde_json::json!({
                "operation": operation,
                "lease": lease,
            }))
            .unwrap();
        let persisted = preparation.sandbox_effect_fence().unwrap();
        assert_eq!(
            host.realization_executor
                .continuation_preparation_effect_fence(thread, &preparation)
                .unwrap(),
            persisted,
            "P1 exact aggregate-owned preparation"
        );
        let mut renewed = lease.clone();
        renewed.expires_at_unix_ms += 30_000;
        install(renewed.clone());
        assert_eq!(
            host.realization_executor
                .continuation_effect_fence(thread, &operation)
                .unwrap(),
            renewed.sandbox_effect_fence(&operation.effect_id).unwrap(),
            "P2 ordinary operation observes same-generation renewal"
        );
        assert_eq!(
            host.realization_executor
                .continuation_preparation_effect_fence(thread, &preparation)
                .unwrap(),
            persisted,
            "P2 preparation never substitutes the renewed fence"
        );
        clock.set(lease.expires_at_unix_ms);
        assert!(
            host.realization_executor
                .continuation_preparation_effect_fence(thread, &preparation)
                .is_err(),
            "P3 preparation must use the Host's injected clock"
        );
        assert!(
            host.realization_executor
                .continuation_effect_fence(thread, &operation)
                .is_ok(),
            "P3 local renewed authority remains live independently"
        );
    }
}
