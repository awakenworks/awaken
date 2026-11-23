use super::*;
use awaken_agent_contract::credential::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage, ModelExposurePolicy, PlaintextHolder,
};
use awaken_lease_contract::fence::Epoch;
use awaken_session_configuration_contract::mcp::{
    McpAttachmentDraft, McpAttachmentOrigin, McpRealizationClaim, SessionMcpAttachmentSet,
};

fn draft(name: &str, revision: u64, holder: &PlaintextHolder) -> McpAttachmentDraft {
    McpAttachmentDraft {
        name: name.into(),
        target: awaken_agent_contract::McpTarget::parse_http(format!(
            "https://mcp.example.test/{name}"
        ))
        .unwrap(),
        prompts_as_skills: false,
        credential: Some(CredentialAccess::new(
            CredentialRef {
                id: format!("source-{name}"),
                revision,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
        )),
        origin: McpAttachmentOrigin::Session,
    }
}

fn claim(name: &str, version: u64, epoch: Epoch) -> McpRealizationClaim {
    McpRealizationClaim {
        realization_id: format!("realize-{name}-{version}-{epoch}"),
        runtime_incarnation: "worker/boot".into(),
        lease_epoch: epoch.get(),
        lease_expires_at_unix_ms: u64::MAX,
        stage_idempotency_key: format!("stage-{name}-{version}-{epoch}"),
    }
}

#[test]
fn lease_recovery_stages_current_heads_without_reopening_superseded_credentials() {
    // Cause graph: C1 an Active publication loses its acknowledgement during
    // lease recovery; C2 the same logical name has a newer desired credential;
    // C3 an independent Active name still needs recovery; C4 replacement activates.
    // Rules S1 C1+!C2 -> restage its current credential; S2 C1+C2 -> stage only
    // the successor, never reopen the retired credential (which may be invalid);
    // S3 every row+C3 -> retain the independent head; S4 C4 -> old generation
    // becomes drain work, current remains publication work. Stage selection is
    // read-only. This fixes the failed Flow snapshot's Active-v1/unacked plus
    // Realizing-v2 state without a wall-clock delay or fake successful adoption.
    for replacement in [false, true] {
        let mut session = crate::tests::persisted("mcp-renewal", true, "idle");
        let holder = session
            .frozen_baseline()
            .unwrap()
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        session.mcp = SessionMcpAttachmentSet::from_initial(
            vec![draft("flow", 1, &holder), draft("independent", 1, &holder)],
            Some(holder.clone()),
        )
        .unwrap();
        let initial = session
            .mcp
            .attachments
            .iter()
            .map(|a| (a.name.clone(), a.generation.clone()))
            .collect::<Vec<_>>();
        for (name, generation) in &initial {
            let owner = claim(name, 1, Epoch::FIRST);
            session
                .mcp
                .claim_realization(generation, owner.clone())
                .unwrap();
            session
                .mcp
                .activate(generation, &owner.realization_id)
                .unwrap();
            session
                .mcp
                .acknowledge_publication(generation, &owner.realization_id)
                .unwrap();
        }
        let successor = if replacement {
            let plan = session
                .mcp
                .request_full_replacement(
                    vec![draft("flow", 2, &holder), draft("independent", 1, &holder)],
                    Some(holder),
                )
                .unwrap();
            assert_eq!(plan.requested.len(), 1);
            let generation = plan.requested[0].clone();
            session
                .mcp
                .claim_realization(&generation, claim("flow", 2, Epoch::FIRST.next().unwrap()))
                .unwrap();
            Some(generation)
        } else {
            None
        };
        for (name, generation) in &initial {
            session
                .mcp
                .claim_recovery(generation, claim(name, 1, Epoch::FIRST.next().unwrap()))
                .unwrap();
        }
        let before = session.mcp.clone();
        let requests =
            SessionRealizations::realization_stage_requests("workspace", &session).unwrap();
        assert_eq!(session.mcp, before, "selection is read-only");
        let selected = requests
            .iter()
            .map(|request| {
                (
                    request.name.as_str(),
                    request.credential.as_ref().unwrap().credential.revision,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            selected,
            vec![
                ("flow", if replacement { 2 } else { 1 }),
                ("independent", 1)
            ],
            "S1-S3 no retired credential in preparation"
        );
        assert_eq!(
            session.mcp.attachments[0]
                .credential
                .as_ref()
                .unwrap()
                .credential
                .revision,
            1,
            "old claim is retained for drain"
        );
        if let Some(successor) = successor {
            let owner = claim("flow", 2, Epoch::FIRST.next().unwrap());
            session
                .mcp
                .activate(&successor, &owner.realization_id)
                .unwrap();
            let drains =
                draining_generations(&session.session_id, &session.mcp.attachments).unwrap();
            assert_eq!(drains.len(), 1, "S4 old physical generation still drains");
            let publish =
                publication_generations(&session.session_id, &session.mcp.attachments).unwrap();
            assert_eq!(publish.len(), 2, "S4 current and independent heads publish");
        }
    }
}
