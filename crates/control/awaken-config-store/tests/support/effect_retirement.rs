// @formal evidence resource_binding.draft_audit_and_effect_are_atomic
// @formal evidence resource_binding.pending_effect_survives_until_readback
// @formal evidence resource_binding.replay_is_idempotent_and_identity_stable
//! Shared durable-journal schedule; application readback wiring has Host tests.
use awaken_agent_config::{
    AgentConfig, AgentInputConfig, AuditedConfigWrite, ManagementAuditRecord, ManagementEffect,
    ScopeId, ScopedConfigRegistry,
};

pub async fn stale_completion_preserves_replacement(
    store: &dyn ScopedConfigRegistry,
    config: &AgentConfig,
) {
    // ABA decision table: E1 A/B observe effect revision 1; A applies/retires;
    // E2 a different audited write queues revision 2 under the same logical key;
    // E3 B completes revision 1 late. E3 MUST leave revision 2 durable. No retry
    // or process-local mutex may stand in for the DB's atomic retirement fence.
    let scope = ScopeId::from("effect-retirement");
    let old = effect(&config.id, 1);
    enqueue(store, &scope, config, 0, &old, "first").await;
    let observed = store
        .pending_management_effects_scoped(&scope)
        .await
        .unwrap();
    assert_eq!(observed, vec![old.clone()]);
    let ManagementEffect::UpsertAgentInputs { config: applied } = &old;
    let receipt = old.confirm_readback(&scope, Some(applied)).unwrap();
    // E0 changed content at the SAME input revision must not acknowledge old
    // intent; scope is also part of the receipt, not an independent DELETE arg.
    let other_scope = ScopeId::from("effect-retirement-other");
    enqueue(store, &other_scope, config, 0, &old, "other-scope").await;
    let mut changed = applied.clone();
    changed.environment = Some(awaken_agent_config::AgentEnvironmentBinding {
        environment_id: "different".into(),
        revision: 1,
    });
    let changed_effect = ManagementEffect::UpsertAgentInputs {
        config: changed.clone(),
    };
    let changed_receipt = changed_effect
        .confirm_readback(&scope, Some(&changed))
        .unwrap();
    assert!(
        !store
            .complete_management_effect_scoped(&changed_receipt)
            .await
            .unwrap(),
        "E0 full content"
    );
    assert!(
        store
            .complete_management_effect_scoped(&receipt)
            .await
            .unwrap()
    );
    let next = effect(&config.id, 2);
    assert!(
        !store
            .complete_management_effect_scoped(&receipt)
            .await
            .unwrap(),
        "E4 absent replay"
    );
    enqueue(store, &scope, config, 1, &next, "next").await;
    assert!(
        !store
            .complete_management_effect_scoped(&receipt)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .pending_management_effects_scoped(&scope)
            .await
            .unwrap(),
        vec![next.clone()],
        "E3 stale completion must preserve replacement"
    );
    assert_eq!(
        store
            .pending_management_effects_scoped(&other_scope)
            .await
            .unwrap(),
        vec![old],
        "E5 scope isolation"
    );
    let ManagementEffect::UpsertAgentInputs {
        config: next_applied,
    } = &next;
    let next_receipt = next.confirm_readback(&scope, Some(next_applied)).unwrap();
    assert!(
        store
            .complete_management_effect_scoped(&next_receipt)
            .await
            .unwrap(),
        "E6 current receipt"
    );
    assert!(
        store
            .pending_management_effects_scoped(&scope)
            .await
            .unwrap()
            .is_empty()
    );
    enqueue_decision_table(store, config).await;
}

async fn enqueue_decision_table(store: &dyn ScopedConfigRegistry, config: &AgentConfig) {
    // Q1 absent+positive => insert; Q2 exact pending => reuse without replacing;
    // Q3 changed content/revision or nonpositive input => reject the WHOLE
    // draft+effect+audit transaction. Remaining pending history stays byte-logically
    // intact, and failed admission must not increment the config revision.
    for (index, (pending, revision, changed, admitted)) in [
        (false, 1, false, true),
        (false, 0, false, false),
        (true, 1, false, true),
        (true, 1, true, false),
        (true, 2, false, false),
        (true, -1, false, false),
    ]
    .into_iter()
    .enumerate()
    {
        let scope = ScopeId::from(format!("effect-enqueue-{index}"));
        let old = effect(&config.id, 1);
        enqueue(store, &scope, config, 0, &old, "initial").await;
        if !pending {
            let ManagementEffect::UpsertAgentInputs { config: observed } = &old;
            let receipt = old.confirm_readback(&scope, Some(observed)).unwrap();
            assert!(
                store
                    .complete_management_effect_scoped(&receipt)
                    .await
                    .unwrap()
            );
        }
        let mut proposed = effect(&config.id, revision);
        if changed {
            let ManagementEffect::UpsertAgentInputs { config } = &mut proposed;
            config.environment = Some(awaken_agent_config::AgentEnvironmentBinding {
                environment_id: "changed".into(),
                revision: 1,
            });
        }
        let audit = ManagementAuditRecord {
            tool: "admin_patch_agent".into(),
            call_id: "candidate".into(),
            summary: "candidate".into(),
        };
        store
            .record_management_audit_scoped(&scope, &audit)
            .await
            .unwrap();
        let mut candidate = config.clone();
        candidate.instructions.push_str(" candidate");
        let result = store
            .put_config_with_audit_effect_scoped(&scope, &candidate, 1, &audit, Some(&proposed))
            .await;
        assert_eq!(result.is_ok(), admitted, "Q{index}");
        let current = store
            .get_config_revision_scoped(&scope, &config.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, if admitted { 2 } else { 1 });
        assert_eq!(
            current.config,
            if admitted { candidate } else { config.clone() }
        );
        assert_eq!(
            store
                .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
                .await
                .unwrap()
                .unwrap()
                .business_committed,
            admitted
        );
        let expected = if pending {
            vec![old]
        } else if admitted {
            vec![proposed]
        } else {
            vec![]
        };
        assert_eq!(
            store
                .pending_management_effects_scoped(&scope)
                .await
                .unwrap(),
            expected
        );
    }
}

fn effect(agent: &str, revision: i64) -> ManagementEffect {
    ManagementEffect::UpsertAgentInputs {
        config: AgentInputConfig {
            agent_id: agent.into(),
            environment: None,
            inputs: Vec::new(),
            revision,
        },
    }
}

async fn enqueue(
    store: &dyn ScopedConfigRegistry,
    scope: &ScopeId,
    config: &AgentConfig,
    revision: u64,
    effect: &ManagementEffect,
    call: &str,
) {
    let audit = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: call.into(),
        summary: format!("request={call}"),
    };
    store
        .record_management_audit_scoped(scope, &audit)
        .await
        .unwrap();
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(scope, config, revision, &audit, Some(effect))
            .await
            .unwrap(),
        AuditedConfigWrite::Applied
    );
}
