//! Real aggregate transitions and publication collections, without assumed uniqueness.
use super::*;
use awaken_session_contract::{
    McpAttachmentDraft, McpAttachmentOrigin, McpRealizationClaim, McpTarget,
    SessionMcpAttachmentSet,
};

fn draft(target: &str) -> McpAttachmentDraft {
    McpAttachmentDraft {
        name: "a".into(),
        target: McpTarget::parse_http(target).expect("fixed valid HTTP target"),
        prompts_as_skills: false,
        origin: McpAttachmentOrigin::Session,
        credential: None,
    }
}

fn claim(id: &str) -> McpRealizationClaim {
    McpRealizationClaim {
        realization_id: id.into(),
        runtime_incarnation: "a".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: 2,
        stage_idempotency_key: id.into(),
    }
}

fn assert_unique_generations(set: &SessionMcpAttachmentSet) {
    for (index, item) in set.attachments.iter().enumerate() {
        for other in &set.attachments[index + 1..] {
            assert!(
                item.generation != other.generation,
                "aggregate transitions preserve unique generation identities"
            );
        }
    }
}

fn check_replacement(renewed: bool, reverse: bool, removed: bool, acknowledged: bool, fault: u8) {
    // Cause/effect graph: C1 old Active and replacement Requested are generated
    // by from_initial/request_full_replacement; C2 replacement activation retires
    // old authority; C3 renewed expiry, Stage order, desired removal and ACK vary;
    // C4 the live receipt is exact/missing/duplicate/future/wrong identity.
    // R1 every actual mutation preserves unique generation identities (asserted,
    // never assumed); R2 only unacknowledged current generations publish; R3 all
    // retired generations drain and no published identity drains; R4 invalid live
    // coverage rejects without mutating the aggregate; R5 receipts retain their
    // original expiry through renewal. Unneeded Stage receipts never invent work.
    let mut set = SessionMcpAttachmentSet::from_initial(vec![draft("http://a")], None).unwrap();
    assert_unique_generations(&set);
    let old_generation = set.attachments[0].generation.clone();
    set.claim_realization(&old_generation, claim("a")).unwrap();
    set.activate(&old_generation, "a").unwrap();
    assert_unique_generations(&set);
    set.request_full_replacement(vec![draft("http://b")], None)
        .unwrap();
    assert_unique_generations(&set);
    assert_eq!(set.attachments.len(), 2);
    let new_generation = set.attachments[1].generation.clone();
    assert_ne!(old_generation, new_generation);
    set.claim_realization(&new_generation, claim("b")).unwrap();
    let old_receipt = projection::mcp_generation_ref("a", &set.attachments[0]).unwrap();
    let new_receipt = projection::mcp_generation_ref("a", &set.attachments[1]).unwrap();
    set.activate(&new_generation, "b").unwrap();
    set.begin_obsolete_drains().unwrap();
    assert_unique_generations(&set);
    assert_eq!(set.attachments[0].state, McpAttachmentState::Draining);
    assert_eq!(set.attachments[1].state, McpAttachmentState::Active);
    if renewed {
        set.extend_active_realization_leases("a", 1, 3).unwrap();
    }
    if acknowledged {
        set.acknowledge_publication(&new_generation, "b").unwrap();
    }
    if removed {
        set.request_full_replacement(Vec::new(), None).unwrap();
        set.begin_obsolete_drains().unwrap();
    }
    assert_unique_generations(&set);
    let mut actual_new = new_receipt.clone();
    match fault {
        0..=2 => {}
        3 => actual_new.lease_expires_at_unix_ms = 4,
        4 => actual_new.session_id = "b".into(),
        5 => actual_new.generation = old_generation.clone(),
        6 => actual_new.runtime_incarnation = "b".into(),
        7 => actual_new.lease_epoch = 2,
        8 => {
            actual_new.generation = awaken_session_contract::McpGeneration::new(
                awaken_session_contract::McpAttachmentId("b".into()),
                actual_new.generation.version(),
                actual_new.generation.digest().clone(),
            );
        }
        _ => unreachable!("closed fault table"),
    }
    let receipts = match fault {
        1 => vec![old_receipt.clone()],
        2 => vec![old_receipt.clone(), actual_new.clone(), actual_new],
        _ if reverse => vec![actual_new, old_receipt.clone()],
        _ => vec![old_receipt.clone(), actual_new],
    };
    let before = set.clone();
    let result = publication_action("a", &set.attachments, receipts);
    assert_eq!(
        set, before,
        "publication planning has no aggregate mutation"
    );
    let needs_publication = !removed && !acknowledged;
    if needs_publication && fault != 0 {
        assert!(
            matches!(result, Err(SessionRealizationControlFailure::Invalid(_))),
            "inexact live receipt coverage fails closed"
        );
        return;
    }
    let SessionRealizationAction::Publish { publish, drain } = result.unwrap() else {
        panic!("publication plan must preserve its action variant");
    };
    assert_eq!(publish.len(), usize::from(needs_publication));
    if needs_publication {
        assert_eq!(
            publish[0], new_receipt,
            "only the exact staged active receipt publishes"
        );
        assert_eq!(publish[0].lease_expires_at_unix_ms, 2);
    }
    assert_eq!(drain.len(), if removed { 2 } else { 1 });
    assert_eq!(
        drain[0], old_receipt,
        "the retired staged generation belongs to drain"
    );
    if removed {
        let mut expected = new_receipt;
        if renewed {
            expected.lease_expires_at_unix_ms = 3;
        }
        assert_eq!(drain[1], expected);
    }
    for published in &publish {
        for drained in &drain {
            assert!(
                published.generation != drained.generation,
                "publication and drain generation identities are disjoint"
            );
        }
    }
}

#[cfg(test)]
#[test]
fn publication_plan_crosses_real_replacement_renewal_ack_removal_and_receipt_faults() {
    // R1-R5: 2^4 x 9 = 144 rows, all constructed and transitioned through the
    // production aggregate. Faults cross receipt cardinality and every identity
    // axis; both Stage orders and old/new expiry ownership are checked.
    for renewed in [false, true] {
        for reverse in [false, true] {
            for removed in [false, true] {
                for acknowledged in [false, true] {
                    for fault in 0..9 {
                        check_replacement(renewed, reverse, removed, acknowledged, fault);
                    }
                }
            }
        }
    }
}
