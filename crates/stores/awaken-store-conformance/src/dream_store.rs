//! Shared Dream process and atomic policy claim conformance.

use awaken_session_contract::{DreamPolicyRecord, DreamProcessRecord, DreamProcessStore};

pub fn dream_policy(next_due_ms: u64) -> DreamPolicyRecord {
    DreamPolicyRecord {
        workspace_id: "workspace".into(),
        memory_store_id: "store".into(),
        config: awaken_session_contract::DreamPolicyConfig::default(),
        next_due_ms,
        last_completed_cutoff_ms: 0,
    }
}

pub fn process(process_id: &str) -> DreamProcessRecord {
    DreamProcessRecord {
        process_id: process_id.into(),
        workspace_id: "workspace".into(),
        status: awaken_session_contract::DreamStatus::Pending,
        source_memory_store_id: "store".into(),
        output_behavior: awaken_session_contract::DreamOutputBehavior::CreateNew,
        session_ids: vec!["session".into()],
        model: awaken_session_contract::DreamModelConfig {
            id: "claude-sonnet-5".into(),
            speed: None,
        },
        request_guidance: None,
        agent_id: "agent".into(),
        result_memory_store_id: None,
        session_id: None,
        transcript_file_ids: Vec::new(),
        cleanup_pending: false,
        created_at: 1,
        ended_at: None,
        archived_at: None,
        error: None,
        policy_key: None,
    }
}

pub fn dream_store_conformance(repository: &dyn DreamProcessStore) {
    // Persistence decision table: R1 absent job/policy + insert -> accepted;
    // R2 stale expected typed value -> rejected without mutation; R3 exact policy
    // version + fresh job -> both commit; R4 stale policy + fresh job ->
    // neither commits. These rules are the multi-replica Dream claim fence.
    let policy = dream_policy(1);
    assert!(
        repository
            .compare_and_swap_dream_policy(None, policy.clone())
            .unwrap()
    );
    assert!(
        !repository
            .compare_and_swap_dream_policy(
                Some(&dream_policy(0)),
                DreamPolicyRecord {
                    next_due_ms: 999,
                    ..policy.clone()
                },
            )
            .unwrap()
    );
    let policy_v2 = DreamPolicyRecord {
        next_due_ms: 2,
        ..policy.clone()
    };
    assert!(
        repository
            .claim_dream_policy(&policy, policy_v2.clone(), process("dream-1"),)
            .unwrap()
    );
    assert!(
        !repository
            .claim_dream_policy(
                &policy,
                DreamPolicyRecord {
                    next_due_ms: 3,
                    ..policy.clone()
                },
                process("dream-2"),
            )
            .unwrap()
    );
    assert_eq!(repository.dream_processes().unwrap().len(), 1);
    assert_eq!(repository.dream_policies().unwrap()[0], policy_v2);

    // R5 independent process insert/exact CAS succeeds; a stale expected
    // process cannot overwrite the successor. R6 a colliding process id
    // rejects a policy claim atomically: neither table changes.
    let original = process("standalone");
    assert!(
        repository
            .compare_and_swap_dream_process(None, original.clone())
            .unwrap()
    );
    assert!(
        !repository
            .compare_and_swap_dream_process(None, original.clone())
            .unwrap()
    );
    let mut running = original.clone();
    running.status = awaken_session_contract::DreamStatus::Running;
    assert!(
        repository
            .compare_and_swap_dream_process(Some(&original), running.clone())
            .unwrap()
    );
    assert!(
        !repository
            .compare_and_swap_dream_process(Some(&original), original.clone())
            .unwrap()
    );
    let before_jobs = repository.dream_processes().unwrap();
    let before_policies = repository.dream_policies().unwrap();
    let mut next_policy = policy_v2.clone();
    next_policy.next_due_ms += 1;
    assert!(
        !repository
            .claim_dream_policy(&policy_v2, next_policy, original)
            .unwrap()
    );
    assert_eq!(repository.dream_processes().unwrap(), before_jobs);
    assert_eq!(repository.dream_policies().unwrap(), before_policies);
}
